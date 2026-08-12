//! `run_tool_as_spine`: spawn the `__run-tool` hand subprocess for one Impulse.

use std::path::Path;

use prost::Message as _;

use crate::config::SandboxConfig;
use crate::proto::gait::sandbox::v1::{
    network_scope::Mode, DenyPolicy as ProtoDenyPolicy, Feedback, Impulse, NetworkScope,
    OutputCaptureMode as ProtoOutputCaptureMode, Scope,
};
use crate::sandbox::{NetworkPolicy, SandboxScope};
use crate::types::OutputCaptureMode;

use super::error_feedback;

fn shell_access_and_whitelist_hit(
    cmd: &str,
    sandbox_cfg: &SandboxConfig,
) -> (
    crate::types::ToolAccess,
    Option<Vec<crate::proto::gait::log::v1::SandboxWhitelistMatchItem>>,
    bool,
) {
    use crate::sandbox_grant::SandboxGrant;

    let mut access = crate::sandbox::ceiling::access_for_tool("shell", sandbox_cfg);
    let Some((entry, grants)) =
        crate::permissions::matching::whitelist_match(cmd, &sandbox_cfg.shell_whitelist)
    else {
        return (access, None, false);
    };
    let mut items = Vec::with_capacity(grants.len());
    for grant in grants {
        let dimension = match grant {
            SandboxGrant::NetworkHost => {
                access.network = true;
                "network"
            }
            SandboxGrant::Writable => {
                access.writes = true;
                "write"
            }
            SandboxGrant::Readable => {
                access.reads = true;
                "read"
            }
        };
        items.push(crate::proto::gait::log::v1::SandboxWhitelistMatchItem {
            tool_name: "shell".into(),
            resource: cmd.into(),
            dimension: dimension.into(),
            pattern: entry.pattern.clone(),
        });
    }
    // "bind" isn't in `SandboxGrant` — that enum is specifically the CLI-approval
    // persistence vocabulary (see its doc comment), and there is no interactive
    // approval flow for local-bind (a socket-bind denial isn't a pre-flight
    // "permission required" gate like a declared write path or network host is;
    // it just fails inside the sandboxed process). So it's read directly off the
    // whitelist entry's raw grant strings instead of routing through that enum.
    if entry.grants.iter().any(|g| g == "bind") {
        access.local_bind = true;
        items.push(crate::proto::gait::log::v1::SandboxWhitelistMatchItem {
            tool_name: "shell".into(),
            resource: cmd.into(),
            dimension: "bind".into(),
            pattern: entry.pattern.clone(),
        });
    }
    let sandbox_free = entry.grants.iter().any(|g| g == "unsandbox");
    if sandbox_free {
        items.push(crate::proto::gait::log::v1::SandboxWhitelistMatchItem {
            tool_name: "shell".into(),
            resource: cmd.into(),
            dimension: "unsandbox".into(),
            pattern: entry.pattern.clone(),
        });
    }
    (access, Some(items), sandbox_free)
}

/// Add Gait-owned, current-session runtime paths to a hand's bounded scope.
/// These are invariants rather than user-configured grants: using the dynamic
/// session id here keeps sibling sessions isolated without asking users to
/// grant the shared `.active/` or `daemons/` roots.
fn grant_session_runtime_paths(
    scope: &mut SandboxScope,
    gait_dir: &Path,
    session_id: &str,
    tool: &str,
) -> std::io::Result<std::path::PathBuf> {
    let worktree = crate::session::live::worktree_dir(gait_dir, session_id);
    if !worktree.exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!(
                "active session worktree does not exist: {}",
                worktree.display()
            ),
        ));
    }
    let canonical_worktree = worktree.canonicalize()?;
    let outputs = crate::session::live::outputs_dir(gait_dir, session_id);
    std::fs::create_dir_all(&outputs)?;
    let outputs = outputs.canonicalize()?;
    if !outputs.starts_with(&canonical_worktree) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "session outputs escape active worktree: {}",
                outputs.display()
            ),
        ));
    }
    // Every hand may inspect complete outputs from earlier calls in this session.
    // Write access is capability-driven and added separately for one spool
    // invocation directory by `grant_output_capture_target`.
    scope.readable.push(outputs.clone());

    // Daemon logs predate session outputs and remain in their existing
    // location. Keep the same current-session-only access rule.
    let session_daemons_dir = gait_dir.join("daemons").join(session_id);
    scope.readable.push(session_daemons_dir.clone());
    if tool == "shell" {
        scope.writable.push(session_daemons_dir);
    }

    Ok(outputs)
}

fn grant_output_capture_target(
    scope: &mut SandboxScope,
    mode: OutputCaptureMode,
    output_path: &Path,
    session_outputs: &Path,
) -> std::io::Result<()> {
    if mode == OutputCaptureMode::Returned {
        return Ok(());
    }
    let parent = output_path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("tool output path has no parent: {}", output_path.display()),
        )
    })?;
    let parent = parent.canonicalize()?;
    if !parent.starts_with(session_outputs) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "tool output target escapes session outputs: {}",
                parent.display()
            ),
        ));
    }
    scope.readable.push(parent.clone());
    scope.writable.push(parent);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub async fn run_tool_as_spine(
    impulse: Impulse,
    sandbox_cfg: &SandboxConfig,
    gait_dir: &Path,
    codebase: &Path,
    session_id: &str,
    sandbox_on: bool,
    exe: &Path,
    group: std::sync::Arc<dyn crate::process::Containment>,
    fs_delegate: Option<std::sync::Arc<dyn crate::embed::FsDelegate>>,
) -> Feedback {
    use tokio::io::AsyncWriteExt;

    let diag = crate::turn::DiagnosticSink::new(gait_dir, session_id);
    let mut sandbox_free = false;
    let output_capture_mode = match ProtoOutputCaptureMode::try_from(impulse.output_capture_mode) {
        Ok(ProtoOutputCaptureMode::Returned) => OutputCaptureMode::Returned,
        Ok(ProtoOutputCaptureMode::Spool) => OutputCaptureMode::Spool,
        Ok(ProtoOutputCaptureMode::Unspecified) | Err(_) => {
            return error_feedback("invalid or unspecified output capture mode")
        }
    };

    let jail_raw = std::env::temp_dir().join(format!(
        "gait-hand-{}",
        uuid::Uuid::new_v4().to_string().replace('-', "")
    ));
    if std::fs::create_dir_all(&jail_raw).is_err() {
        return error_feedback("failed to create temp jail");
    }
    let jail = jail_raw.canonicalize().unwrap_or_else(|_| jail_raw.clone());

    // For detached daemon shell calls, use the full agent ceiling (all capabilities
    // enabled, bounded only by the session sandbox config) rather than the
    // per-call tool scope. This lets the daemon process inherit the agent's
    // configured limits (e.g. network, codebase writes) rather than the narrower
    // default shell scope.
    let is_detached_shell = impulse.tool == "shell"
        && impulse
            .args
            .as_ref()
            .and_then(|s| s.fields.get("detached"))
            .and_then(|v| match &v.kind {
                Some(pbjson_types::value::Kind::BoolValue(b)) => Some(*b),
                _ => None,
            })
            .unwrap_or(false);
    let mut whitelist_hit = None;
    let scope_result = if is_detached_shell {
        use crate::types::ToolAccess;
        crate::sandbox::ceiling::compute_scope(
            ToolAccess {
                reads: true,
                writes: true,
                network: true,
                spawns: true,
                local_bind: true,
            },
            sandbox_cfg,
            codebase,
            &jail,
        )
    } else if impulse.tool == "shell" {
        let cmd = impulse
            .args
            .as_ref()
            .and_then(|s| s.fields.get("cmd"))
            .and_then(|v| match &v.kind {
                Some(pbjson_types::value::Kind::StringValue(s)) => Some(s.as_str()),
                _ => None,
            })
            .unwrap_or("");
        let (access, hit, matched_sandbox_free) = shell_access_and_whitelist_hit(cmd, sandbox_cfg);
        whitelist_hit = hit;
        sandbox_free = matched_sandbox_free;
        crate::sandbox::ceiling::compute_scope(access, sandbox_cfg, codebase, &jail)
    } else {
        crate::sandbox::ceiling::bound_request(&impulse.tool, sandbox_cfg, codebase, &jail)
    };
    let mut scope = match scope_result {
        Ok(scope) => scope,
        Err(error) => return error_feedback(&error.to_string()),
    };

    let session_outputs =
        match grant_session_runtime_paths(&mut scope, gait_dir, session_id, &impulse.tool) {
            Ok(path) => path,
            Err(error) => {
                let _ = std::fs::remove_dir_all(&jail);
                return error_feedback(&format!("failed to prepare session outputs: {error}"));
            }
        };

    // Reserve every hand one Gait-owned full-output path for the shared finalizer.
    // Only a tool declaring `Spool` receives this path or write access to its
    // parent; returned-value tools never see it.
    let output_path =
        match crate::tools::prepare_tool_output_path(gait_dir, session_id, &impulse.tool) {
            Ok(path) => path,
            Err(error) => {
                let _ = std::fs::remove_dir_all(&jail);
                return error_feedback(&format!("failed to prepare tool output: {error}"));
            }
        };
    if let Err(error) = grant_output_capture_target(
        &mut scope,
        output_capture_mode,
        &output_path,
        &session_outputs,
    ) {
        let _ = std::fs::remove_dir_all(&jail);
        return error_feedback(&format!("failed to grant tool output target: {error}"));
    }

    // Shell subprocesses may outlive the hand (detached mode), so their
    // inherited TMPDIR must share the active session's lifetime rather than
    // the per-hand jail's lifetime.
    let shell_process_dir = if impulse.tool == "shell" {
        let invocation_id = uuid::Uuid::new_v4().to_string().replace('-', "");
        let path = session_outputs.join("process").join(invocation_id);
        if let Err(error) = std::fs::create_dir_all(&path) {
            let _ = std::fs::remove_dir_all(&jail);
            return error_feedback(&format!("failed to prepare shell process output: {error}"));
        }
        scope.readable.push(path.clone());
        scope.writable.push(path.clone());
        Some(path)
    } else {
        None
    };

    let requested = scope_to_proto(&scope);

    // ── fs-delegation: scope-check the path, then pre-read the editor buffer ──
    // Delegation removes the kernel seal from this path, so the SPINE re-enforces the
    // same ceiling here (design §"Sandbox: the spine is the fs-scope authority").
    let delegated_path = fs_delegate
        .as_ref()
        .and_then(|_| delegated_target(&impulse.tool, &impulse.args, codebase));
    let prefetch = if let (Some(del), Some(path)) = (fs_delegate.as_ref(), delegated_path.as_ref())
    {
        // Enforce scope only when the sandbox is on (parity with the hand's seal,
        // skipped when seal=false). `path` is already normalized by delegated_target.
        // Mirrors check_own_tool_scope (src/turn/mod.rs:709-731).
        if sandbox_on {
            let is_write = impulse.tool != "read";
            let ok = if is_write {
                scope.writable.iter().any(|w| path.starts_with(w))
            } else {
                scope.read_all || scope.readable.iter().any(|r| path.starts_with(r))
            };
            if !ok {
                let _ = std::fs::remove_dir_all(&jail);
                return error_feedback(&format!(
                    "delegated {} out of scope: {}",
                    if is_write { "write" } else { "read" },
                    path.display()
                ));
            }
        }
        match del.read_text_file(path).await {
            Ok(content) => Some(gait_proto::gait::sandbox::v1::Prefetch {
                path: path.to_string_lossy().into_owned(),
                content, // Some(buffer) | None(new file)
            }),
            Err(e) => {
                let _ = std::fs::remove_dir_all(&jail); // FAIL-READ must not leak the jail
                return error_feedback(&format!("fs delegate read failed: {e}"));
            }
        }
    } else {
        None
    };

    let shell_whitelist_patterns = if impulse.tool == "shell" {
        sandbox_cfg
            .shell_whitelist
            .iter()
            .map(|e| e.pattern.clone())
            .collect()
    } else {
        vec![]
    };

    let impulse_with_scope = Impulse {
        tool: impulse.tool.clone(),
        args: impulse.args,
        requested: Some(requested),
        prefetch,
        shell_whitelist_patterns,
        output_capture_mode: impulse.output_capture_mode,
    };
    let bytes = impulse_with_scope.encode_to_vec();

    // debug_assert: the brain (group leader) must be alive before we enroll a hand.
    // pgid == 0 means spawn_enrolled hasn't been called yet for the brain — logic error.
    #[cfg(debug_assertions)]
    {
        let handle_str = group.handle();
        debug_assert!(
            handle_str.is_some(),
            "hand enrolled before brain: cascade group has no leader yet"
        );
    }
    let mut child = {
        use tokio::process::Command as TokioCommand;
        let mut std_cmd = std::process::Command::new(exe);
        std_cmd
            .arg("__run-tool")
            .arg("--gait-dir")
            .arg(gait_dir)
            .arg("--codebase")
            .arg(codebase)
            .arg("--session")
            .arg(session_id)
            .arg("--tool")
            .arg(&impulse.tool);
        if output_capture_mode == OutputCaptureMode::Spool {
            std_cmd.arg("--output-spool").arg(&output_path);
        }
        std_cmd
            .arg("--seal")
            .arg(if sandbox_on && !sandbox_free {
                "true"
            } else {
                "false"
            })
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .env_clear()
            .envs(std::env::vars().filter(|(k, _)| {
                matches!(
                    k.as_str(),
                    "PATH" | "HOME" | "USER" | "SHELL" | "TERM" | "LANG" | "LC_ALL" | "TMPDIR"
                )
            }))
            .env(
                "TMPDIR",
                shell_process_dir.as_deref().unwrap_or(jail.as_path()),
            );
        // Enroll hand into the cascade group (joins brain's process group).
        if let Err(e) = group.configure_enrolled(&mut std_cmd) {
            let _ = std::fs::remove_dir_all(&jail);
            return error_feedback(&format!("failed to configure hand enrollment: {e}"));
        }
        let mut tokio_cmd = TokioCommand::from(std_cmd);
        match tokio_cmd.spawn() {
            Ok(c) => {
                group.register_pid(c.id().unwrap_or(0) as i32);
                let rec = crate::diagnostic::DiagRecord::lifecycle(
                    session_id,
                    crate::diagnostic::Phase::AwaitFeedback,
                    crate::diagnostic::PhaseEvent::Start,
                    &format!(
                        "hand_spawned tool={} pid={} seal={} stdout=hand_response stderr=piped",
                        impulse.tool,
                        c.id().unwrap_or(0),
                        sandbox_on && !sandbox_free
                    ),
                );
                diag.debug(&rec.to_json_line());
                c
            }
            Err(e) => {
                let _ = std::fs::remove_dir_all(&jail);
                return error_feedback(&format!("failed to spawn __run-tool: {e}"));
            }
        }
    };

    {
        let mut stdin = match child.stdin.take() {
            Some(s) => s,
            None => {
                let _ = std::fs::remove_dir_all(&jail);
                return error_feedback("no stdin on __run-tool");
            }
        };
        if let Err(e) = stdin.write_all(&bytes).await {
            let _ = std::fs::remove_dir_all(&jail);
            return error_feedback(&format!("write impulse failed: {e}"));
        }
    }

    let out = match child.wait_with_output().await {
        Ok(o) => o,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&jail);
            return error_feedback(&format!("wait_with_output failed: {e}"));
        }
    };
    let exit_detail = format!(
        "hand_exited tool={} status={} stdout_bytes={} stderr_bytes={}",
        impulse.tool,
        out.status,
        out.stdout.len(),
        out.stderr.len()
    );
    let rec = crate::diagnostic::DiagRecord::lifecycle(
        session_id,
        crate::diagnostic::Phase::AwaitFeedback,
        if out.status.success() {
            crate::diagnostic::PhaseEvent::Ok
        } else {
            crate::diagnostic::PhaseEvent::Error
        },
        &exit_detail,
    );
    diag.debug(&rec.to_json_line());
    let _ = std::fs::remove_dir_all(&jail);

    // The hand always writes a HandResponse on stdout. Decode it regardless of
    // exit code; only fall back to exit-code/stderr if stdout is missing/garbled
    // (a true crash — the hand never got to write).
    use gait_proto::gait::sandbox::v1::{hand_response, HandResponse};
    let mut feedback = match HandResponse::decode(out.stdout.as_slice()) {
        Ok(HandResponse {
            outcome: Some(hand_response::Outcome::Feedback(f)),
        }) => super::output::finalize_feedback(
            f,
            gait_dir,
            session_id,
            &impulse.tool,
            Some(output_path),
        ),
        Ok(HandResponse {
            outcome: Some(hand_response::Outcome::Error(e)),
        }) => error_feedback(&format!("hand error: {}", e.message)),
        Ok(HandResponse { outcome: None }) => error_feedback("hand returned empty HandResponse"),
        Err(_) => error_feedback(&format!(
            "hand crashed (exit {}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        )),
    };

    // If the hand spawned a detached daemon, record it in daemons.jsonl.
    // The spine is the sole gait-dir writer; best-effort (ignore errors).
    if let Some(ref rec) = feedback.daemon {
        // Build the proto-backed registry record. `session` is the owning
        // parent session id (Task 6 — Review Correction [E]); it is used by
        // `__daemon-exit` to look up which parent to wake on completion.
        let dr = crate::proto::gait::log::v1::DaemonStart {
            name: rec.name.clone(),
            pid: rec.pid,
            pgid: rec.pgid,
            started_at: rec.started_at,
            cmd: rec.cmd.clone(),
            cwd: rec.cwd.clone(),
            log_path: rec.log_path.clone(),
            session: rec.session.clone(),
        };
        let _ = crate::session::daemons::append_start(gait_dir, &dr);
    }

    // Strip final_content FIRST, unconditionally, on its own line: it must never
    // travel up the Nerve to the brain. (A "guard-then-take" refactor would leak
    // editor content into the brain's log — keep this separate and explicit.)
    let final_content = feedback.final_content.take();
    if let (Some(del), Some(path)) = (fs_delegate.as_ref(), delegated_path.as_ref()) {
        if let Some(fc) = final_content {
            if let Err(e) = del.write_text_file(path, &fc).await {
                // jail was already removed after wait_with_output above; the write
                // happens post-hand, so there is nothing left to clean here.
                // fail-write: provisional success withheld.
                return error_feedback(&format!("fs delegate write failed: {e}"));
            }
        }
    }

    if let Some(items) = whitelist_hit {
        feedback.whitelist_match_items = items;
    }
    feedback
}

/// Convert `SandboxScope` to the proto `Scope` wire type.
pub fn scope_to_proto(s: &SandboxScope) -> Scope {
    Scope {
        read_all: s.read_all,
        readable: s
            .readable
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect(),
        writable: s
            .writable
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect(),
        network: Some(match &s.network {
            NetworkPolicy::Deny => NetworkScope {
                mode: Mode::Deny as i32,
                hosts: vec![],
            },
            NetworkPolicy::All => NetworkScope {
                mode: Mode::All as i32,
                hosts: vec![],
            },
            NetworkPolicy::Hosts(h) => NetworkScope {
                mode: Mode::Hosts as i32,
                hosts: h.clone(),
            },
        }),
        local_bind: s.local_bind,
        read_deny: Some(ProtoDenyPolicy {
            subpaths: s
                .read_deny
                .subpaths
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect(),
            globs: s.read_deny.globs.clone(),
        }),
        write_deny: Some(ProtoDenyPolicy {
            subpaths: s
                .write_deny
                .subpaths
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect(),
            globs: s.write_deny.globs.clone(),
        }),
    }
}

/// If `tool` is one of the five delegating fs tools and its args carry a
/// `file_path`, return the path resolved as the tools resolve it (absolute as-is,
/// else `codebase.join(file_path)`) and then **lexically normalized**. Otherwise
/// `None`. The normalized path is authoritative for delegated ops: it is what the
/// spine scope-checks, ships in `Prefetch.path`, and hands to the delegate.
pub fn delegated_target(
    tool: &str,
    args: &Option<pbjson_types::Struct>,
    codebase: &Path,
) -> Option<std::path::PathBuf> {
    matches!(tool, "read" | "write" | "apply_patch").then_some(())?;
    if tool == "apply_patch" {
        let body = args
            .as_ref()?
            .fields
            .get("patch")
            .and_then(|v| match &v.kind {
                Some(pbjson_types::value::Kind::StringValue(s)) => Some(s),
                _ => None,
            })?;
        let patch = crate::tools::apply_patch::parse(body).ok()?;
        return crate::tools::apply_patch::resolve_target(codebase, &patch).ok();
    }
    let fp = args
        .as_ref()?
        .fields
        .get("file_path")
        .and_then(|v| match &v.kind {
            Some(pbjson_types::value::Kind::StringValue(s)) => Some(s.clone()),
            _ => None,
        })?;
    let p = std::path::Path::new(&fp);
    if p.is_absolute() {
        Some(p.to_path_buf())
    } else {
        Some(crate::sandbox::normalize_lexical(&codebase.join(&fp)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn current_session_outputs_are_auto_granted_without_sibling_access() {
        let gait_dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(crate::session::live::worktree_dir(
            gait_dir.path(),
            "session-a",
        ))
        .unwrap();
        let mut shell_scope = SandboxScope::default();
        let own =
            grant_session_runtime_paths(&mut shell_scope, gait_dir.path(), "session-a", "shell")
                .unwrap();
        let sibling = crate::session::live::outputs_dir(gait_dir.path(), "session-b");

        assert!(own.is_dir(), "auto-granted output root must exist");
        assert!(shell_scope.readable.contains(&own));
        assert!(
            !shell_scope.writable.contains(&own),
            "the session output root is read-only even for shell"
        );
        assert!(!shell_scope.readable.contains(&sibling));
        assert!(!shell_scope.writable.contains(&sibling));

        let mut read_scope = SandboxScope::default();
        let own_for_read =
            grant_session_runtime_paths(&mut read_scope, gait_dir.path(), "session-a", "read")
                .unwrap();
        assert!(read_scope.readable.contains(&own_for_read));
        assert!(
            !read_scope.writable.contains(&own_for_read),
            "returned-value tools must not receive automatic output write access"
        );
    }

    #[test]
    fn only_spool_capture_grants_its_invocation_directory() {
        let gait_dir = tempfile::TempDir::new().unwrap();
        let session_id = "session-a";
        std::fs::create_dir_all(crate::session::live::worktree_dir(
            gait_dir.path(),
            session_id,
        ))
        .unwrap();

        let mut returned_scope = SandboxScope::default();
        let outputs =
            grant_session_runtime_paths(&mut returned_scope, gait_dir.path(), session_id, "read")
                .unwrap();
        let returned_path =
            crate::tools::prepare_tool_output_path(gait_dir.path(), session_id, "read").unwrap();
        grant_output_capture_target(
            &mut returned_scope,
            OutputCaptureMode::Returned,
            &returned_path,
            &outputs,
        )
        .unwrap();
        assert!(returned_scope.writable.is_empty());

        let mut spool_scope = SandboxScope::default();
        let outputs =
            grant_session_runtime_paths(&mut spool_scope, gait_dir.path(), session_id, "grep")
                .unwrap();
        let spool_path =
            crate::tools::prepare_tool_output_path(gait_dir.path(), session_id, "grep").unwrap();
        let spool_parent = spool_path.parent().unwrap().canonicalize().unwrap();
        grant_output_capture_target(
            &mut spool_scope,
            OutputCaptureMode::Spool,
            &spool_path,
            &outputs,
        )
        .unwrap();

        assert!(spool_scope.readable.contains(&spool_parent));
        assert!(spool_scope.writable.contains(&spool_parent));
        assert!(!spool_scope.writable.contains(&outputs));
    }

    #[test]
    fn output_grant_never_precreates_the_active_worktree() {
        let gait_dir = tempfile::TempDir::new().unwrap();
        let worktree = crate::session::live::worktree_dir(gait_dir.path(), "not-started");
        let error = grant_session_runtime_paths(
            &mut SandboxScope::default(),
            gait_dir.path(),
            "not-started",
            "shell",
        )
        .unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
        assert!(
            !worktree.exists(),
            "the spine must not steal session-worktree creation from startup"
        );
    }

    #[cfg(unix)]
    #[test]
    fn output_grant_rejects_a_symlink_escape() {
        let gait_dir = tempfile::TempDir::new().unwrap();
        let outside = tempfile::TempDir::new().unwrap();
        let worktree = crate::session::live::worktree_dir(gait_dir.path(), "session-a");
        std::fs::create_dir_all(&worktree).unwrap();
        std::os::unix::fs::symlink(
            outside.path(),
            crate::session::live::outputs_dir(gait_dir.path(), "session-a"),
        )
        .unwrap();

        let mut scope = SandboxScope::default();
        let error = grant_session_runtime_paths(&mut scope, gait_dir.path(), "session-a", "shell")
            .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(scope.readable.is_empty());
        assert!(scope.writable.is_empty());
    }

    #[test]
    fn scope_to_proto_round_trips_deny_policy() {
        use crate::sandbox::DenyPolicy;
        let scope = SandboxScope {
            read_deny: DenyPolicy {
                subpaths: vec![std::path::PathBuf::from("/home/user/.ssh")],
                globs: vec!["**/.env*".to_string()],
            },
            write_deny: DenyPolicy::default(),
            ..SandboxScope::default()
        };
        let proto = scope_to_proto(&scope);
        let read_deny = proto.read_deny.expect("read_deny must be populated");
        assert_eq!(read_deny.subpaths, vec!["/home/user/.ssh".to_string()]);
        assert_eq!(read_deny.globs, vec!["**/.env*".to_string()]);
    }

    #[test]
    fn shell_whitelist_bind_grant_sets_local_bind_and_logs_dimension() {
        use gait_types::config::{SandboxConfig, SandboxWhitelistEntry};

        let mut cfg = SandboxConfig::default();
        cfg.shell_whitelist.push(SandboxWhitelistEntry {
            pattern: "flutter test *".into(),
            grants: vec!["network".into(), "bind".into()],
        });

        let (access, items, sandbox_free) =
            shell_access_and_whitelist_hit("flutter test foo.dart", &cfg);
        assert!(access.network, "network grant must still apply");
        assert!(access.local_bind, "bind grant must set local_bind");
        assert!(
            !sandbox_free,
            "network/bind grants do not disable the sandbox"
        );
        let items = items.expect("whitelist matched");
        assert!(
            items.iter().any(|i| i.dimension == "bind"),
            "expected a bind dimension in whitelist match items: {items:?}"
        );
    }

    #[test]
    fn shell_whitelist_without_bind_grant_leaves_local_bind_false() {
        use gait_types::config::{SandboxConfig, SandboxWhitelistEntry};

        let mut cfg = SandboxConfig::default();
        cfg.shell_whitelist.push(SandboxWhitelistEntry {
            pattern: "flutter test *".into(),
            grants: vec!["network".into()],
        });

        let (access, _items, sandbox_free) =
            shell_access_and_whitelist_hit("flutter test foo.dart", &cfg);
        assert!(
            !access.local_bind,
            "no bind grant means local_bind stays false"
        );
        assert!(!sandbox_free);
    }

    #[test]
    fn shell_whitelist_unsandbox_grant_only_matches_the_command_pattern() {
        use gait_types::config::{SandboxConfig, SandboxWhitelistEntry};

        let mut cfg = SandboxConfig::default();
        cfg.shell_whitelist.push(SandboxWhitelistEntry {
            pattern: "flutter test *".into(),
            grants: vec!["unsandbox".into()],
        });

        let (_, items, matched) = shell_access_and_whitelist_hit("flutter test foo.dart", &cfg);
        assert!(matched);
        assert!(items
            .unwrap()
            .iter()
            .any(|item| item.dimension == "unsandbox"));

        let (_, _, unmatched) = shell_access_and_whitelist_hit("dart test foo.dart", &cfg);
        assert!(!unmatched);
    }

    #[test]
    fn delegated_target_resolves_only_fs_tools() {
        let mut s = pbjson_types::Struct::default();
        s.fields.insert(
            "file_path".into(),
            pbjson_types::Value {
                kind: Some(pbjson_types::value::Kind::StringValue("src/a.rs".into())),
            },
        );
        let cb = Path::new("/ws");
        assert_eq!(
            delegated_target("read", &Some(s.clone()), cb),
            Some(Path::new("/ws/src/a.rs").to_path_buf())
        );
        assert_eq!(delegated_target("shell", &Some(s.clone()), cb), None); // not a delegating tool

        let mut abs = pbjson_types::Struct::default();
        abs.fields.insert(
            "file_path".into(),
            pbjson_types::Value {
                kind: Some(pbjson_types::value::Kind::StringValue("/etc/x".into())),
            },
        );
        assert_eq!(
            delegated_target("write", &Some(abs), cb),
            Some(Path::new("/etc/x").to_path_buf())
        );

        // `..` is lexically normalized so a later scope check can't be fooled by a path
        // that literally starts_with the codebase but escapes it.
        let mut esc = pbjson_types::Struct::default();
        esc.fields.insert(
            "file_path".into(),
            pbjson_types::Value {
                kind: Some(pbjson_types::value::Kind::StringValue(
                    "../../etc/passwd".into(),
                )),
            },
        );
        assert_eq!(
            delegated_target("write", &Some(esc), cb),
            Some(Path::new("/etc/passwd").to_path_buf())
        );
    }

    #[tokio::test]
    async fn delegated_write_out_of_scope_is_rejected() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let called = Arc::new(AtomicBool::new(false));
        struct Fake(Arc<AtomicBool>);
        #[async_trait::async_trait]
        impl crate::embed::FsDelegate for Fake {
            async fn read_text_file(&self, _p: &Path) -> anyhow::Result<Option<String>> {
                Ok(None)
            }
            async fn write_text_file(&self, _p: &Path, _c: &str) -> anyhow::Result<()> {
                self.0.store(true, Ordering::SeqCst);
                Ok(())
            }
        }

        let ws = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(crate::session::live::worktree_dir(
            ws.path(),
            "sess-escape-01",
        ))
        .unwrap();
        let mut s = pbjson_types::Struct::default();
        s.fields.insert(
            "file_path".into(),
            pbjson_types::Value {
                kind: Some(pbjson_types::value::Kind::StringValue(
                    "/etc/gait-escape-probe".into(),
                )),
            },
        );
        let impulse = gait_proto::gait::sandbox::v1::Impulse {
            tool: "write".into(),
            args: Some(s),
            requested: None,
            prefetch: None,
            shell_whitelist_patterns: vec![],
            output_capture_mode: ProtoOutputCaptureMode::Returned as i32,
        };
        let group = crate::process::establish(None);
        let feedback = run_tool_as_spine(
            impulse,
            &crate::config::SandboxConfig::default(),
            ws.path(),
            ws.path(),
            "sess-escape-01",
            true,                          // sandbox ON → spine enforces scope
            Path::new("/nonexistent-exe"), // never reached: scope check returns first
            group,
            Some(Arc::new(Fake(called.clone())) as Arc<dyn crate::embed::FsDelegate>),
        )
        .await;

        let rendered = serde_json::to_string(&feedback.result).unwrap();
        assert!(
            rendered.to_lowercase().contains("out of scope"),
            "expected scope rejection: {rendered}"
        );
        assert!(
            !called.load(Ordering::SeqCst),
            "delegate write must NOT run for an out-of-scope path"
        );
    }
}

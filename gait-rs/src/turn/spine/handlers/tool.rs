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

/// Why a hand's identity deny list could not be built (fail closed).
#[derive(Debug, coded_error::CodedError)]
#[coded(namespace = "gait.hand")]
pub(crate) enum HandDenyError {
    #[coded(code = "deny_path_has_colon", class = Logical)]
    ColonPath { path: std::path::PathBuf },
    #[coded(code = "machine_dir_unresolved", class = Logical)]
    MachineDirUnresolved(anyhow::Error),
}

impl std::fmt::Display for HandDenyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for HandDenyError {}

impl HandDenyError {
    /// The text the model sees for the refusal.
    fn message(&self) -> String {
        match self {
            Self::ColonPath { path } => format!(
                "identity path {} contains ':' and cannot be carried in the hand's deny list; move it",
                path.display()
            ),
            Self::MachineDirUnresolved(source) => format!("{source:#}"),
        }
    }
}

/// The `GAIT_IDENTITY_DENY` value handed to a spawned tool hand: the agent's
/// identity folders plus, when identity is in use, the machine identity
/// directory. "Identity in use" means `GAIT_IDENTITY_DIR` is set and non-empty, or
/// `<gait_dir>/identity` or `identity-root` exists (or cannot be inspected). Fails closed only when identity is in
/// use — an unresolvable or `:`-containing machine directory is then an error;
/// otherwise the machine entry is simply skipped.
fn hand_identity_deny_env(
    gait_dir: &Path,
    var: impl Fn(&str) -> Option<std::ffi::OsString>,
) -> Result<String, HandDenyError> {
    let identity_dir = match var("GAIT_IDENTITY_DIR") {
        Some(dir) if !dir.is_empty() => std::path::PathBuf::from(dir),
        _ => gait_dir.join("identity"),
    };
    // An unreadable path counts as present: fail closed.
    let present = |p: std::path::PathBuf| p.try_exists().unwrap_or(true);
    let identity_in_use = var("GAIT_IDENTITY_DIR").is_some_and(|d| !d.is_empty())
        || present(gait_dir.join("identity"))
        || present(gait_dir.join("identity-root"));
    let machine = match gait_identity::machine_dir_with(&var) {
        Ok(machine) => Some(machine),
        Err(e) if identity_in_use => return Err(HandDenyError::MachineDirUnresolved(e)),
        Err(_) => None,
    };
    let mut paths = gait_identity::agent_deny_paths(&identity_dir);
    paths.extend(machine);
    // A ':' path cannot ride the list: refuse when identity is in use or the
    // path exists; a standalone gait skips an entry that is not there.
    let mut kept = Vec::with_capacity(paths.len());
    for p in paths {
        if p.to_string_lossy().contains(':') {
            if identity_in_use || present(p.clone()) {
                return Err(HandDenyError::ColonPath { path: p });
            }
            continue;
        }
        kept.push(p);
    }
    Ok(kept
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(":"))
}

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

fn detached_shell_scope(
    sandbox_cfg: &SandboxConfig,
    codebase: &Path,
    jail: &Path,
) -> Result<crate::sandbox::SandboxScope, crate::sandbox::ceiling::ScopePathError> {
    use crate::types::ToolAccess;
    // For detached process shell calls, use the full agent ceiling (all capabilities
    // enabled, bounded only by the session sandbox config) rather than the
    // per-call tool scope. This lets the process inherit the agent's
    // configured limits (e.g. network, codebase writes) rather than the narrower
    // default shell scope. The full agent ceiling is inherited EXCEPT that
    // `sandbox.tools.shell` overrides set to false narrow it.
    crate::sandbox::ceiling::compute_scope(
        narrow_by_shell_override(
            ToolAccess {
                reads: true,
                writes: true,
                network: true,
                spawns: true,
                local_bind: true,
            },
            sandbox_cfg,
        ),
        sandbox_cfg,
        codebase,
        jail,
    )
}

/// Narrow the full agent ToolAccess by the `sandbox.tools.shell` override:
/// each field set to `Some(false)` clears the corresponding capability
/// (narrowing only); `Some(true)` or `None` leave it unchanged. `spawns` has
/// no override field and is always preserved.
fn narrow_by_shell_override(
    full: crate::types::ToolAccess,
    sandbox_cfg: &SandboxConfig,
) -> crate::types::ToolAccess {
    let override_cfg = sandbox_cfg.tools.get("shell");
    let is_off = |field: Option<bool>| field == Some(false);
    crate::types::ToolAccess {
        reads: full.reads && !override_cfg.map(|o| is_off(o.reads)).unwrap_or(false),
        writes: full.writes && !override_cfg.map(|o| is_off(o.writes)).unwrap_or(false),
        network: full.network && !override_cfg.map(|o| is_off(o.network)).unwrap_or(false),
        spawns: full.spawns,
        local_bind: full.local_bind && !override_cfg.map(|o| is_off(o.local_bind)).unwrap_or(false),
    }
}

/// Add Gait-owned, current-session runtime paths to a hand's bounded scope.
/// These are invariants rather than user-configured grants: using the dynamic
/// session id here keeps sibling sessions isolated without asking users to
/// grant the shared `.active/` or `processes/` roots.
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

    // Process logs predate session outputs and remain in their existing
    // location. Keep the same current-session-only access rule.
    let session_processes_dir = gait_dir.join("processes").join(session_id);
    scope.readable.push(session_processes_dir.clone());
    if tool == "shell" {
        scope.writable.push(session_processes_dir);
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

/// How a hand process ended, for the error text of a hand that never answered.
fn hand_exit_detail(out: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stderr = stderr.trim();
    let stderr: String = stderr.chars().take(600).collect();
    format!("exited {} with stderr: {stderr}", out.status)
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
    // Unlike output_capture_mode, an absent/unspecified presentation is not an
    // error -- it's the ordinary case for every caller that never set it
    // (the outer ToolCall path, and every existing test), and defaults safely
    // to today's human-facing preview.
    use gait_proto::gait::sandbox::v1::OutputPresentation as ProtoOutputPresentation;
    let output_presentation = match ProtoOutputPresentation::try_from(impulse.output_presentation) {
        Ok(ProtoOutputPresentation::CodemodeFull) => crate::types::OutputPresentation::CodemodeFull,
        Ok(ProtoOutputPresentation::ModelPreview)
        | Ok(ProtoOutputPresentation::Unspecified)
        | Err(_) => crate::types::OutputPresentation::ModelPreview,
    };

    // Built before the jail exists: a refusal leaves nothing to clean up.
    let deny_env_value = match hand_identity_deny_env(gait_dir, |k| std::env::var_os(k)) {
        Ok(value) => value,
        Err(refusal) => return error_feedback(&refusal.message()),
    };

    let jail_raw = std::env::temp_dir().join(format!(
        "gait-hand-{}",
        uuid::Uuid::new_v4().to_string().replace('-', "")
    ));
    if std::fs::create_dir_all(&jail_raw).is_err() {
        return error_feedback("failed to create temp jail");
    }
    let jail = jail_raw.canonicalize().unwrap_or_else(|_| jail_raw.clone());

    // For detached process shell calls, use the full agent ceiling (all capabilities
    // enabled, bounded only by the session sandbox config) rather than the
    // per-call tool scope. This lets the process process inherit the agent's
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
        detached_shell_scope(sandbox_cfg, codebase, &jail)
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
                // Lazily consult the session log for once/session unlock grants
                // before refusing; a read error means no grants (fail closed).
                let events = crate::session::log::read_events(&crate::session::log::log_path(
                    gait_dir, session_id,
                ))
                .unwrap_or_default();
                let unlocked = crate::session::retrace::active_unlock_state(&events).grants(path);
                if !unlocked {
                    let _ = std::fs::remove_dir_all(&jail);
                    return error_feedback(&format!(
                        "delegated {} out of scope: {}",
                        if is_write { "write" } else { "read" },
                        path.display()
                    ));
                }
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
        // The hand never finalizes output — it just runs the tool and returns
        // the raw Feedback. Presentation only matters afterward, spine-side.
        output_presentation: impulse.output_presentation,
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
            .envs(std::env::vars().filter(|(k, _)| crate::env_passes_to_children(k)))
            .env(
                "TMPDIR",
                shell_process_dir.as_deref().unwrap_or(jail.as_path()),
            );
        std_cmd.env(gait_identity::DENY_PATHS_ENV, deny_env_value);
        crate::launch::with_trace_env(&mut std_cmd);
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
            // The hand is gone before it read its input: its exit status and
            // stderr are the only account of why.
            drop(stdin);
            let died = child.wait_with_output().await.map_or_else(
                |_| "exit status unavailable".to_owned(),
                |o| hand_exit_detail(&o),
            );
            let _ = std::fs::remove_dir_all(&jail);
            return error_feedback(&format!("write impulse failed: {e}; hand {died}"));
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
            output_presentation,
        ),
        Ok(HandResponse {
            outcome: Some(hand_response::Outcome::Error(e)),
        }) => error_feedback(&format!("hand error: {}", e.message)),
        Ok(HandResponse { outcome: None }) => error_feedback(&format!(
            "hand returned empty HandResponse; hand {}",
            hand_exit_detail(&out)
        )),
        Err(_) => error_feedback(&format!(
            "hand crashed (exit {}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        )),
    };

    // If the hand spawned a detached process, record it in processes.jsonl.
    // The spine is the sole gait-dir writer; best-effort (ignore errors).
    if let Some(ref rec) = feedback.process {
        // Build the proto-backed registry record. `session` is the owning
        // parent session id (Task 6 — Review Correction [E]); it is used by
        // `__process-exit` to look up which parent to wake on completion.
        let dr = crate::proto::gait::log::v1::ProcessStart {
            name: rec.name.clone(),
            pid: rec.pid,
            pgid: rec.pgid,
            started_at: rec.started_at,
            cmd: rec.cmd.clone(),
            cwd: rec.cwd.clone(),
            log_path: rec.log_path.clone(),
            session: rec.session.clone(),
            ..Default::default()
        };
        let dr = crate::trace::with_start_carrier(dr);
        let _ = crate::session::processes::append_start(gait_dir, &dr);
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
/// `file_path`, return the path resolved as the tools resolve it — via the
/// shared [`crate::sandbox::clean_target`] (absolute replaces the codebase root,
/// else joined onto it), then **lexically normalized**. Otherwise
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
    Some(crate::sandbox::clean_target(codebase, p))
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
    fn detached_shell_scope_guard_network_override_is_currently_ignored() {
        use gait_types::config::{NetworkConfig, ToolAccessOverride};

        let codebase = tempfile::TempDir::new().unwrap();
        let jail = tempfile::TempDir::new().unwrap();
        let mut cfg = gait_types::config::SandboxConfig {
            network: NetworkConfig::All,
            ..Default::default()
        };
        cfg.tools.insert(
            "shell".into(),
            ToolAccessOverride {
                network: Some(false),
                ..Default::default()
            },
        );

        let detached = detached_shell_scope(&cfg, codebase.path(), jail.path()).unwrap();
        assert!(
            !matches!(detached.network, NetworkPolicy::All),
            "detached shell must not ignore the per-tool network override"
        );
    }

    #[test]
    fn detached_shell_scope_guard_writes_override_is_currently_ignored() {
        use gait_types::config::{NetworkConfig, ToolAccessOverride};

        let codebase = tempfile::TempDir::new().unwrap();
        let jail = tempfile::TempDir::new().unwrap();
        let mut cfg = gait_types::config::SandboxConfig {
            network: NetworkConfig::All,
            ..Default::default()
        };
        cfg.tools.insert(
            "shell".into(),
            ToolAccessOverride {
                writes: Some(false),
                ..Default::default()
            },
        );

        let detached = detached_shell_scope(&cfg, codebase.path(), jail.path()).unwrap();
        assert!(
            !detached
                .writable
                .contains(&codebase.path().canonicalize().unwrap()),
            "detached shell must not ignore the per-tool writes override"
        );
    }

    #[test]
    fn detached_shell_scope_control_without_overrides_keeps_full_ceiling() {
        use gait_types::config::NetworkConfig;

        let codebase = tempfile::TempDir::new().unwrap();
        let jail = tempfile::TempDir::new().unwrap();
        let cfg = gait_types::config::SandboxConfig {
            network: NetworkConfig::All,
            ..Default::default()
        };

        let detached = detached_shell_scope(&cfg, codebase.path(), jail.path()).unwrap();
        assert!(
            matches!(detached.network, NetworkPolicy::All),
            "with no override, detached shell keeps network All"
        );
        assert!(
            detached
                .writable
                .contains(&codebase.path().canonicalize().unwrap()),
            "with no override, detached shell keeps the codebase writable"
        );
    }

    #[test]
    fn foreground_shell_scope_reflects_the_network_override() {
        use gait_types::config::{NetworkConfig, ToolAccessOverride};

        let codebase = tempfile::TempDir::new().unwrap();
        let jail = tempfile::TempDir::new().unwrap();
        let mut cfg = gait_types::config::SandboxConfig {
            network: NetworkConfig::All,
            ..Default::default()
        };
        cfg.tools.insert(
            "shell".into(),
            ToolAccessOverride {
                network: Some(false),
                ..Default::default()
            },
        );

        let access = crate::sandbox::ceiling::access_for_tool("shell", &cfg);
        let foreground =
            crate::sandbox::ceiling::compute_scope(access, &cfg, codebase.path(), jail.path())
                .unwrap();
        assert!(
            !matches!(foreground.network, NetworkPolicy::All),
            "the foreground shell scope must honor the network override"
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
            output_presentation: gait_proto::gait::sandbox::v1::OutputPresentation::Unspecified
                as i32,
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

    #[tokio::test]
    async fn delegated_read_honors_session_unlock_outside_codebase() {
        use gait_proto::gait::log::v1 as pb;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        struct Fake(Arc<AtomicBool>);
        #[async_trait::async_trait]
        impl crate::embed::FsDelegate for Fake {
            async fn read_text_file(&self, _p: &Path) -> anyhow::Result<Option<String>> {
                self.0.store(true, Ordering::SeqCst);
                Ok(Some("hi".to_string()))
            }
            async fn write_text_file(&self, _p: &Path, _c: &str) -> anyhow::Result<()> {
                Ok(())
            }
        }

        let ws = tempfile::TempDir::new().unwrap();
        let session = "sess-unlock-01";
        std::fs::create_dir_all(crate::session::live::worktree_dir(ws.path(), session)).unwrap();

        let outside = tempfile::TempDir::new().unwrap();
        let granted = outside.path().join("granted.txt");
        let other = outside.path().join("other.txt");
        std::fs::write(&granted, "secret-ok").unwrap();
        std::fs::write(&other, "secret-no").unwrap();

        let log_path = crate::session::log::log_path(ws.path(), session);
        std::fs::create_dir_all(log_path.parent().unwrap()).unwrap();
        crate::proto::wire::append(
            &log_path,
            crate::proto::events::sandbox_unlock(&[pb::SandboxUnlockItem {
                id: "u1".to_string(),
                tool_name: "read".to_string(),
                resource: granted.to_string_lossy().into_owned(),
                dimension: "read".to_string(),
                preview: None,
            }]),
        )
        .unwrap();
        crate::session::log::write_unlock_reply(
            &log_path,
            "user",
            vec![pb::SandboxUnlockReplyItem {
                id: "u1".to_string(),
                decision: pb::Decision::Allow as i32,
                scope: pb::Scope::Session as i32,
            }],
        )
        .unwrap();

        async fn call(ws: &Path, session: &str, file_path: &str, flag: Arc<AtomicBool>) -> String {
            let mut s = pbjson_types::Struct::default();
            s.fields.insert(
                "file_path".into(),
                pbjson_types::Value {
                    kind: Some(pbjson_types::value::Kind::StringValue(file_path.into())),
                },
            );
            let impulse = gait_proto::gait::sandbox::v1::Impulse {
                tool: "read".into(),
                args: Some(s),
                requested: None,
                prefetch: None,
                shell_whitelist_patterns: vec![],
                output_capture_mode: ProtoOutputCaptureMode::Returned as i32,
                output_presentation: gait_proto::gait::sandbox::v1::OutputPresentation::Unspecified
                    as i32,
            };
            let group = crate::process::establish(None);
            let feedback = run_tool_as_spine(
                impulse,
                &crate::config::SandboxConfig::default(),
                ws,
                ws,
                session,
                true,
                Path::new("/nonexistent-exe"),
                group,
                Some(Arc::new(Fake(flag)) as Arc<dyn crate::embed::FsDelegate>),
            )
            .await;
            serde_json::to_string(&feedback.result).unwrap()
        }

        // Granted (session-scope unlock): must NOT be refused as out of scope.
        let called = Arc::new(AtomicBool::new(false));
        let rendered = call(
            ws.path(),
            session,
            &granted.to_string_lossy(),
            called.clone(),
        )
        .await;
        assert!(
            !rendered.to_lowercase().contains("out of scope"),
            "session-unlocked read must not be refused: {rendered}"
        );
        assert!(
            called.load(Ordering::SeqCst),
            "delegate read must run for the unlocked path: {rendered}"
        );

        // Not unlocked: refused, delegate untouched.
        let called2 = Arc::new(AtomicBool::new(false));
        let rendered = call(
            ws.path(),
            session,
            &other.to_string_lossy(),
            called2.clone(),
        )
        .await;
        assert!(
            rendered.to_lowercase().contains("out of scope"),
            "unlocked-scope check must refuse a path without a grant: {rendered}"
        );
        assert!(
            !called2.load(Ordering::SeqCst),
            "delegate read must NOT run for an unlocked-foreign path"
        );
    }

    #[tokio::test]
    async fn delegated_read_and_write_scope_check_survives_uncleaned_absolute_paths() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        struct Fake {
            read_called: Arc<AtomicBool>,
            write_called: Arc<AtomicBool>,
        }
        #[async_trait::async_trait]
        impl crate::embed::FsDelegate for Fake {
            async fn read_text_file(&self, _p: &Path) -> anyhow::Result<Option<String>> {
                self.read_called.store(true, Ordering::SeqCst);
                Ok(None)
            }
            async fn write_text_file(&self, _p: &Path, _c: &str) -> anyhow::Result<()> {
                self.write_called.store(true, Ordering::SeqCst);
                Ok(())
            }
        }

        async fn run_case(
            ws: &Path,
            tool: &str,
            file_path: &str,
            session: &str,
        ) -> (String, Arc<AtomicBool>, Arc<AtomicBool>) {
            let read_called = Arc::new(AtomicBool::new(false));
            let write_called = Arc::new(AtomicBool::new(false));
            let fake = Fake {
                read_called: read_called.clone(),
                write_called: write_called.clone(),
            };

            std::fs::create_dir_all(crate::session::live::worktree_dir(ws, session)).unwrap();
            let mut s = pbjson_types::Struct::default();
            s.fields.insert(
                "file_path".into(),
                pbjson_types::Value {
                    kind: Some(pbjson_types::value::Kind::StringValue(
                        file_path.to_string(),
                    )),
                },
            );
            let impulse = gait_proto::gait::sandbox::v1::Impulse {
                tool: tool.to_string(),
                args: Some(s),
                requested: None,
                prefetch: None,
                shell_whitelist_patterns: vec![],
                output_capture_mode: ProtoOutputCaptureMode::Returned as i32,
                output_presentation: gait_proto::gait::sandbox::v1::OutputPresentation::Unspecified
                    as i32,
            };
            let group = crate::process::establish(None);
            let feedback = run_tool_as_spine(
                impulse,
                &crate::config::SandboxConfig::default(),
                ws,
                ws,
                session,
                true, // sandbox ON → spine enforces scope
                Path::new("/nonexistent-exe"),
                group,
                Some(Arc::new(fake) as Arc<dyn crate::embed::FsDelegate>),
            )
            .await;
            let rendered = serde_json::to_string(&feedback.result).unwrap();
            (rendered, read_called, write_called)
        }

        for tool in ["read", "write"] {
            let ws = tempfile::TempDir::new().unwrap();
            let ws_str = format!("{}", ws.path().display());

            // Direct `..` escape: refused, delegate untouched.
            let direct = format!("{}/../x", ws_str);
            let (rendered, read_called, write_called) =
                run_case(ws.path(), tool, &direct, &format!("sess-{tool}-direct")).await;
            assert!(
                rendered.to_lowercase().contains("out of scope"),
                "[{tool} direct] expected scope rejection: {rendered}"
            );
            assert!(
                !read_called.load(Ordering::SeqCst),
                "[{tool} direct] read delegate must not run"
            );
            assert!(
                !write_called.load(Ordering::SeqCst),
                "[{tool} direct] write delegate must not run"
            );

            // Nested `..` escape: refused, delegate untouched.
            let nested = format!("{}/sub/../../x", ws_str);
            let (rendered, read_called, write_called) =
                run_case(ws.path(), tool, &nested, &format!("sess-{tool}-nested")).await;
            assert!(
                rendered.to_lowercase().contains("out of scope"),
                "[{tool} nested] expected scope rejection: {rendered}"
            );
            assert!(
                !read_called.load(Ordering::SeqCst),
                "[{tool} nested] read delegate must not run"
            );
            assert!(
                !write_called.load(Ordering::SeqCst),
                "[{tool} nested] write delegate must not run"
            );

            // In-scope path with an interior `..`: allowed; the read delegate runs.
            std::fs::create_dir_all(ws.path().join("sub")).unwrap();
            std::fs::write(ws.path().join("ok"), "hello").unwrap();
            let inside = format!("{}/sub/../ok", ws_str);
            let (rendered, read_called, write_called) =
                run_case(ws.path(), tool, &inside, &format!("sess-{tool}-inside")).await;
            assert!(
                !rendered.to_lowercase().contains("out of scope"),
                "[{tool} inside] must not be a scope rejection: {rendered}"
            );
            assert!(
                read_called.load(Ordering::SeqCst),
                "[{tool} inside] read delegate must have run: {rendered}"
            );
            let _ = write_called;
        }
    }

    fn env_of(
        pairs: &'static [(&'static str, &'static str)],
    ) -> impl Fn(&str) -> Option<std::ffi::OsString> {
        move |k| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| std::ffi::OsString::from(*v))
        }
    }

    #[test]
    fn hand_deny_env_covers_machine_dir_without_gait_identity_dir() {
        let value = hand_identity_deny_env(
            Path::new("/g"),
            env_of(&[("MACHINE_IDENTITY_DIR", "/m/mi"), ("HOME", "/h")]),
        )
        .unwrap();
        let parts: Vec<&str> = value.split(':').collect();
        assert_eq!(parts, vec!["/g/identity", "/g/identity-root", "/m/mi"]);
    }

    #[test]
    fn hand_deny_env_uses_the_default_machine_dir_and_the_handed_identity_dir() {
        let value = hand_identity_deny_env(
            Path::new("/g"),
            env_of(&[("GAIT_IDENTITY_DIR", "/agents/a"), ("HOME", "/h")]),
        )
        .unwrap();
        let parts: Vec<&str> = value.split(':').collect();
        assert_eq!(parts, vec!["/agents/a", "/h/.local/share/machine-identity"]);
    }

    #[test]
    fn hand_deny_env_with_nothing_set_and_no_identity_is_ok_without_a_machine_entry() {
        let gait = tempfile::TempDir::new().unwrap();
        let value = hand_identity_deny_env(gait.path(), env_of(&[])).unwrap();
        let g = gait.path();
        let parts: Vec<String> = value.split(':').map(str::to_owned).collect();
        assert_eq!(
            parts,
            vec![
                g.join("identity").to_string_lossy().into_owned(),
                g.join("identity-root").to_string_lossy().into_owned()
            ]
        );
    }

    #[test]
    fn hand_deny_env_skips_a_colon_machine_path_that_does_not_exist_when_identity_is_unused() {
        let gait = tempfile::TempDir::new().unwrap();
        let value = hand_identity_deny_env(
            gait.path(),
            env_of(&[
                ("MACHINE_IDENTITY_DIR", "/nonexistent/m/a:b"),
                ("HOME", "/h"),
            ]),
        )
        .unwrap();
        assert!(!value.contains("a:b"), "{value}");
    }

    #[test]
    fn hand_deny_env_refuses_an_existing_colon_machine_dir_even_when_identity_is_unused() {
        let gait = tempfile::TempDir::new().unwrap();
        let base = tempfile::TempDir::new().unwrap();
        let machine = base.path().join("a:b");
        std::fs::create_dir(&machine).unwrap();
        let var =
            move |k: &str| (k == "MACHINE_IDENTITY_DIR").then(|| machine.clone().into_os_string());
        let err = hand_identity_deny_env(gait.path(), var).unwrap_err();
        assert!(format!("{err:#}").contains("':'"), "{err:#}");
    }

    #[test]
    fn hand_deny_env_treats_identity_root_or_an_empty_identity_dir_var_correctly() {
        let gait = tempfile::TempDir::new().unwrap();
        // Empty GAIT_IDENTITY_DIR is not "in use": no identity dirs, no machine dir => Ok.
        assert!(hand_identity_deny_env(gait.path(), env_of(&[("GAIT_IDENTITY_DIR", "")])).is_ok());
        std::fs::create_dir(gait.path().join("identity-root")).unwrap();
        assert!(hand_identity_deny_env(gait.path(), env_of(&[])).is_err());
    }

    #[test]
    fn hand_deny_env_refuses_a_path_the_colon_list_cannot_carry() {
        let err = hand_identity_deny_env(
            Path::new("/g"),
            env_of(&[
                ("GAIT_IDENTITY_DIR", "/agents/a"),
                ("MACHINE_IDENTITY_DIR", "/m/a:b"),
                ("HOME", "/h"),
            ]),
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("':'"), "{err:#}");
    }

    #[test]
    fn hand_deny_env_fails_closed_when_an_identity_dir_exists_and_machine_dir_is_not_computable() {
        let gait = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(gait.path().join("identity")).unwrap();
        let err = hand_identity_deny_env(gait.path(), env_of(&[])).unwrap_err();
        assert!(
            format!("{err:#}").contains("machine identity directory"),
            "{err:#}"
        );
    }

    #[test]
    fn hand_deny_env_fails_closed_when_the_machine_dir_is_not_computable() {
        let err = hand_identity_deny_env(
            Path::new("/g"),
            env_of(&[("GAIT_IDENTITY_DIR", "/agents/a")]),
        )
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("machine identity directory"),
            "{err:#}"
        );
    }

    #[test]
    fn hand_deny_env_colon_path_failure_is_the_typed_variant() {
        let err: anyhow::Error = hand_identity_deny_env(
            Path::new("/g"),
            env_of(&[
                ("GAIT_IDENTITY_DIR", "/agents/a"),
                ("MACHINE_IDENTITY_DIR", "/m/a:b"),
                ("HOME", "/h"),
            ]),
        )
        .unwrap_err()
        .into();
        assert!(
            matches!(
                err.downcast_ref::<HandDenyError>(),
                Some(HandDenyError::ColonPath { path }) if path == Path::new("/m/a:b")
            ),
            "{err:#}"
        );
    }

    #[test]
    fn hand_deny_env_unresolved_machine_dir_is_the_typed_variant() {
        let err: anyhow::Error = hand_identity_deny_env(
            Path::new("/g"),
            env_of(&[("GAIT_IDENTITY_DIR", "/agents/a")]),
        )
        .unwrap_err()
        .into();
        assert!(
            matches!(
                err.downcast_ref::<HandDenyError>(),
                Some(HandDenyError::MachineDirUnresolved(_))
            ),
            "{err:#}"
        );
    }
}

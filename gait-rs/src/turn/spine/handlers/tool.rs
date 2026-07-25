//! `run_tool_as_spine`: spawn the `__run-tool` hand subprocess for one Impulse.

use std::path::Path;

use prost::Message as _;

use crate::config::SandboxConfig;
use crate::proto::gait::sandbox::v1::{
    network_scope::Mode, DenyPolicy as ProtoDenyPolicy, Feedback, Impulse, NetworkScope, Scope,
};
use crate::sandbox::{NetworkPolicy, SandboxScope};

use super::error_feedback;

fn shell_access_and_whitelist_hit(
    cmd: &str,
    sandbox_cfg: &SandboxConfig,
) -> (
    crate::types::ToolAccess,
    Option<Vec<crate::proto::gait::log::v1::SandboxWhitelistMatchItem>>,
) {
    use crate::sandbox_grant::SandboxGrant;

    let mut access = crate::sandbox::ceiling::access_for_tool("shell", sandbox_cfg);
    let Some((entry, grants)) =
        crate::permissions::matching::whitelist_match(cmd, &sandbox_cfg.shell_whitelist)
    else {
        return (access, None);
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
    (access, Some(items))
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
    let mut scope = if is_detached_shell {
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
        let (access, hit) = shell_access_and_whitelist_hit(cmd, sandbox_cfg);
        whitelist_hit = hit;
        crate::sandbox::ceiling::compute_scope(access, sandbox_cfg, codebase, &jail)
    } else {
        crate::sandbox::ceiling::bound_request(&impulse.tool, sandbox_cfg, codebase, &jail)
    };

    // Grant this session's own daemon-log directory, never any sibling
    // session's — auto-injected here (not config-driven) so no gait.jsonc
    // entry can accidentally widen it back to the shared `daemons/` root.
    // Readable for every tool (so read/grep/ls can inspect a session's own
    // daemon output); writable only for `shell` (which creates the dir and
    // the log file when spawning a detached daemon).
    let session_daemons_dir = gait_dir.join("daemons").join(session_id);
    scope.readable.push(session_daemons_dir.clone());
    if impulse.tool == "shell" {
        scope.writable.push(session_daemons_dir);
    }

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
            .arg(&impulse.tool)
            .arg("--seal")
            .arg(if sandbox_on { "true" } else { "false" })
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
            .env("TMPDIR", &jail);
        // Enroll hand into the cascade group (joins brain's process group).
        if let Err(e) = group.configure_enrolled(&mut std_cmd) {
            let _ = std::fs::remove_dir_all(&jail);
            return error_feedback(&format!("failed to configure hand enrollment: {e}"));
        }
        let mut tokio_cmd = TokioCommand::from(std_cmd);
        match tokio_cmd.spawn() {
            Ok(c) => {
                group.register_pid(c.id().unwrap_or(0) as i32);
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
    let _ = std::fs::remove_dir_all(&jail);

    // The hand always writes a HandResponse on stdout. Decode it regardless of
    // exit code; only fall back to exit-code/stderr if stdout is missing/garbled
    // (a true crash — the hand never got to write).
    use gait_proto::gait::sandbox::v1::{hand_response, HandResponse};
    let mut feedback = match HandResponse::decode(out.stdout.as_slice()) {
        Ok(HandResponse {
            outcome: Some(hand_response::Outcome::Feedback(f)),
        }) => f,
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

        let (access, items) = shell_access_and_whitelist_hit("flutter test foo.dart", &cfg);
        assert!(access.network, "network grant must still apply");
        assert!(access.local_bind, "bind grant must set local_bind");
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

        let (access, _items) = shell_access_and_whitelist_hit("flutter test foo.dart", &cfg);
        assert!(
            !access.local_bind,
            "no bind grant means local_bind stays false"
        );
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
        assert_eq!(delegated_target("edit_replace", &Some(s.clone()), cb), None);
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

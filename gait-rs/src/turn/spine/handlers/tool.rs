//! `run_tool_as_spine`: spawn the `__run-tool` hand subprocess for one Impulse.

use std::path::Path;

use prost::Message as _;

use crate::config::SandboxConfig;
use crate::proto::gait::sandbox::v1::{
    network_scope::Mode, Feedback, Impulse, NetworkScope, Scope,
};
use crate::sandbox::{NetworkPolicy, SandboxScope};

use super::error_feedback;

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
    let scope = if impulse.tool == "shell"
        && impulse
            .args
            .as_ref()
            .and_then(|s| s.fields.get("detached"))
            .and_then(|v| match &v.kind {
                Some(pbjson_types::value::Kind::BoolValue(b)) => Some(*b),
                _ => None,
            })
            .unwrap_or(false)
    {
        use crate::types::ToolAccess;
        crate::sandbox::ceiling::compute_scope(
            ToolAccess {
                reads: true,
                writes: true,
                network: true,
                spawns: true,
            },
            sandbox_cfg,
            codebase,
            &jail,
        )
    } else {
        crate::sandbox::ceiling::bound_request(&impulse.tool, sandbox_cfg, codebase, &jail)
    };
    let requested = scope_to_proto(&scope);

    // Resolve cwd: if the brain sent a cwd in the Impulse, use it; otherwise
    // read active_cwd from the session log (same as SubprocessSynapse did).
    // RelaySynapse sends empty cwd because the brain doesn't read the session
    // log for cwd — the spine owns it.
    let cwd = if impulse.cwd.is_empty() {
        let events =
            crate::session::log::read_events(&crate::session::live::log_path(gait_dir, session_id))
                .unwrap_or_default();
        crate::session::retrace::active_cwd(&events, codebase)
            .to_string_lossy()
            .into_owned()
    } else {
        impulse.cwd.clone()
    };

    let impulse_with_scope = Impulse {
        tool: impulse.tool.clone(),
        args: impulse.args,
        cwd,
        requested: Some(requested),
        prefetch: None,
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
    let feedback = match HandResponse::decode(out.stdout.as_slice()) {
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
    }
}

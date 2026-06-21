//! Spine-side helpers shared by the CLI spine (`gait run`) and the REPL spine
//! (`gait chat`).  Both spawn a `gait __brain` per turn and relay its Nerve
//! signals; this module owns that shared relay loop and the tool-execution helper.

use std::path::Path;
use std::sync::Arc;

use prost::Message as _;
use tokio::sync::Mutex;

use crate::config::SandboxConfig;
use crate::port::output::TurnSink;
use crate::proto::gait::sandbox::v1::{
    network_scope::Mode, signal, Feedback, Impulse, NetworkScope, Scope, Signal,
};
use crate::proto::wire;
use crate::sandbox::{NetworkPolicy, SandboxScope};
use crate::turn::nerve;

// ── relay_brain_turn ─────────────────────────────────────────────────────────

/// Spawn `gait __brain` for one turn and relay its Nerve signals.
///
/// * `Impulse`       → `run_tool_as_spine` (spawned task) → `Feedback`
/// * `Append`        → `wire::append_with_id` (spine is sole log writer)
/// * `StreamForward` → `sink.item()` (REPL TUI rendering)
/// * `Terminated`    → parse `TurnResult`, `sink.end_turn()`, `sink.result()`, return
///
/// Returns `Ok(true)` if the brain signalled failure, `Ok(false)` on success.
pub async fn relay_brain_turn(
    brain_args: &[String],
    gait_dir: &Path,
    codebase: &Path,
    session_id: &str,
    api_key: &str,
    exe: &Path,
    sandbox_cfg: &SandboxConfig,
    sandbox_on: bool,
    sink: &mut dyn TurnSink,
) -> anyhow::Result<bool> {
    use tokio::process::Command;

    let mut child = Command::new(exe)
        .arg("__brain")
        .args(brain_args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .env("GAIT_BRAIN_API_KEY", api_key)
        .spawn()
        .map_err(|e| anyhow::anyhow!("failed to spawn __brain: {e}"))?;

    let child_stdin = Arc::new(Mutex::new(child.stdin.take().expect("piped stdin")));
    let mut child_stdout = child.stdout.take().expect("piped stdout");

    let log_path = gait_dir.join("sessions").join(session_id).join("log.jsonl");

    let mut run_result: Option<crate::turn::TurnResult> = None;
    let mut brain_failed = false;

    sink.begin_turn();
    loop {
        let signal = match nerve::read_signal(&mut child_stdout).await {
            Ok(s) => s,
            Err(_) => break, // brain exited / pipe closed
        };
        let corr_id = signal.corr_id;
        match signal.body {
            Some(signal::Body::Impulse(impulse)) => {
                let stdin_writer = Arc::clone(&child_stdin);
                let cfg = sandbox_cfg.clone();
                let gd = gait_dir.to_path_buf();
                let cb = codebase.to_path_buf();
                let sid = session_id.to_string();
                let exe_c = exe.to_path_buf();
                tokio::spawn(async move {
                    let feedback =
                        run_tool_as_spine(impulse, &cfg, &gd, &cb, &sid, sandbox_on, &exe_c).await;
                    let resp = Signal {
                        corr_id,
                        body: Some(signal::Body::Feedback(feedback)),
                    };
                    let mut w = stdin_writer.lock().await;
                    nerve::write_signal(&mut *w, &resp).await.ok();
                });
            }
            Some(signal::Body::Append(append)) => {
                if let Some(evt) = append.event {
                    wire::append_with_id(&log_path, &evt).ok();
                }
            }
            Some(signal::Body::StreamForward(fwd)) => {
                if let Some(item) = fwd.item {
                    sink.item(&item);
                }
            }
            Some(signal::Body::Terminated(t)) => {
                brain_failed = t.failed;
                run_result = Some(turn_result_from_json(&t.run_result_json));
                break;
            }
            Some(signal::Body::Spawn(sp)) => {
                // Brain (sealed) delegates process-launch to spine.
                // Append --sandbox-locked so the child cannot disable sandboxing.
                let mut argv = sp.argv;
                if sandbox_on {
                    argv.push("--sandbox-locked".to_string());
                }
                let _ = std::process::Command::new(exe)
                    .args(&argv)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn();
            }
            _ => {}
        }
    }
    sink.end_turn();

    let _ = child.wait().await;

    if let Some(result) = run_result {
        sink.result(&result);
    }

    Ok(brain_failed)
}

// ── run_tool_as_spine ────────────────────────────────────────────────────────

/// Spawn `gait __run-tool` for one tool invocation (spine's authoritative scope
/// ceiling applied via `bound_request`). Mirrors `SubprocessSynapse::fire` but
/// takes an `Impulse` directly from the Nerve rather than a `Tool` ref.
pub async fn run_tool_as_spine(
    impulse: Impulse,
    sandbox_cfg: &SandboxConfig,
    gait_dir: &Path,
    codebase: &Path,
    session_id: &str,
    sandbox_on: bool,
    exe: &Path,
) -> Feedback {
    use tokio::io::AsyncWriteExt;
    use tokio::process::Command;

    let jail_raw = std::env::temp_dir().join(format!(
        "gait-hand-{}",
        uuid::Uuid::new_v4().to_string().replace('-', "")
    ));
    if std::fs::create_dir_all(&jail_raw).is_err() {
        return error_feedback("failed to create temp jail");
    }
    let jail = jail_raw.canonicalize().unwrap_or_else(|_| jail_raw.clone());

    let scope = crate::sandbox::ceiling::bound_request(&impulse.tool, sandbox_cfg, codebase, &jail);
    let requested = scope_to_proto(&scope);

    let impulse_with_scope = Impulse {
        tool: impulse.tool.clone(),
        args: impulse.args,
        cwd: impulse.cwd,
        requested: Some(requested),
    };
    let bytes = impulse_with_scope.encode_to_vec();

    let mut child = match Command::new(exe)
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
        .env("TMPDIR", &jail)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&jail);
            return error_feedback(&format!("failed to spawn __run-tool: {e}"));
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

    if out.status.success() {
        Feedback::decode(out.stdout.as_slice())
            .unwrap_or_else(|e| error_feedback(&format!("bad Feedback proto: {e}")))
    } else {
        error_feedback(&format!(
            "hand exited {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        ))
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Build a Feedback carrying a string error result.
pub fn error_feedback(msg: &str) -> Feedback {
    Feedback {
        result: Some(pbjson_types::Value {
            kind: Some(pbjson_types::value::Kind::StringValue(msg.to_string())),
        }),
        events: vec![],
        denied: None,
    }
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

/// Remove `--task <value>` from an arg slice (brain runs Resume mode).
pub fn strip_task_from_args(args: &mut Vec<String>) {
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--task" {
            args.remove(i);
            if i < args.len() {
                args.remove(i);
            }
        } else {
            i += 1;
        }
    }
}

/// Reconstruct a `TurnResult` from the JSON that `BrainSink` encodes in
/// `Terminated.run_result_json` (a serialized `RunResult` proto).
///
/// Only `Cancelled` and `Unlock` are faithfully reconstructed — those are the
/// only variants the REPL sink cares about. Other variants produce a minimal
/// `Done`.
fn turn_result_from_json(json: &str) -> crate::turn::TurnResult {
    use crate::proto::gait::log::v1::{run_result, RunResult};
    let rr: RunResult = serde_json::from_str(json).unwrap_or_default();
    match rr.result {
        Some(run_result::Result::Cancelled(_)) => crate::turn::TurnResult::Cancelled,
        Some(run_result::Result::Unlock(u)) => crate::turn::TurnResult::Unlock { items: u.items },
        Some(run_result::Result::Done(d)) => crate::turn::TurnResult::Done {
            message: d.message,
            result: None,
            changes: d.changes,
        },
        _ => crate::turn::TurnResult::Done {
            message: String::new(),
            result: None,
            changes: None,
        },
    }
}

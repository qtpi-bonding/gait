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
    network_scope::Mode, signal, Feedback, Impulse, NetworkScope, Notify, Scope, Signal,
};
use crate::proto::wire;
use crate::sandbox::{NetworkPolicy, SandboxScope};
use crate::turn::nerve;

// ── ProfileInputs + build_turn_profile ───────────────────────────────────────

/// Inputs the spine needs to build the turn's `Profile` (parsed by the CLI/REPL
/// caller, since gait-core cannot parse clap args). Mirrors the brain's old
/// agent/model/allow/deny handling.
pub struct ProfileInputs {
    pub agent_name: Option<String>,
    pub model_override: Option<String>,
    pub allow: Vec<String>,
    pub deny: Vec<String>,
}

/// Build the turn's `Profile` on the spine (disk scan: agent file, system-prompt
/// layers, skills/mcp/plugins). Folds CLI allow/deny into config first, mirroring
/// the brain's previous in-process logic.
pub fn build_turn_profile(
    gait_dir: &std::path::Path,
    codebase: &std::path::Path,
    inputs: &ProfileInputs,
) -> crate::profile::Profile {
    let mut cfg = crate::config::load_config(gait_dir);
    for p in &inputs.allow {
        if !cfg.allow.contains(p) {
            cfg.allow.push(p.clone());
        }
    }
    for p in &inputs.deny {
        if !cfg.deny.contains(p) {
            cfg.deny.push(p.clone());
        }
    }
    let agent_path: Option<std::path::PathBuf> = match &inputs.agent_name {
        Some(name) => Some(crate::agent_frontmatter::resolve_markdown_context_path(
            gait_dir,
            crate::agent_frontmatter::MarkdownKind::Agent,
            name,
        )),
        None => {
            let default = crate::agent_frontmatter::resolve_markdown_context_path(
                gait_dir,
                crate::agent_frontmatter::MarkdownKind::Agent,
                "default",
            );
            default.exists().then_some(default)
        }
    };
    crate::profile::Profile::from_config_and_agent(
        &cfg,
        gait_dir,
        Some(codebase),
        agent_path.as_deref(),
        inputs.model_override.clone(),
    )
}

// ── relay_brain_turn ─────────────────────────────────────────────────────────

/// Spawn `gait __brain` for one turn and relay its Nerve signals.
///
/// * `Impulse`       → `run_tool_as_spine` (spawned task) → `Feedback`
/// * `Append`        → `wire::append_with_id` (spine is sole log writer)
/// * `StreamForward` → `sink.item()` (REPL TUI rendering)
/// * `Terminated`    → parse `TurnResult`, `sink.end_turn()`, `sink.result()`, return
///
/// The spine is the sole gait-dir writer. Before spawning the brain, it performs
/// all startup disk writes (worktree, checkpoint, session.json, lock, SessionStart)
/// so the brain can run with `writable: []`. The brain's `run_turn` skips these
/// when `relay.is_some()`.
///
/// Returns `Ok(true)` if the brain signalled failure, `Ok(false)` on success.
#[allow(clippy::too_many_arguments)]
pub async fn relay_brain_turn(
    brain_args: &[String],
    gait_dir: &Path,
    codebase: &Path,
    session_id: &str,
    api_key: &str,
    exe: &Path,
    sandbox_cfg: &SandboxConfig,
    sandbox_on: bool,
    profile_inputs: &ProfileInputs,
    sink: &mut dyn TurnSink,
) -> anyhow::Result<bool> {
    use tokio::process::Command;

    // ── Spine-owned startup writes (brain is disk-write-free) ───────────────
    // These all write to gait-dir; the brain's seal has writable: [] so it
    // cannot do them. The spine does them before spawning the brain.
    let repos = crate::git::Repos::production();
    let session_dir = gait_dir.join("sessions").join(session_id);
    let tx_path = session_dir.join("log.jsonl");

    // 1. Session worktree (idempotent — callers may have already done this).
    repos.traces.create_session_worktree(gait_dir, session_id)?;

    // 2. Checkpoint repo init (idempotent — checks if HEAD exists).
    repos.checkpoints.init(gait_dir, session_id, codebase)?;

    // 3. Session lock — held for the duration of the brain process.
    let _lock = crate::turn::SessionLock::acquire(&session_dir)?;

    // 5. SessionStart event — write once per session (idempotent via event check).
    {
        use crate::proto::gait::log::v1::{event, payload};
        let ingestor = crate::session::Ingestor::from_session(gait_dir, session_id);
        let has_start = ingestor.events().unwrap_or_default().iter().any(|e| {
            matches!(
                e.body.as_ref().and_then(|b| match b {
                    event::Body::Payload(p) => p.kind.as_ref(),
                    _ => None,
                }),
                Some(payload::Kind::SessionStart(_))
            )
        });
        if !has_start {
            let codebase_hash = repos.codebase.head_hash(codebase).unwrap_or_default();
            let dirty_files = repos.codebase.dirty_files(codebase);
            let codebase_dirty = !dirty_files.is_empty();
            crate::session::log::init_session(crate::session::log::InitSessionOpts {
                log_path: tx_path.clone(),
                id: session_id.to_string(),
                model: String::new(),
                codebase_hash,
                codebase_dirty,
                assigned_by: None,
                codebase_path: Some(codebase.to_string_lossy().into_owned()),
            })?;
        }
    }

    // ── Spawn brain ─────────────────────────────────────────────────────────
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

    // ── BrainInit: push config + full log + git so the brain reads zero disk. ──
    // The caller appended this turn's user input before relay_brain_turn, and the
    // SessionStart write above ran, so `events` is complete.
    {
        use crate::proto::gait::sandbox::v1::{signal, BrainInit, GitStatus, Signal};
        use tokio::io::AsyncWriteExt;
        let cfg = crate::config::load_config(gait_dir);
        let config_json = serde_json::to_string(&cfg).unwrap_or_else(|_| "{}".to_string());
        let events = crate::session::log::read_events(&log_path).unwrap_or_default();
        let head_hash = repos.codebase.head_hash(codebase).unwrap_or_default();
        let dirty_files = repos.codebase.dirty_files(codebase);
        let profile = build_turn_profile(gait_dir, codebase, profile_inputs);
        let profile_json = serde_json::to_string(&profile).unwrap_or_else(|_| "null".to_string());
        let init = BrainInit {
            config_json,
            events,
            git: Some(GitStatus {
                head_hash,
                dirty_files,
            }),
            profile_json,
        };
        let sig = Signal {
            corr_id: 0,
            body: Some(signal::Body::BrainInit(init)),
        };
        let mut w = child_stdin.lock().await;
        nerve::write_signal(&mut *w, &sig).await?;
        w.flush().await?;
    }

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
                    use tokio::io::AsyncWriteExt;
                    w.flush().await.ok();
                });
            }
            Some(signal::Body::Append(append)) => {
                if let Some(mut evt) = append.event {
                    enrich_permissions_preview(
                        &mut evt,
                        codebase,
                        gait_dir,
                        session_id,
                        sandbox_cfg,
                    );
                    wire::append_with_id(&log_path, &evt).ok();
                }
            }
            Some(signal::Body::StreamForward(fwd)) => {
                if let Some(mut item) = fwd.item {
                    if let Some(crate::proto::gait::log::v1::stream_item::Item::Event(
                        ref mut evt,
                    )) = item.item
                    {
                        enrich_permissions_preview(
                            evt,
                            codebase,
                            gait_dir,
                            session_id,
                            sandbox_cfg,
                        );
                    }
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
            Some(signal::Body::Notify(Notify {
                target_session,
                from_session,
                status,
                summary,
            })) => {
                // Spine is sole gait-dir writer: write agent_update to parent session.
                let event = crate::proto::events::agent_update(
                    from_session.clone(),
                    &status,
                    summary.clone(),
                    None,
                );
                let target_dir = gait_dir.join("sessions").join(&target_session);
                let tx_path = target_dir.join("log.jsonl");
                // Acquire lock with retry (parent may be running).
                let mut lock: Option<crate::turn::SessionLock> = None;
                for _ in 0..20 {
                    match crate::turn::SessionLock::acquire(&target_dir) {
                        Ok(l) => {
                            lock = Some(l);
                            break;
                        }
                        Err(e) if e.to_string().contains("already running") => {
                            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                        }
                        Err(e) => {
                            eprintln!("[spine] warning: failed to acquire lock for notify: {e}");
                            break;
                        }
                    }
                }
                if let Some(_lock) = lock {
                    wire::append(&tx_path, event).ok();
                    let repos = crate::git::Repos::production();
                    repos
                        .traces
                        .commit_session(
                            gait_dir,
                            &target_session,
                            &format!(
                                "session({}): agent_update from {}",
                                target_session, from_session
                            ),
                        )
                        .ok();
                }
                // No Feedback needed — one-way signal like Append.
            }
            _ => {}
        }
    }
    sink.end_turn();

    // Drop the brain's stdin writer to signal EOF — the brain's Nerve reader
    // task is blocking on read_signal from stdin and will exit on EOF, which
    // lets the brain's tokio runtime shut down and the process exit.
    drop(child_stdin);

    let _ = child.wait().await;

    if let Some(result) = &run_result {
        // The spine is the sole gait-dir writer. The brain skipped
        // commit_session and checkpoint_codebase (writable: []) — the spine
        // does both here, after the brain exits, so the session worktree and
        // shadow checkpoint repo reflect the transcript at this exitable
        // boundary.
        let msg = match result {
            crate::turn::TurnResult::Done { .. } => {
                format!("session({}): turn complete", session_id)
            }
            crate::turn::TurnResult::Cancelled => {
                format!("session({}): cancelled", session_id)
            }
            crate::turn::TurnResult::Permissions => {
                format!("session({}): permissions", session_id)
            }
            crate::turn::TurnResult::Clarify { .. } => {
                format!("session({}): clarify", session_id)
            }
            crate::turn::TurnResult::Inform { .. } => {
                format!("session({}): inform", session_id)
            }
            crate::turn::TurnResult::Unlock { .. } => {
                format!("session({}): unlock", session_id)
            }
        };

        // Checkpoint the codebase (shadow repo in gait-dir). Best-effort — a
        // failure means `gait rewind` won't have a snapshot for this turn,
        // but the turn itself is complete.
        let events = crate::session::log::read_events(&log_path).unwrap_or_default();
        let turn_n = crate::session::log::count_turn_complete(&events) as u32;
        let _ = repos
            .checkpoints
            .checkpoint(gait_dir, session_id, codebase, turn_n);

        // Commit the transcript to the session worktree.
        let _ = repos.traces.commit_session(gait_dir, session_id, &msg);
    }

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

    // Resolve cwd: if the brain sent a cwd in the Impulse, use it; otherwise
    // read active_cwd from the session log (same as SubprocessSynapse did).
    // RelaySynapse sends empty cwd because the brain doesn't read the session
    // log for cwd — the spine owns it.
    let cwd = if impulse.cwd.is_empty() {
        let events = crate::session::log::read_events(
            &gait_dir.join("sessions").join(session_id).join("log.jsonl"),
        )
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

    // The hand always writes a HandResponse on stdout. Decode it regardless of
    // exit code; only fall back to exit-code/stderr if stdout is missing/garbled
    // (a true crash — the hand never got to write).
    use gait_proto::gait::sandbox::v1::{hand_response, HandResponse};
    match HandResponse::decode(out.stdout.as_slice()) {
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

// ── enrich_permissions_preview ───────────────────────────────────────────────

/// Spine-side preview: for a `Permissions` event, fill any empty `preview` on
/// edit/write items by running that tool's own `preview()` in-process. The spine
/// is unsealed and already opens the codebase, so this is where the diff read
/// belongs (the brain stays read-free). Scope-bounded: a target outside the
/// tool's read ceiling yields no preview rather than reading an arbitrary file.
fn enrich_permissions_preview(
    event: &mut crate::proto::gait::log::v1::Event,
    codebase: &Path,
    gait_dir: &Path,
    session_id: &str,
    sandbox_cfg: &SandboxConfig,
) {
    use crate::proto::gait::log::v1::{event, payload};

    let Some(event::Body::Payload(p)) = event.body.as_mut() else {
        return;
    };
    let Some(payload::Kind::Permissions(perm)) = p.kind.as_mut() else {
        return;
    };

    let ctx = crate::tools::ToolCtx::new(
        codebase.to_path_buf(),
        gait_dir.to_path_buf(),
        session_id.to_string(),
    );
    let tools =
        crate::tools::builtin_tools(ctx, vec![], vec![], vec![], crate::git::Repos::production());

    let jail = std::env::temp_dir().join(format!(
        "gait-preview-{}",
        uuid::Uuid::new_v4().to_string().replace('-', "")
    ));

    for item in perm.items.iter_mut() {
        if item.preview.is_some() {
            continue; // already filled (e.g. non-relay path)
        }
        let Some(tool) = tools.iter().find(|t| t.name() == item.tool_name) else {
            continue;
        };
        // Convert tool_args (proto Struct) back to serde_json::Value for the tool API.
        let args: serde_json::Value = item
            .tool_args
            .as_ref()
            .and_then(|s| serde_json::to_value(s).ok())
            .unwrap_or(serde_json::Value::Null);

        // Scope-bound the read: only preview targets within the tool's read ceiling.
        let ceiling =
            crate::sandbox::ceiling::compute_scope(tool.access(), sandbox_cfg, codebase, &jail);
        let targets = tool.scope_targets(&args);
        let in_scope = targets.is_empty()
            || targets.iter().all(|(path, _dim)| {
                // Canonicalize the target path so /var/... and /private/var/... compare equal.
                let canonical = path.canonicalize().unwrap_or_else(|_| path.clone());
                ceiling.read_all
                    || ceiling.readable.iter().any(|r| canonical.starts_with(r))
                    || ceiling.writable.iter().any(|w| canonical.starts_with(w))
            });
        if !in_scope {
            continue;
        }

        if let Some(diff) = tool.preview(&args) {
            item.preview = Some(diff);
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::{build_turn_profile, enrich_permissions_preview, error_feedback, ProfileInputs};

    #[test]
    fn hand_response_error_arm_becomes_error_feedback() {
        use gait_proto::gait::sandbox::v1::{hand_response, HandError, HandResponse};
        use prost::Message as _;
        let bytes = HandResponse {
            outcome: Some(hand_response::Outcome::Error(HandError {
                message: "nope".into(),
            })),
        }
        .encode_to_vec();
        // mirror the spine's decode mapping
        let fb = match HandResponse::decode(bytes.as_slice()) {
            Ok(HandResponse {
                outcome: Some(hand_response::Outcome::Error(e)),
            }) => error_feedback(&format!("hand error: {}", e.message)),
            _ => panic!("expected error arm"),
        };
        let s = fb.result.and_then(|v| match v.kind {
            Some(pbjson_types::value::Kind::StringValue(s)) => Some(s),
            _ => None,
        });
        assert_eq!(s.as_deref(), Some("hand error: nope"));
    }

    #[test]
    fn build_turn_profile_folds_cli_allow() {
        let dir = tempfile::TempDir::new().unwrap();
        // Write a minimal gait.jsonc so load_config picks up a known model.
        std::fs::write(dir.path().join("gait.jsonc"), r#"{"model":"anthropic/x"}"#).unwrap();
        let inputs = ProfileInputs {
            agent_name: None,
            model_override: None,
            allow: vec!["read".into()],
            deny: vec![],
        };
        let p = build_turn_profile(dir.path(), dir.path(), &inputs);
        assert_eq!(p.model, "anthropic/x");
        assert!(p.allow.iter().any(|a| a == "read"));
    }

    #[test]
    fn build_turn_profile_uses_default_model_without_config() {
        let dir = tempfile::TempDir::new().unwrap();
        // No gait.jsonc — load_config falls back to GaitConfig::default().
        let inputs = ProfileInputs {
            agent_name: None,
            model_override: None,
            allow: vec![],
            deny: vec![],
        };
        let p = build_turn_profile(dir.path(), dir.path(), &inputs);
        assert_eq!(p.model, crate::config::DEFAULT_MODEL);
    }

    fn make_permissions_event(
        items: Vec<crate::proto::gait::log::v1::PermissionItem>,
    ) -> crate::proto::gait::log::v1::Event {
        use crate::proto::gait::log::v1::{event, payload, Payload, Permissions};
        gait_proto::envelope::mint(
            event::Body::Payload(Payload {
                kind: Some(payload::Kind::Permissions(Permissions { items })),
            }),
            None,
            0,
            0,
        )
    }

    /// An in-scope write target: preview should be filled.
    #[test]
    fn enrich_fills_preview_for_in_scope_write() {
        use crate::config::SandboxConfig;
        use crate::proto::gait::log::v1::{event, payload, PermissionItem};

        let dir = tempfile::TempDir::new().unwrap();
        let codebase = dir.path();
        let target_file = codebase.join("hello.txt");
        std::fs::write(&target_file, "old content\n").unwrap();

        let item = PermissionItem {
            id: "req-1".into(),
            cmd: "write hello.txt".into(),
            reason: "write requires permission".into(),
            patterns: vec!["write:hello.txt".into()],
            tool_name: "write".into(),
            tool_args: crate::proto::events::to_struct(&serde_json::json!({
                "file_path": target_file.to_string_lossy(),
                "content": "new content\n"
            })),
            preview: None,
        };

        let mut evt = make_permissions_event(vec![item]);
        let sandbox_cfg = SandboxConfig::default();
        enrich_permissions_preview(&mut evt, codebase, codebase, "s1", &sandbox_cfg);

        let Some(event::Body::Payload(p)) = evt.body.as_ref() else {
            panic!("expected payload body");
        };
        let Some(payload::Kind::Permissions(perm)) = p.kind.as_ref() else {
            panic!("expected permissions");
        };
        assert_eq!(perm.items.len(), 1);
        assert!(
            perm.items[0].preview.is_some(),
            "expected preview to be filled for in-scope write"
        );
    }

    /// A target outside the codebase: preview should stay None.
    #[test]
    fn enrich_skips_preview_for_out_of_scope_target() {
        use crate::config::SandboxConfig;
        use crate::proto::gait::log::v1::{event, payload, PermissionItem};

        let dir = tempfile::TempDir::new().unwrap();
        let codebase = dir.path();

        // Point the write outside the codebase (e.g. /etc/passwd)
        let item = PermissionItem {
            id: "req-2".into(),
            cmd: "write /etc/passwd".into(),
            reason: "write requires permission".into(),
            patterns: vec!["write:/etc/passwd".into()],
            tool_name: "write".into(),
            tool_args: crate::proto::events::to_struct(&serde_json::json!({
                "file_path": "/etc/passwd",
                "content": "malicious"
            })),
            preview: None,
        };

        let mut evt = make_permissions_event(vec![item]);
        let sandbox_cfg = SandboxConfig::default();
        enrich_permissions_preview(&mut evt, codebase, codebase, "s1", &sandbox_cfg);

        let Some(event::Body::Payload(p)) = evt.body.as_ref() else {
            panic!("expected payload body");
        };
        let Some(payload::Kind::Permissions(perm)) = p.kind.as_ref() else {
            panic!("expected permissions");
        };
        assert!(
            perm.items[0].preview.is_none(),
            "expected preview to be None for out-of-scope target"
        );
    }

    /// Already-filled preview should not be overwritten.
    #[test]
    fn enrich_does_not_overwrite_existing_preview() {
        use crate::config::SandboxConfig;
        use crate::proto::gait::log::v1::{event, payload, PermissionItem};

        let dir = tempfile::TempDir::new().unwrap();
        let codebase = dir.path();
        let target_file = codebase.join("existing.txt");
        std::fs::write(&target_file, "content\n").unwrap();

        let item = PermissionItem {
            id: "req-3".into(),
            cmd: "write existing.txt".into(),
            reason: "write requires permission".into(),
            patterns: vec![],
            tool_name: "write".into(),
            tool_args: crate::proto::events::to_struct(&serde_json::json!({
                "file_path": target_file.to_string_lossy(),
                "content": "new\n"
            })),
            preview: Some("--- brain computed this ---".into()),
        };

        let mut evt = make_permissions_event(vec![item]);
        let sandbox_cfg = SandboxConfig::default();
        enrich_permissions_preview(&mut evt, codebase, codebase, "s1", &sandbox_cfg);

        let Some(event::Body::Payload(p)) = evt.body.as_ref() else {
            panic!();
        };
        let Some(payload::Kind::Permissions(perm)) = p.kind.as_ref() else {
            panic!();
        };
        assert_eq!(
            perm.items[0].preview.as_deref(),
            Some("--- brain computed this ---"),
            "existing preview must not be overwritten"
        );
    }
}

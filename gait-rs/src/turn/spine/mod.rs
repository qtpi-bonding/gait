//! Spine-side helpers shared by the CLI spine (`gait run`) and the REPL spine
//! (`gait chat`).  Both spawn a `gait __brain` per turn and relay its Nerve
//! signals; this module owns that shared relay loop and the tool-execution helper.

use std::path::Path;
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::config::SandboxConfig;
use crate::port::output::TurnSink;
use crate::proto::gait::sandbox::v1::{signal, Notify};
#[allow(unused_imports)] // used in test-only wire::append calls
use crate::proto::wire;
use crate::turn::nerve;
use crate::turn::DiagnosticSink;

mod watchdog;
#[allow(unused_imports)] // Watchdog adopted by relay.rs in a later stage
pub(crate) use watchdog::{watchdog_breach, SpinePhase, Watchdog, WatchdogCfg};

mod preview;
pub(crate) use preview::enrich_permissions_preview;
mod util;
pub use util::strip_task_from_args;
pub(crate) use util::turn_result_from_json;

mod lifecycle;
pub(crate) use lifecycle::brain_argv;
pub use lifecycle::{build_turn_profile, ProfileInputs, TurnMode};

// ── relay_brain_turn ─────────────────────────────────────────────────────────

/// Shared slot holding the active cascade group for the current turn.
/// The signal handler (SIGINT/SIGTERM) reads this to call `kill_all()`.
pub type SignalSlot =
    std::sync::Arc<std::sync::Mutex<Option<std::sync::Arc<dyn crate::process::Containment>>>>;

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
    gait_dir: &Path,
    codebase: &Path,
    session_id: &str,
    api_key: &str,
    exe: &Path,
    sandbox_cfg: &SandboxConfig,
    sandbox_on: bool,
    auto: bool,
    max_iter: Option<usize>,
    profile_inputs: &ProfileInputs,
    sink: &mut dyn TurnSink,
    cascade_group_arg: Option<String>,
    // Optional shared slot: if provided, the established group is stored here
    // for the duration of the turn so an external signal handler can call
    // kill_all(). The caller must clear the slot after the turn completes.
    signal_slot: Option<&SignalSlot>,
    repos: &crate::git::Repos,
    mode: &TurnMode,
) -> anyhow::Result<bool> {
    // ── Establish (or inherit) the cascade group for this turn ─────────────
    let group = crate::process::establish(cascade_group_arg);

    // Store the group in the signal slot (if provided) so SIGINT/SIGTERM can
    // reap it. Sequenced: establish → store → run → kill_all → caller clears.
    if let Some(slot) = signal_slot {
        *slot.lock().unwrap() = Some(Arc::clone(&group));
    }
    // ── Build the turn context once ────────────────────────────────────────
    // The ctx carries every spine-side handle the turn needs (gait-dir,
    // codebase, session id, repos, exe, cascade group, sandbox flag). Building
    // it here means the persistence types can later be constructed from `&ctx`
    // and the relay loop can clone it cheaply into spawned tasks.
    let ctx = TurnCtx {
        gait_dir: gait_dir.to_path_buf(),
        codebase: codebase.to_path_buf(),
        session_id: session_id.to_string(),
        repos: repos.clone(),
        exe: exe.to_path_buf(),
        group: Arc::clone(&group),
        sandbox_on,
    };

    // Sole owner of diagnostic.jsonl writes for this turn. Built once after
    // TurnCtx so the three call sites can use `&diag` without rebuilding the
    // path. See src/turn/diag_sink.rs for the gating/terminal discipline.
    let diag = DiagnosticSink::new(gait_dir, session_id);

    // Session-scoped git handle — holds only traces+checkpoints Arcs (no codebase
    // repo), so codebase mixing is structurally impossible.
    // Session-scoped git handle — null in ephemeral mode (no worktree/checkpoint/commit).
    let git = match mode {
        TurnMode::Persistent => crate::turn::SessionGit::for_session(repos, gait_dir, session_id),
        TurnMode::Ephemeral { .. } => crate::turn::SessionGit::null(),
    };

    let session_dir = gait_dir.join("sessions").join(session_id);

    // Persistent: worktree + checkpoint init + lock + SessionStart, exactly as before.
    // Ephemeral: an in-memory log seeded with the one-off input; no FS writes at all.
    let log = match mode {
        TurnMode::Persistent => {
            git.ensure_worktree()?;
            git.checkpoint_init(codebase)?;
            let log = crate::turn::SessionLog::open(&session_dir)?;
            // SessionStart — write once per session (idempotent via event check).
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
                    let tx_path = log
                        .log_path()
                        .expect("persistent log has a path")
                        .to_path_buf();
                    crate::session::log::init_session(crate::session::log::InitSessionOpts {
                        log_path: tx_path,
                        id: session_id.to_string(),
                        model: String::new(),
                        codebase_hash,
                        codebase_dirty,
                        assigned_by: None,
                        codebase_path: Some(codebase.to_string_lossy().into_owned()),
                        notify_done: false,
                    })?;
                }
            }
            log
        }
        TurnMode::Ephemeral { input } => crate::turn::SessionLog::ephemeral(input.clone()),
    };
    // ── Spawn brain (enrolled as cascade group leader) ──────────────────────
    let mut child = {
        use tokio::process::Command as TokioCommand;
        let brain_args = brain_argv(
            gait_dir,
            codebase,
            session_id,
            auto,
            max_iter,
            sandbox_on,
            profile_inputs,
        );
        let mut std_cmd = std::process::Command::new(exe);
        std_cmd
            .arg("__brain")
            .args(&brain_args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .env("GAIT_BRAIN_API_KEY", api_key);
        // Set up cascade group enrollment hooks (setsid for brain = first call).
        group
            .configure_enrolled(&mut std_cmd)
            .map_err(|e| anyhow::anyhow!("failed to configure brain enrollment: {e}"))?;
        // Convert to tokio Command (keeps all pre_exec hooks) and spawn async.
        let mut tokio_cmd = TokioCommand::from(std_cmd);
        let child = tokio_cmd
            .spawn()
            .map_err(|e| anyhow::anyhow!("failed to spawn __brain: {e}"))?;
        // Register brain pid so group can reap it and knows its pgid.
        group.register_pid(child.id().unwrap_or(0) as i32);
        child
    };
    // Capture the group handle now that the brain (leader) is realized.
    let _group_handle = group.handle(); // Future: pass to blocking subagents (Plan 4).

    let child_stdin = Arc::new(Mutex::new(child.stdin.take().expect("piped stdin")));
    let mut child_stdout = child.stdout.take().expect("piped stdout");

    // ── BrainInit: push config + full log + git so the brain reads zero disk. ──
    // The caller appended this turn's user input before relay_brain_turn, and the
    // SessionStart write above ran, so `events` is complete.
    {
        use crate::proto::gait::sandbox::v1::{signal, BrainInit, DaemonStatus, GitStatus, Signal};
        use tokio::io::AsyncWriteExt;
        let cfg = crate::config::load_config(gait_dir);
        let config_json = serde_json::to_string(&cfg).unwrap_or_else(|_| "{}".to_string());
        let events = log.events();
        let head_hash = repos.codebase.head_hash(codebase).unwrap_or_default();
        let dirty_files = repos.codebase.dirty_files(codebase);
        let profile = build_turn_profile(gait_dir, codebase, profile_inputs);
        let profile_json = serde_json::to_string(&profile).unwrap_or_else(|_| "null".to_string());
        let daemons = {
            use crate::session::daemons::{active, is_alive};
            active(gait_dir)
                .unwrap_or_default()
                .into_iter()
                .map(|r| DaemonStatus {
                    alive: is_alive(&r),
                    name: r.name,
                    pid: r.pid,
                    log_path: r.log_path,
                })
                .collect::<Vec<_>>()
        };
        let init = BrainInit {
            config_json,
            events,
            git: Some(GitStatus {
                head_hash,
                dirty_files,
            }),
            profile_json,
            daemons,
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
    // Track in-flight ToolCall call_ids (→ tool names) so we can synthesize a
    // ToolResult for any that are orphaned if the brain is killed.
    let mut pending_calls: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    let mut inflight: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();
    // Transitions: BrainPhase→HandsPhase on first ToolCall insert; back on drain.
    let mut spine_phase = SpinePhase::BrainPhase;

    let wd = WatchdogCfg::default();
    let mut last_phase = crate::diagnostic::Phase::TurnBegin;
    let mut last_phase_elapsed_ms: u64 = 0;
    // None until the first ping → cold-start grace (watchdog disarmed).
    let mut last_ping_at: Option<std::time::Instant> = None;

    sink.begin_turn();
    loop {
        let read = nerve::read_signal(&mut child_stdout);
        let timed = tokio::time::timeout(std::time::Duration::from_millis(wd.ping_ms), read).await;
        let signal = match timed {
            Ok(Ok(s)) => s,
            Ok(Err(_)) => break, // pipe closed (brain exited)
            Err(_) => {
                // read timed out → fall through to the watchdog check
                let since = last_ping_at.map(|t| t.elapsed().as_millis() as u64);
                if let Some(reason) =
                    watchdog_breach(&wd, last_phase, last_phase_elapsed_ms, since, spine_phase)
                {
                    lifecycle::emit_terminal_failure(
                        &log,
                        &diag,
                        &mut brain_failed,
                        last_phase,
                        reason,
                    );
                    let _ = group.kill_all();
                    break;
                }
                continue;
            }
        };
        let corr_id = signal.corr_id;
        match signal.body {
            Some(signal::Body::Impulse(impulse)) => {
                let stdin_writer = Arc::clone(&child_stdin);
                if impulse.tool == "spawn_agent" {
                    let ctx = ctx.clone();
                    inflight.spawn(async move {
                        let args = handlers::agent::parse_spawn_args(&impulse);
                        let feedback = handlers::agent::handle_spawn_agent(args, ctx).await;
                        handlers::reply_feedback(stdin_writer, corr_id, feedback).await;
                    });
                } else {
                    let cfg = sandbox_cfg.clone();
                    let gd = gait_dir.to_path_buf();
                    let cb = codebase.to_path_buf();
                    let sid = session_id.to_string();
                    let exe_c = exe.to_path_buf();
                    let grp = Arc::clone(&group);
                    inflight.spawn(async move {
                        let feedback = run_tool_as_spine(
                            impulse, &cfg, &gd, &cb, &sid, sandbox_on, &exe_c, grp,
                        )
                        .await;
                        handlers::reply_feedback(stdin_writer, corr_id, feedback).await;
                    });
                }
            }
            Some(signal::Body::Append(append)) => {
                if let Some(mut evt) = append.event {
                    // Track in-flight ToolCall / ToolResult pairs by call_id.
                    // On cancel, any remaining pending_calls are orphans that need
                    // a synthesized ToolResult so retrace reconstructs cleanly.
                    use crate::proto::gait::log::v1::{event, payload};
                    if let Some(event::Body::Payload(ref p)) = evt.body {
                        match p.kind.as_ref() {
                            Some(payload::Kind::ToolCall(tc)) => {
                                pending_calls.insert(tc.call_id.clone(), tc.name.clone());
                                if pending_calls.len() == 1 {
                                    spine_phase = SpinePhase::HandsPhase;
                                }
                            }
                            Some(payload::Kind::ToolResult(tr)) => {
                                pending_calls.remove(&tr.call_id);
                                if pending_calls.is_empty() {
                                    spine_phase = SpinePhase::BrainPhase;
                                }
                            }
                            _ => {}
                        }
                    }
                    enrich_permissions_preview(
                        &mut evt,
                        codebase,
                        gait_dir,
                        session_id,
                        sandbox_cfg,
                    );
                    log.relay_event(&evt).ok();
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
                // Brain-reported failure: the brain already appended its own Error
                // event to log.jsonl via Signal::Append before sending Terminated,
                // so we must NOT double-write the event here. We do write a
                // diagnostic record so the failure is always explained in
                // diagnostic.jsonl regardless of GAIT_DEBUG.
                if t.failed {
                    let rec = crate::diagnostic::DiagRecord::terminal(
                        session_id,
                        last_phase,
                        "brain reported failure via Terminated signal",
                    );
                    diag.terminal(&rec.to_json_line());
                }
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
                if sp.detached {
                    // Peer agent: breaks out of the cascade group — survives parent death.
                    let mut cmd = std::process::Command::new(exe);
                    cmd.args(&argv)
                        .stdin(std::process::Stdio::null())
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null());
                    let _ = group.spawn_detached(cmd);
                } else {
                    // Subagent: enrolled in parent's cascade group — reaped when parent ends.
                    // Pass --cascade-group so the child self-assigns (race-free enrollment).
                    if let Some(handle) = group.handle() {
                        argv.extend(["--cascade-group".to_string(), handle]);
                    } else {
                        eprintln!("[spine] warning: spawn_agent cascade-group handle unavailable; subagent will not be enrolled in cascade group");
                    }
                    let mut cmd = std::process::Command::new(exe);
                    cmd.args(&argv)
                        .stdin(std::process::Stdio::null())
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null());
                    let _ = group.spawn_enrolled(cmd);
                }
            }
            Some(signal::Body::Notify(Notify {
                target_session,
                from_session,
                status,
                summary,
            })) => {
                // Route subagent completion through notify_parent (one-shot lock acquire,
                // dedup, write, commit, spawn_wake). Replaces the 20-retry open_retrying
                // path. ParentBusy → the live turn's per-step poll surfaces it next step.
                let c = crate::session::completion::Completion::Agent {
                    from: from_session.clone(),
                    status: status.clone(),
                    summary: summary.clone(),
                };
                match crate::session::completion::notify_parent(
                    gait_dir,
                    repos,
                    &target_session,
                    &c,
                ) {
                    crate::session::completion::NotifyOutcome::Woke => {}
                    crate::session::completion::NotifyOutcome::ParentBusy => {}
                    crate::session::completion::NotifyOutcome::AlreadySurfaced => {}
                    crate::session::completion::NotifyOutcome::NoParent => {
                        eprintln!(
                            "[spine] warning: parent session {target_session} not found for notify"
                        );
                    }
                }
                // No Feedback needed — one-way signal like Append.
            }
            Some(signal::Body::Diagnostic(diag_msg)) => {
                last_phase = crate::diagnostic::phase_from_str(&diag_msg.phase);
                last_phase_elapsed_ms = diag_msg.elapsed_ms;
                last_ping_at = Some(std::time::Instant::now());
                diag.ping(&diag_msg.record_json);
            }
            _ => {}
        }

        // Watchdog: runs after every signal (in addition to every read-timeout).
        let since = last_ping_at.map(|t| t.elapsed().as_millis() as u64);
        if let Some(reason) =
            watchdog_breach(&wd, last_phase, last_phase_elapsed_ms, since, spine_phase)
        {
            lifecycle::emit_terminal_failure(&log, &diag, &mut brain_failed, last_phase, reason);
            let _ = group.kill_all();
            break;
        }
    }
    lifecycle::teardown(
        sink,
        inflight,
        pending_calls,
        child_stdin,
        group,
        child,
        git,
        codebase,
        session_id,
        &log,
        run_result,
        brain_failed,
    )
    .await
}

mod handlers;
pub(crate) use handlers::tool::run_tool_as_spine;
#[allow(unused_imports)] // path-stable facade re-export; no in-crate caller yet
pub(crate) use handlers::{error_feedback, TurnCtx};

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    #[test]
    fn diagnostic_line_is_appended_to_diagnostic_jsonl() {
        let tmp = tempfile::tempdir().unwrap();
        let gait_dir = tmp.path();
        let session = "stest";
        std::fs::create_dir_all(gait_dir.join("sessions").join(session)).unwrap();

        super::DiagnosticSink::new(gait_dir, session).terminal(r#"{"phase":"awaiting_headers"}"#);
        super::DiagnosticSink::new(gait_dir, session).terminal(r#"{"phase":"streaming"}"#);

        let p = gait_dir
            .join("sessions")
            .join(session)
            .join("diagnostic.jsonl");
        let body = std::fs::read_to_string(&p).unwrap();
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("awaiting_headers"));
        // And it did NOT touch log.jsonl
        assert!(!gait_dir
            .join("sessions")
            .join(session)
            .join("log.jsonl")
            .exists());
    }
}

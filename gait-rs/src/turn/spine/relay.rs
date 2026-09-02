//! The relay loop: `relay_brain_turn` spawns a `gait __brain` subprocess for
//! one turn and dispatches its Nerve signals until the turn ends, then hands
//! off to `lifecycle::teardown`.

use std::path::Path;
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::config::SandboxConfig;
use crate::port::output::TurnSink;
use crate::proto::gait::sandbox::v1::{signal, Notify};
use crate::turn::nerve;
use crate::turn::DiagnosticSink;

use super::checkpoint_enrich::enrich_checkpoint;
use super::handlers::{self, tool::run_tool_as_spine, TurnCtx};
use super::lifecycle::{self, brain_argv, ProfileInputs, TurnMode};
use super::preview::enrich_permissions_preview;
use super::watchdog::{SpinePhase, Watchdog};

/// Encode credential material exclusively for the private first BrainInit
/// frame. This must never be reused for events, diagnostics, or environment.
fn credential_to_wire_json(credential: &gait_auth::ResolvedCredential) -> anyhow::Result<String> {
    let value = match credential {
        gait_auth::ResolvedCredential::ApiKey(key) => serde_json::json!({
            "kind": "api_key", "secret": key,
        }),
        gait_auth::ResolvedCredential::Subscription(subscription) => serde_json::json!({
            "kind": "subscription", "credential": subscription,
        }),
    };
    serde_json::to_string(&value).map_err(Into::into)
}

/// Shared slot holding the active cascade group for the current turn.
/// The signal handler (SIGINT/SIGTERM) reads this to call `kill_all()`.
pub type SignalSlot =
    std::sync::Arc<std::sync::Mutex<Option<std::sync::Arc<dyn crate::process::Containment>>>>;

/// State owned exclusively by the relay loop. Handlers never touch it — they
/// receive `TurnCtx` and return values; only `relay_brain_turn` mutates it.
struct RelayState {
    finalized_turn: Option<crate::proto::gait::sandbox::v1::FinalizedTurn>,
    brain_failed: bool,
    /// In-flight ToolCall call_id → its originating tool and step, for orphan
    /// ToolResult synthesis at the exact same lifecycle coordinate.
    pending_calls: std::collections::HashMap<String, lifecycle::PendingCall>,
    /// call_id → tool_name for ToolResults from checkpoint-gated tools (shell/write/
    /// apply_patch), kept for the lifetime of the turn (never evicted) so a later
    /// Replace targeting that call_id can still be recognized as needing a checkpoint
    /// stamp — pending_calls itself is popped as soon as the ToolResult arrives.
    completed_mutations: std::collections::HashMap<String, String>,
    inflight: tokio::task::JoinSet<()>,
    spine_phase: SpinePhase,
    watchdog: Watchdog,
    /// Set on the first successfully-read signal of any kind. Distinguishes a
    /// brain that never got going (cold-start: pipe closes before it ever says
    /// anything — a benign empty turn, see `silent_brain_runs_to_process_exit_
    /// not_watchdog_kill`) from one that was genuinely mid-turn — pinged,
    /// called a tool, streamed a delta — and then vanished without a
    /// `Terminated` signal, which IS a failure worth surfacing.
    ever_active: bool,
}

impl RelayState {
    fn new() -> Self {
        Self {
            finalized_turn: None,
            brain_failed: false,
            pending_calls: std::collections::HashMap::new(),
            completed_mutations: std::collections::HashMap::new(),
            inflight: tokio::task::JoinSet::new(),
            spine_phase: SpinePhase::BrainPhase,
            watchdog: Watchdog::new(),
            ever_active: false,
        }
    }
}

/// Spawn `gait __brain` for one turn and relay its Nerve signals.
///
/// * `Impulse`       → `run_tool_as_spine` (spawned task) → `Feedback`
/// * `ArchiveOutput` → persist/bound an in-process tool result → `Feedback`
/// * `Append`        → `wire::append_with_id` (spine is sole log writer)
/// * `StreamForward` → `sink.item()` (REPL TUI rendering)
/// * `Terminated`    → parse `TurnResult`, `sink.end_turn()`, `sink.result()`, return
///
/// The spine is the sole gait-dir writer. Before spawning the brain, it performs
/// all startup disk writes (worktree, checkpoint, lock, SessionStart)
/// so the brain can run with `writable: []`. (session.json was dropped
/// per the design spec §2; SessionStart and the lock remain.) The brain's `run_turn` skips these
/// when `relay.is_some()`.
///
/// Returns `Ok(true)` if the brain signalled failure, `Ok(false)` on success.
#[allow(clippy::too_many_arguments)]
pub async fn relay_brain_turn(
    gait_dir: &Path,
    codebase: &Path,
    session_id: &str,
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
    fs_delegate: Option<std::sync::Arc<dyn crate::embed::FsDelegate>>,
) -> anyhow::Result<crate::proto::gait::log::v1::RunResult> {
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
        diag: DiagnosticSink::new(gait_dir, session_id),
    };

    // Sole owner of diagnostic.jsonl writes for this turn. Built once after
    // TurnCtx so the three call sites can use `&diag` without rebuilding the
    // path. See src/turn/diag_sink.rs for the gating/terminal discipline.
    let diag = DiagnosticSink::new(gait_dir, session_id);

    // Session-scoped git handle — holds only traces+checkpoints Arcs (no codebase
    // repo), so codebase mixing is structurally impossible.
    // Session-scoped git handle — null in ephemeral mode (no worktree/checkpoint/commit).
    let git = match mode {
        TurnMode::Persistent { .. } => {
            crate::turn::SessionGit::for_session(repos, gait_dir, session_id)
        }
        TurnMode::Ephemeral { .. } => crate::turn::SessionGit::null(),
    };

    // Persistent: worktree + checkpoint init + lock + SessionStart, exactly as before.
    // Ephemeral: an in-memory log seeded with the one-off input; no FS writes at all.
    let log = match mode {
        TurnMode::Persistent { .. } => {
            git.ensure_worktree()?;
            git.checkpoint_init(codebase)?;
            let log = crate::turn::SessionLog::open(gait_dir, session_id)?;
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
        TurnMode::Ephemeral { .. } => crate::turn::SessionLog::ephemeral(Vec::new()),
    };
    let trigger = match mode {
        TurnMode::Persistent { trigger } => trigger.clone(),
        TurnMode::Ephemeral { input } => input
            .iter()
            .filter_map(|event| event.body.clone())
            .map(|body| (body, None))
            .collect(),
    };
    let started = crate::turn::run_lifecycle::begin_turn(&log, &git, codebase, trigger)?;
    // Fold the durable selected inference before provider/credential binding.
    let events = log.events();
    // Same computation teardown (lifecycle.rs) does post-turn with its own
    // `events` snapshot — here it's pre-turn, so it names *this* turn's
    // number. It cannot change mid-turn (no RunComplete for this turn has
    // been logged yet), so compute it once and reuse it for every checkpoint
    // enrichment below rather than re-reading the log per ToolResult.
    let turn_n = started.turn_index;
    let mut profile = lifecycle::build_turn_profile(gait_dir, codebase, profile_inputs);
    {
        use crate::session::retrace::{active_inference, active_mutations, apply_cascade};
        let mut muts = active_mutations(&events);
        apply_cascade(&mut muts, &events);
        if let Some(inference) = active_inference(&events, &muts) {
            profile.overlay_inference(&inference)?;
        }
    }
    let credential_lease = crate::auth::resolve_credential_lease_result(
        profile
            .model
            .split_once('/')
            .map(|(provider, _)| provider)
            .unwrap_or(""),
    )?;

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
            .stderr(std::process::Stdio::inherit());
        // Set up cascade group enrollment hooks (setsid for brain = first call).
        group
            .configure_enrolled(&mut std_cmd)
            .map_err(|e| anyhow::anyhow!("failed to configure brain enrollment: {e}"))?;
        // Convert to tokio Command (keeps all pre_exec hooks) and spawn async.
        let mut tokio_cmd = TokioCommand::from(std_cmd);
        let child = tokio_cmd
            .spawn()
            .map_err(|e| anyhow::anyhow!("failed to spawn __brain: {e}"))?;
        let rec = crate::diagnostic::DiagRecord::lifecycle(
            session_id,
            crate::diagnostic::Phase::TurnBegin,
            crate::diagnostic::PhaseEvent::Start,
            &format!(
                "brain_spawned pid={} sandbox_on={} stdout=nerve stderr=inherited",
                child.id().unwrap_or(0),
                sandbox_on
            ),
        );
        diag.debug(&rec.to_json_line());
        // Register brain pid so group can reap it and knows its pgid.
        group.register_pid(child.id().unwrap_or(0) as i32);
        child
    };
    let brain_pid = child.id().unwrap_or(0);
    // Capture the group handle now that the brain (leader) is realized.
    let _group_handle = group.handle(); // Future: pass to blocking subagents (Plan 4).

    let child_stdin = Arc::new(Mutex::new(child.stdin.take().expect("piped stdin")));
    let child_stdout = child.stdout.take().expect("piped stdout");
    let mut nerve_reader = nerve::NerveReader::new(child_stdout);

    // ── BrainInit: push config + full log + git so the brain reads zero disk. ──
    // The caller appended this turn's user input before relay_brain_turn, and the
    // SessionStart write above ran, so `events` is complete.
    {
        use crate::proto::gait::sandbox::v1::{signal, BrainInit, DaemonStatus, GitStatus, Signal};
        use tokio::io::AsyncWriteExt;
        let cfg = crate::config::load_config(gait_dir);
        let config_json = serde_json::to_string(&cfg).unwrap_or_else(|_| "{}".to_string());
        let head_hash = repos.codebase.head_hash(codebase).unwrap_or_default();
        let dirty_files = repos.codebase.dirty_files(codebase);
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
            credential_json: credential_lease
                .as_ref()
                .map(|lease| credential_to_wire_json(&lease.credential))
                .transpose()?
                .unwrap_or_default(),
        };
        let sig = Signal {
            corr_id: 0,
            body: Some(signal::Body::BrainInit(init)),
        };
        let mut w = child_stdin.lock().await;
        nerve::write_signal(&mut *w, &sig).await?;
        w.flush().await?;
    }

    let mut state = RelayState::new();

    sink.begin_turn();
    loop {
        let read = nerve_reader.read_signal();
        let timed = tokio::time::timeout(
            std::time::Duration::from_millis(state.watchdog.ping_ms()),
            read,
        )
        .await;
        let signal = match timed {
            Ok(Ok(s)) => {
                state.ever_active = true;
                s
            }
            Ok(Err(e)) => {
                // Pipe closed without a Terminated signal. If the brain never got
                // going at all (cold start — see `silent_brain_runs_to_process_
                // exit_not_watchdog_kill`), this is a benign empty turn: keep the
                // old silent `break`. But if it was genuinely mid-turn (pinged,
                // called a tool, streamed something) and then vanished — crash,
                // OOM-kill, any other unexpected exit — that must go through
                // emit_terminal_failure (not a bare `break`) so brain_failed=true
                // and teardown's synthesize_aborted_if_missing has something to
                // synthesize from; otherwise a real failure silently vanishes as
                // a false "success".
                if state.ever_active {
                    let error = e.to_string();
                    let class = if error.contains("nerve: unsupported version") {
                        "nerve_protocol_error"
                    } else if error.contains("read version")
                        || error.contains("read len")
                        || error.contains("read signal bytes")
                        || error.contains("decode Signal")
                    {
                        "nerve_read_error"
                    } else {
                        "brain_pipe_error"
                    };
                    lifecycle::emit_terminal_failure(
                        &log,
                        &diag,
                        &mut state.brain_failed,
                        state.watchdog.phase(),
                        &format!(
                            "{class} brain_pid={brain_pid} phase={:?} error={error}",
                            state.watchdog.phase()
                        ),
                    );
                }
                break;
            }
            Err(_) => {
                // read timed out → fall through to the watchdog check
                if let Some(reason) = state.watchdog.breach(state.spine_phase) {
                    lifecycle::emit_terminal_failure(
                        &log,
                        &diag,
                        &mut state.brain_failed,
                        state.watchdog.phase(),
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
                    let gd = gait_dir.to_path_buf();
                    let sid = session_id.to_string();
                    state.inflight.spawn(async move {
                        let args = handlers::agent::parse_spawn_args(&impulse);
                        let feedback = handlers::agent::handle_spawn_agent(args, ctx).await;
                        let feedback = handlers::output::finalize_feedback(
                            feedback,
                            &gd,
                            &sid,
                            "spawn_agent",
                            None,
                            crate::types::OutputPresentation::ModelPreview,
                        );
                        handlers::reply_feedback(stdin_writer, corr_id, feedback).await;
                    });
                } else {
                    let cfg = sandbox_cfg.clone();
                    let gd = gait_dir.to_path_buf();
                    let cb = codebase.to_path_buf();
                    let sid = session_id.to_string();
                    let exe_c = exe.to_path_buf();
                    let grp = Arc::clone(&group);
                    let del = fs_delegate.clone();
                    state.inflight.spawn(async move {
                        let feedback = run_tool_as_spine(
                            impulse, &cfg, &gd, &cb, &sid, sandbox_on, &exe_c, grp, del,
                        )
                        .await;
                        handlers::reply_feedback(stdin_writer, corr_id, feedback).await;
                    });
                }
            }
            Some(signal::Body::ArchiveOutput(request)) => {
                let stdin_writer = Arc::clone(&child_stdin);
                let gd = gait_dir.to_path_buf();
                let sid = session_id.to_string();
                state.inflight.spawn(async move {
                    let feedback = handlers::output::archive_output_feedback(request, &gd, &sid);
                    handlers::reply_feedback(stdin_writer, corr_id, feedback).await;
                });
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
                                state.pending_calls.insert(
                                    tc.call_id.clone(),
                                    lifecycle::PendingCall {
                                        tool_name: tc.name.clone(),
                                        step: evt.step,
                                    },
                                );
                                if state.pending_calls.len() == 1 {
                                    state.spine_phase = SpinePhase::HandsPhase;
                                }
                            }
                            Some(payload::Kind::ToolResult(tr)) => {
                                if let Some(pending) = state.pending_calls.remove(&tr.call_id) {
                                    if matches!(
                                        pending.tool_name.as_str(),
                                        "shell" | "write" | "apply_patch"
                                    ) {
                                        state
                                            .completed_mutations
                                            .insert(tr.call_id.clone(), pending.tool_name);
                                    }
                                }
                                if state.pending_calls.is_empty() {
                                    state.spine_phase = SpinePhase::BrainPhase;
                                }
                            }
                            _ => {}
                        }
                    }
                    // Stamp a shadow-git checkpoint SHA onto ToolResults (direct or
                    // Replace-wrapped) from shell/write/apply_patch. For a direct
                    // ToolResult, pending_calls already had this call_id removed by
                    // the match arm above, but completed_mutations was just
                    // populated with it in that same arm — the `.or_else` fallback
                    // covers it. For a Replace (apply_output_filter's rewrite of an
                    // earlier ToolResult), pending_calls never had this event's
                    // call_id (a Replace carries no ToolCall/ToolResult kind of its
                    // own to trigger that bookkeeping) — only completed_mutations,
                    // populated when the original ToolResult came through earlier
                    // in the turn, has it.
                    enrich_checkpoint(
                        &mut evt,
                        |call_id| {
                            state
                                .pending_calls
                                .get(call_id)
                                .map(|pending| pending.tool_name.clone())
                                .or_else(|| state.completed_mutations.get(call_id).cloned())
                        },
                        &git,
                        codebase,
                        started.run.index,
                        turn_n,
                    );
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
                state.brain_failed = t.failed;
                // Brain-reported failure: the brain already appended its own Error
                // event to log.jsonl via Signal::Append before sending Terminated,
                // so we must NOT double-write the event here. We do write a
                // diagnostic record when GAIT_DEBUG is enabled.
                if t.failed {
                    let rec = crate::diagnostic::DiagRecord::terminal(
                        session_id,
                        state.watchdog.phase(),
                        "brain reported failure via Terminated signal",
                    );
                    diag.terminal(&rec.to_json_line());
                }
                state.finalized_turn = t.turn;
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
                        #[allow(clippy::print_stderr)]
                        // tracked debt: sealed-brain stderr bypasses the Nerve/event channel — see docs/superpowers/plans/2026-07-05-repl-stderr-race-fix.md
                        {
                            eprintln!("[spine] warning: spawn_agent cascade-group handle unavailable; subagent will not be enrolled in cascade group");
                        }
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
                idempotency_key,
            })) => {
                // Route subagent completion through notify_parent (one-shot lock acquire,
                // dedup, write, commit, spawn_wake). Replaces the 20-retry open_retrying
                // path. ParentBusy → the live turn's per-step poll surfaces it next step.
                let c = crate::session::completion::Completion {
                    payload: crate::proto::events::agent_update(
                        from_session.clone(),
                        &status,
                        summary.clone(),
                        None,
                        idempotency_key.clone(),
                    ),
                    idempotency_key: idempotency_key.clone(),
                };
                match crate::session::completion::notify_parent(
                    gait_dir,
                    repos,
                    &target_session,
                    &c,
                ) {
                    crate::session::completion::NotifyOutcome::Woke(req) => {
                        crate::launch::spawn_wake(&req);
                    }
                    crate::session::completion::NotifyOutcome::ParentBusy => {}
                    crate::session::completion::NotifyOutcome::AlreadySurfaced => {}
                    crate::session::completion::NotifyOutcome::NoParent => {
                        #[allow(clippy::print_stderr)]
                        // tracked debt: sealed-brain stderr bypasses the Nerve/event channel — see docs/superpowers/plans/2026-07-05-repl-stderr-race-fix.md
                        {
                            eprintln!(
                                "[spine] warning: parent session {target_session} not found for notify"
                            );
                        }
                    }
                }
                // No Feedback needed — one-way signal like Append.
            }
            Some(signal::Body::Diagnostic(diag_msg)) => {
                state.watchdog.observe(
                    crate::diagnostic::phase_from_str(&diag_msg.phase),
                    diag_msg.elapsed_ms,
                );
                diag.ping(&diag_msg.record_json);
            }
            _ => {}
        }

        // Watchdog: runs after every signal (in addition to every read-timeout).
        if let Some(reason) = state.watchdog.breach(state.spine_phase) {
            lifecycle::emit_terminal_failure(
                &log,
                &diag,
                &mut state.brain_failed,
                state.watchdog.phase(),
                reason,
            );
            let _ = group.kill_all();
            break;
        }
    }
    lifecycle::teardown(
        sink,
        state.inflight,
        state.pending_calls,
        child_stdin,
        group,
        child,
        git,
        codebase,
        session_id,
        &log,
        &started,
        state.finalized_turn,
        state.brain_failed,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::gait::log::v1::ToolResult;

    /// Mirrors the bookkeeping done in the `Append` arm's `ToolResult` match: pops
    /// the originating tool name from `pending_calls` and, if it is one of the
    /// checkpoint-gated tools (shell/write/apply_patch), records it in
    /// `completed_mutations` so a later `Replace` targeting that call_id can still
    /// be recognized. Also updates `spine_phase` based on whether any in-flight
    /// ToolCalls remain — identical to the inline behavior in the handler.
    fn record_tool_result(state: &mut RelayState, tr: &ToolResult) {
        if let Some(pending) = state.pending_calls.remove(&tr.call_id) {
            if matches!(
                pending.tool_name.as_str(),
                "shell" | "write" | "apply_patch"
            ) {
                state
                    .completed_mutations
                    .insert(tr.call_id.clone(), pending.tool_name);
            }
        }
        if state.pending_calls.is_empty() {
            state.spine_phase = SpinePhase::BrainPhase;
        }
    }

    #[test]
    fn tool_result_for_mutating_tool_is_tracked() {
        let mut state = RelayState::new();
        state.pending_calls.insert(
            "call-1".to_string(),
            lifecycle::PendingCall {
                tool_name: "shell".to_string(),
                step: Some(3),
            },
        );
        let tr = ToolResult {
            call_id: "call-1".to_string(),
            ..Default::default()
        };
        record_tool_result(&mut state, &tr);
        assert_eq!(
            state.completed_mutations.get("call-1"),
            Some(&"shell".to_string())
        );
    }

    #[test]
    fn tool_result_for_read_tool_is_not_tracked() {
        let mut state = RelayState::new();
        state.pending_calls.insert(
            "call-2".to_string(),
            lifecycle::PendingCall {
                tool_name: "read".to_string(),
                step: Some(4),
            },
        );
        let tr = ToolResult {
            call_id: "call-2".to_string(),
            ..Default::default()
        };
        record_tool_result(&mut state, &tr);
        assert!(!state.completed_mutations.contains_key("call-2"));
    }
}

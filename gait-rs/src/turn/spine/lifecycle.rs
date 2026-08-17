//! Turn profile construction, brain argv, and the load-bearing post-loop
//! teardown sequence (drain → orphan-repair → EOF → reap → checkpoint → commit).

use crate::git::codebase::CodebaseRepo;
use std::path::Path;
use std::sync::Arc;

// ── ProfileInputs + build_turn_profile ───────────────────────────────────────

/// Inputs the spine needs to build the turn's `Profile` (parsed by the CLI/REPL
/// caller, since gait-core cannot parse clap args). Mirrors the brain's old
/// agent/model/allow/deny handling.
#[derive(Clone)]
pub struct ProfileInputs {
    pub agent_name: Option<String>,
    pub inference_override: Option<String>,
    /// Already validated immutable snapshot, used by ephemeral consumers that
    /// resolve against a real workspace but keep session storage in scratch.
    /// Mutually exclusive with `inference_override`.
    pub resolved_inference: Option<crate::proto::gait::log::v1::Inference>,
    /// True only when the named inference came from an explicit caller choice.
    pub explicit_inference_override: bool,
    pub allow: Vec<String>,
    pub deny: Vec<String>,
    pub result_schema: Option<serde_json::Value>,
    /// Ephemeral one-off: brain registers only `structured_done` (no hands).
    pub hands_free: bool,
}

/// Selects whether a turn persists to the gait-dir. `Persistent` is every
/// production/test caller; `Ephemeral` is the `gait -e` one-off, which serves the
/// brain's input from memory and writes nothing to disk.
pub enum TurnMode {
    Persistent {
        trigger: Vec<crate::proto::gait::log::v1::event::Body>,
    },
    Ephemeral {
        input: Vec<crate::proto::gait::log::v1::Event>,
    },
}

/// Build the turn's `Profile` on the spine (disk scan: agent file, system-prompt
/// layers, skills/mcp/plugins). Folds CLI allow/deny into config first, mirroring
/// the brain's previous in-process logic.
pub fn build_turn_profile(
    gait_dir: &std::path::Path,
    codebase: &std::path::Path,
    inputs: &ProfileInputs,
) -> crate::profile::Profile {
    use crate::config::resolver::{ConfigResolver, Overrides};
    let loaded = crate::config::load_config(gait_dir);
    // consumer layer: nothing (gait.jsonc is already baked into `loaded`)
    // per-call layer: CLI allow/deny overrides (only when non-empty, to preserve additive semantics)
    let per_call = Overrides {
        allow: if inputs.allow.is_empty() {
            None
        } else {
            // merge: start from loaded allow and add any new patterns
            let mut merged = loaded.allow.clone();
            for p in &inputs.allow {
                if !merged.contains(p) {
                    merged.push(p.clone());
                }
            }
            Some(merged)
        },
        deny: if inputs.deny.is_empty() {
            None
        } else {
            let mut merged = loaded.deny.clone();
            for p in &inputs.deny {
                if !merged.contains(p) {
                    merged.push(p.clone());
                }
            }
            Some(merged)
        },
        ..Overrides::empty()
    };
    let mut cfg = ConfigResolver::new(loaded)
        .with(Overrides::empty())
        .with(per_call)
        .resolve();
    if let Some(s) = &inputs.result_schema {
        cfg.result_schema = Some(s.clone());
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
    assert!(
        inputs.resolved_inference.is_none() || inputs.inference_override.is_none(),
        "resolved inference and named inference override are mutually exclusive"
    );
    let mut profile = match &inputs.resolved_inference {
        Some(inference) => crate::profile::Profile::from_resolved_inference(
            &cfg,
            gait_dir,
            Some(codebase),
            agent_path.as_deref(),
            inference,
        ),
        None => crate::profile::Profile::from_config_and_agent(
            &cfg,
            gait_dir,
            Some(codebase),
            agent_path.as_deref(),
            inputs.inference_override.clone(),
        ),
    };
    profile.hands_free = inputs.hands_free;
    profile
}

// ── brain_argv ────────────────────────────────────────────────────────────────

/// Build the argv passed to `gait __brain` — the single source of truth for the
/// brain's command line. The `"__brain"` role token itself is prepended by the
/// spawn in `relay_brain_turn`; no consumer constructs any of this.
///
/// Only flags the brain actually reads from argv are emitted: `--gait-dir`,
/// `--codebase`, `--session`, `--auto`, `--max-iter`, `--allow`, `--deny`, and
/// the sandbox flag. The profile (model/agent/result-schema) reaches the brain
/// via the `BrainInit` frame, not argv.
pub(crate) fn brain_argv(
    gait_dir: &Path,
    codebase: &Path,
    session_id: &str,
    auto: bool,
    max_iter: Option<usize>,
    sandbox_on: bool,
    profile: &ProfileInputs,
) -> Vec<String> {
    crate::launch::BrainLaunch {
        gait_dir: gait_dir.to_path_buf(),
        codebase: codebase.to_path_buf(),
        session_id: session_id.to_string(),
        auto,
        max_iter,
        sandbox_on,
        allow: profile.allow.clone(),
        deny: profile.deny.clone(),
    }
    .to_argv()
}

/// Give in-flight Impulse-handler tasks (spawn_agent and every other tool
/// exec — see the `tokio::spawn` sites in `relay_brain_turn`) up to `grace`
/// to finish naturally, then abort whatever's still running. Called once,
/// right after the read loop exits (EOF / Terminated / watchdog), and BEFORE
/// `reap_brain`: without this, a tool call that's still mid-flight when the
/// loop breaks can keep running — spawning subagent processes and writing
/// to the gait-dir — after the caller has already been told the turn is done.
///
/// `abort_all` cancels the *task* at its next await point but does not itself
/// kill any OS process a task was waiting on (e.g. `run_tool_as_spine`'s hand
/// subprocess) — it is enrolled in the cascade `group` when spawned
/// (`configure_enrolled`/`register_pid` in
/// `run_tool_as_spine`), so the `reap_brain` → `group.kill_all()` call that
/// MUST run immediately after this one is what actually kills them. Calling
/// this after `reap_brain`, or not at all, is the bug this function exists to
/// fix — do not reorder.
async fn drain_inflight(mut set: tokio::task::JoinSet<()>, grace: std::time::Duration) {
    let _ = tokio::time::timeout(grace, async { while set.join_next().await.is_some() {} }).await;
    set.abort_all();
    while set.join_next().await.is_some() {}
}

/// Reap the brain subprocess at end-of-turn, then drop the tokio `Child` without
/// ever awaiting it.
///
/// We deliberately do NOT call `child.wait().await`. On macOS, tokio registers a
/// kqueue `EVFILT_PROC`/`NOTE_EXIT` filter lazily on the first poll of `wait()`,
/// and registering that filter against a pid that has *already* exited never
/// delivers an event — the await blocks **forever**. Every end-of-turn path
/// reaches here with the brain already gone (its stdout pipe closed → EOF) or
/// just signalled to die, so this was a live hang: the spine would sit alive at
/// 0% CPU indefinitely, never running the post-turn checkpoint/commit.
///
/// `group.kill_all()` is the correct reaper and is idempotent (a no-op if the
/// watchdog already reaped in-loop). Platform specifics:
/// - macOS: `kill_all` blocking-`waitpid`s the brain — correct for an
///   already-exited zombie. If the brain is still alive it blocks until a detached
///   SIGTERM→(300ms)→SIGKILL escalation thread makes it exit. `spawn_blocking`
///   keeps that potential block off the runtime workers.
/// - Linux: `kill_all` reaps with `WNOHANG`, so a still-live brain is reaped by
///   tokio's orphan reaper when `child` is dropped (the macOS kqueue defect that
///   motivates this whole helper does not apply on Linux).
///
/// Either way we move `child` in and drop it WITHOUT awaiting. Reaping before the
/// drop means tokio's SIGCHLD reaper can't race-steal the pid; the post-drop
/// `try_wait` then sees `ECHILD` (harmless), so there is no zombie leak.
async fn reap_brain(group: Arc<dyn crate::process::Containment>, child: tokio::process::Child) {
    let _ = tokio::task::spawn_blocking(move || group.kill_all()).await;
    drop(child);
}

// ── Helpers ──────────────────────────────────────────────────────────────────
/// THE single dual-emit site for terminal failures. Ordering is load-bearing:
/// the Error event lands in log.jsonl FIRST (transcript correctness + exit
/// parity via `brain_failed`), the DiagRecord in diagnostic.jsonl SECOND
/// (best-effort). Single-threaded sole writer ⇒ back-to-back.
/// Mirrors the orphan-tool repair append at spine.rs:631-636.
pub(crate) fn emit_terminal_failure(
    log: &crate::turn::SessionLog,
    diag: &crate::turn::DiagnosticSink,
    brain_failed: &mut bool,
    phase: crate::diagnostic::Phase,
    detail: &str,
) {
    // 1) Error event → log.jsonl FIRST (also sets brain_failed for exit parity).
    // Always use ErrorPhase::Llm and a descriptive prefix so the transcript
    // clearly names the timeout rather than mapping a diagnostic phase string.
    let body = crate::proto::events::error(format!("LLM timed out: {detail}"), "llm");
    let prev = log.last_event();
    let scope = prev
        .as_ref()
        .map(scope_of_previous_event)
        .unwrap_or(crate::proto::envelope::EventScope::Session);
    let evt = crate::proto::envelope::mint(body, prev.as_ref(), scope);
    log.relay_event(&evt).ok();
    *brain_failed = true;

    // 2) DiagRecord → diagnostic.jsonl SECOND (gated by DiagnosticSink).
    let rec = crate::diagnostic::DiagRecord::terminal(diag.session_id(), phase, detail);
    diag.terminal(&rec.to_json_line());
}

fn scope_of_previous_event(
    event: &crate::proto::gait::log::v1::Event,
) -> crate::proto::envelope::EventScope {
    match (event.run, event.turn, event.step) {
        (Some(run), Some(turn), Some(step)) => {
            crate::proto::envelope::EventScope::Step { run, turn, step }
        }
        (Some(run), Some(turn), None) => crate::proto::envelope::EventScope::Turn { run, turn },
        (Some(run), None, None) => crate::proto::envelope::EventScope::Run { run },
        _ => crate::proto::envelope::EventScope::Session,
    }
}

#[derive(Clone, Debug)]
pub(super) struct PendingCall {
    pub tool_name: String,
    pub step: Option<u32>,
}

fn append_orphan_tool_result(
    log: &crate::turn::SessionLog,
    run: u32,
    turn: u32,
    call_id: String,
    pending: PendingCall,
) -> anyhow::Result<()> {
    let step = pending.step.ok_or_else(|| {
        anyhow::anyhow!(
            "cannot repair orphan tool call {call_id}: originating ToolCall had no step scope"
        )
    })?;
    let msg = format!(
        "[LLM timed out before '{}' result was received]",
        pending.tool_name
    );
    log.append_body(
        crate::proto::events::tool_result(call_id, msg, None, None),
        crate::proto::envelope::EventScope::Step { run, turn, step },
    )?;
    Ok(())
}

/// Post-loop epilogue. The step order is load-bearing:
///   0. sink.end_turn().
///   1. drain in-flight tasks (grace, then abort) — must precede reap so a
///      task abort never races the cascade kill.
///   2. drop child_stdin (EOF to the brain's Nerve reader).
///   3. reap: group.kill_all(), drop Child WITHOUT awaiting (macOS kqueue
///      workaround — awaiting child.wait() hangs; see reap_brain).
///   4. synthesize orphan ToolResults for pending call_ids, collecting every
///      repair error before returning one.
///   5. git.checkpoint(codebase, ..) (best-effort).
///   6. git.commit(&msg) (message keyed on run_result kind).
///   7. sink.result(&result).
///
/// Steps 1–4 must complete before 5–6 so the transcript is final before commit.
/// Returns `brain_failed` (the value relay_brain_turn must return).
/// When the brain dies (crash / watchdog breach) without ever reaching
/// `finalize`, `run_result` stays `None` and no result is emitted on stdout.
/// Coalesce that missing result under `brain_failed` so the spine still
/// surfaces a typed terminal reason. The discriminator value is `"aborted"` —
/// distinct from the in-finalize `"error"` / `"max_steps"` reasons emitted by
/// `run_result_proto`, which the brain emits itself when it owns the turn.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn teardown(
    sink: &mut dyn crate::port::output::TurnSink,
    inflight: tokio::task::JoinSet<()>,
    mut pending_calls: std::collections::HashMap<String, PendingCall>,
    child_stdin: Arc<tokio::sync::Mutex<tokio::process::ChildStdin>>,
    group: Arc<dyn crate::process::Containment>,
    child: tokio::process::Child,
    git: crate::turn::SessionGit,
    codebase: &Path,
    session_id: &str,
    log: &crate::turn::SessionLog,
    started: &crate::turn::run_lifecycle::StartedTurn,
    finalized_turn: Option<crate::proto::gait::sandbox::v1::FinalizedTurn>,
    brain_failed: bool,
) -> anyhow::Result<crate::proto::gait::log::v1::RunResult> {
    sink.end_turn();

    // Bound how long an in-flight tool call (or spawn_agent handler) can keep
    // running after the read loop has already decided the
    // turn is over. Must run before reap_brain: this is the spine's own
    // in-process task leak, distinct from (and not covered by) reap_brain's
    // OS-process-group kill.
    drain_inflight(inflight, std::time::Duration::from_secs(3)).await;

    // Drop the brain's stdin writer to signal EOF — the brain's Nerve reader
    // task is blocking on read_signal from stdin and will exit on EOF, which
    // lets the brain's tokio runtime shut down and the process exit.
    drop(child_stdin);

    // Single authoritative reap for EVERY exit path (EOF / Terminated / watchdog).
    // Reaps via kill_all() + Child drop — never tokio's child.wait(), which is
    // fatal on macOS (see `reap_brain`).
    reap_brain(group, child).await;

    // Synthesize a ToolResult for each orphaned ToolCall (brain was killed before
    // it could emit results). Tools that completed already have real results in the
    // log; only call_ids still in pending_calls are orphans. Reap first: a failed
    // log append must never let the brain subprocess outlive its turn. Attempt all
    // repairs so one bad append cannot abandon the remaining call IDs.
    let mut repair_errors = Vec::new();
    for (call_id, pending) in pending_calls.drain() {
        if let Err(error) =
            append_orphan_tool_result(log, started.run.index, started.turn_index, call_id, pending)
        {
            repair_errors.push(error);
        }
    }
    if !repair_errors.is_empty() {
        anyhow::bail!("orphan repair failed: {repair_errors:?}");
    }

    let draft = finalized_turn.unwrap_or_else(|| crate::proto::gait::sandbox::v1::FinalizedTurn {
        outcome: crate::proto::gait::sandbox::v1::FinalizedTurnOutcome::Failed as i32,
        failure_reason: "aborted".into(),
        message: "brain exited without a finalized turn".into(),
        ..Default::default()
    });
    let turn = durable_turn_result(log, started, draft, brain_failed);
    let turn_event = log.append_body(
        crate::proto::events::turn_result(turn.clone()),
        crate::proto::envelope::EventScope::Turn {
            run: started.run.index,
            turn: started.turn_index,
        },
    )?;

    use crate::proto::gait::log::v1 as pb;
    use pb::turn_result::Outcome;
    match turn
        .outcome
        .as_ref()
        .expect("durable turn always has outcome")
    {
        Outcome::Pause(_) => {}
        outcome => {
            let checkpoint = git
                .checkpoint(
                    codebase,
                    crate::git::checkpoint::CheckpointBoundary::RunFinal {
                        run: started.run.index,
                    },
                )
                .ok()
                .filter(|sha| !sha.is_empty());
            let dirty_files = crate::git::codebase::GitCodebaseRepo.dirty_files(codebase);
            let final_state = pb::RunEndState {
                codebase: Some(pb::CodebaseState {
                    hash: crate::git::codebase::GitCodebaseRepo
                        .head_hash(codebase)
                        .unwrap_or_default(),
                    dirty: !dirty_files.is_empty(),
                    dirty_files,
                }),
                checkpoint: checkpoint.clone(),
            };
            let scope = crate::proto::envelope::EventScope::Run {
                run: started.run.index,
            };
            match outcome {
                Outcome::Done(_) => {
                    let changes = match (
                        started.run.baseline_checkpoint.as_deref(),
                        checkpoint.as_deref(),
                    ) {
                        (Some(baseline), Some(final_checkpoint)) => {
                            git.diff_between(baseline, final_checkpoint).ok().flatten()
                        }
                        _ => None,
                    };
                    let summary = crate::turn::run_summary::build(
                        &log.events(),
                        started.run.index,
                        started.run.start_seq,
                        turn_event.seq,
                        changes,
                    );
                    log.append_body(
                        crate::proto::events::run_complete(pb::RunComplete {
                            final_state: Some(final_state),
                            summary: Some(summary),
                        }),
                        scope,
                    )?;
                }
                Outcome::Cancelled(_) => {
                    log.append_body(
                        crate::proto::events::run_cancelled(pb::RunCancelled {
                            final_state: Some(final_state),
                        }),
                        scope,
                    )?;
                }
                Outcome::Failed(_) => {
                    log.append_body(
                        crate::proto::events::run_failed(pb::RunFailed {
                            final_state: Some(final_state),
                        }),
                        scope,
                    )?;
                }
                Outcome::Pause(_) => unreachable!(),
            }
        }
    }

    let result = crate::turn::run_lifecycle::fold_run_result(&log.events(), started.run.index)?;
    let _ = git.commit(&crate::git::build_trace_message(
        &format!(
            "session({session_id}): run {} turn {}",
            started.run.index, started.turn_index
        ),
        &crate::git::TraceTrailers {
            phase: if brain_failed { "failed" } else { "turn" }.into(),
            turn: Some(started.turn_index),
            codebase: codebase.display().to_string(),
            codebase_hash: crate::git::codebase::GitCodebaseRepo
                .head_hash(codebase)
                .unwrap_or_default(),
            ..Default::default()
        },
    ));
    sink.result(&result);
    Ok(result)
}

fn durable_turn_result(
    log: &crate::turn::SessionLog,
    started: &crate::turn::run_lifecycle::StartedTurn,
    draft: crate::proto::gait::sandbox::v1::FinalizedTurn,
    brain_failed: bool,
) -> crate::proto::gait::log::v1::TurnResult {
    use crate::proto::gait::log::v1 as pb;
    use crate::proto::gait::sandbox::v1::FinalizedTurnOutcome as DraftOutcome;
    use std::collections::BTreeMap;

    let events = log.events();
    let mut grouped = BTreeMap::<u32, Vec<&pb::Event>>::new();
    for event in events.iter().filter(|event| {
        event.run == Some(started.run.index)
            && event.turn == Some(started.turn_index)
            && event.step.is_some()
    }) {
        grouped.entry(event.step.unwrap()).or_default().push(event);
    }
    let mut usage = pb::TurnTokenUsage::default();
    let steps: Vec<pb::StepResult> = grouped
        .into_iter()
        .map(|(index, events)| {
            let mut step_usage = pb::StepTokenUsage::default();
            for event in &events {
                if let Some(pb::payload::Kind::LlmCallCompleted(done)) =
                    crate::turn::event_kind(event)
                {
                    if let Some(delta) = &done.usage {
                        step_usage.input += delta.input;
                        step_usage.output += delta.output;
                        step_usage.cache_read += delta.cache_read;
                        step_usage.cache_write += delta.cache_write;
                    }
                }
            }
            usage.input += step_usage.input;
            usage.output += step_usage.output;
            usage.cache_read += step_usage.cache_read;
            usage.cache_write += step_usage.cache_write;
            let first = events.first().map_or(started.started_ms, |event| event.ts);
            let last = events.last().map_or(first, |event| event.ts);
            pb::StepResult {
                index,
                usage: Some(step_usage),
                event_ids: events.iter().map(|event| event.id.clone()).collect(),
                timing: Some(pb::ExecutionTiming {
                    started_ms: first,
                    ended_ms: Some(last),
                    active_duration: Some(duration_ms(last.saturating_sub(first))),
                }),
            }
        })
        .collect();
    let ended_ms = now_ms();
    let raw = DraftOutcome::try_from(draft.outcome).unwrap_or(DraftOutcome::Failed);
    let outcome = if brain_failed {
        pb::turn_result::Outcome::Failed(pb::TurnFailed {
            reason: if draft.failure_reason.is_empty() {
                "brain_failed".into()
            } else {
                draft.failure_reason.clone()
            },
            message: draft.message.clone(),
        })
    } else {
        match raw {
            DraftOutcome::TextDone => pb::turn_result::Outcome::Done(pb::TurnDone {
                output: Some(pb::turn_done::Output::Text(pb::TextDone {
                    message: draft.message.clone(),
                })),
            }),
            DraftOutcome::StructuredDone => pb::turn_result::Outcome::Done(pb::TurnDone {
                output: Some(pb::turn_done::Output::Structured(pb::StructuredDone {
                    message: draft.message.clone(),
                    result: draft.result.clone(),
                })),
            }),
            DraftOutcome::Cancelled => pb::turn_result::Outcome::Cancelled(pb::TurnCancelled {
                signal_event_id: last_event_id(&events, |kind| {
                    matches!(kind, pb::payload::Kind::CancellationSignal(_))
                })
                .unwrap_or_default(),
            }),
            DraftOutcome::Failed | DraftOutcome::Unspecified => {
                pb::turn_result::Outcome::Failed(pb::TurnFailed {
                    reason: draft.failure_reason.clone(),
                    message: draft.message.clone(),
                })
            }
            pause => pb::turn_result::Outcome::Pause(pb::TurnPause {
                reason: match pause {
                    DraftOutcome::Clarify => pb::PauseReason::Clarify as i32,
                    DraftOutcome::Inform => pb::PauseReason::Inform as i32,
                    DraftOutcome::Permissions => pb::PauseReason::Permissions as i32,
                    DraftOutcome::SandboxUnlock => pb::PauseReason::SandboxUnlock as i32,
                    _ => pb::PauseReason::Unspecified as i32,
                },
                request_event_id: last_event_id(&events, |kind| {
                    matches!(
                        kind,
                        pb::payload::Kind::Clarify(_)
                            | pb::payload::Kind::Inform(_)
                            | pb::payload::Kind::Permissions(_)
                            | pb::payload::Kind::SandboxUnlock(_)
                    )
                })
                .unwrap_or_default(),
            }),
        }
    };
    let blocks = if draft.blocks.is_empty() {
        events
            .iter()
            .filter_map(|event| match crate::turn::event_kind(event) {
                Some(pb::payload::Kind::AssistantText(text))
                    if event.run == Some(started.run.index)
                        && event.turn == Some(started.turn_index) =>
                {
                    Some(text.content.clone())
                }
                _ => None,
            })
            .collect()
    } else {
        draft.blocks
    };
    pb::TurnResult {
        turn_id: started.turn_id.clone(),
        index: started.turn_index,
        steps,
        usage: Some(usage),
        blocks,
        timing: Some(pb::ExecutionTiming {
            started_ms: started.started_ms,
            ended_ms: Some(ended_ms),
            active_duration: Some(duration_ms(ended_ms.saturating_sub(started.started_ms))),
        }),
        outcome: Some(outcome),
    }
}

fn last_event_id(
    events: &[crate::proto::gait::log::v1::Event],
    predicate: impl Fn(&crate::proto::gait::log::v1::payload::Kind) -> bool,
) -> Option<String> {
    events.iter().rev().find_map(|event| {
        let kind = crate::turn::event_kind(event)?;
        predicate(&kind).then(|| event.id.clone())
    })
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis() as u64)
}

fn duration_ms(ms: u64) -> pbjson_types::Duration {
    pbjson_types::Duration {
        seconds: (ms / 1000) as i64,
        nanos: ((ms % 1000) * 1_000_000) as i32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression for the spine end-of-turn hang: `reap_brain` must return promptly
    /// even when the brain has already exited. The old teardown called
    /// `child.wait().await` here, which on macOS hangs forever for an already-exited
    /// pid (kqueue `EVFILT_PROC` never fires) — the spine sat alive at 0% CPU and
    /// never ran the post-turn checkpoint/commit. The fix reaps via
    #[tokio::test]
    async fn drain_inflight_lets_fast_tasks_finish_normally() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        let mut set = tokio::task::JoinSet::new();
        let finished = Arc::new(AtomicBool::new(false));
        let f = finished.clone();
        set.spawn(async move {
            f.store(true, Ordering::SeqCst);
        });
        drain_inflight(set, std::time::Duration::from_millis(200)).await;
        assert!(
            finished.load(Ordering::SeqCst),
            "fast task should have completed, not been aborted"
        );
    }

    #[tokio::test]
    async fn drain_inflight_aborts_tasks_that_outlive_the_grace_period() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        let mut set = tokio::task::JoinSet::new();
        let reached_end = Arc::new(AtomicBool::new(false));
        let r = reached_end.clone();
        set.spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            r.store(true, Ordering::SeqCst);
        });
        let start = std::time::Instant::now();
        drain_inflight(set, std::time::Duration::from_millis(50)).await;
        assert!(
            start.elapsed() < std::time::Duration::from_secs(2),
            "drain_inflight should abort stragglers promptly, took {:?}",
            start.elapsed()
        );
        assert!(
            !reached_end.load(Ordering::SeqCst),
            "long task should have been aborted before completing"
        );
    }

    /// `group.kill_all()` (blocking waitpid) and never awaits the tokio `Child`.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn reap_brain_does_not_hang_on_already_exited_child() {
        // Build the child exactly as the spine does: std::Command → configure_enrolled
        // → tokio Command, so it joins a real cascade group the way the brain does.
        let group = crate::process::establish(None);
        let mut std_cmd = std::process::Command::new("true"); // exits 0 immediately
        group
            .configure_enrolled(&mut std_cmd)
            .expect("configure enrolled");
        let mut tokio_cmd = tokio::process::Command::from(std_cmd);
        let child = tokio_cmd.spawn().expect("spawn child");
        let pid = child.id().expect("pid") as i32;
        group.register_pid(pid);

        // Let it fully exit → it is now a zombie / already-reaped pid, the exact
        // condition that wedged `child.wait().await`.
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;

        // Must complete well within the bound. On the old code this would block
        // indefinitely on macOS and the timeout would trip.
        let res = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            reap_brain(group.clone(), child),
        )
        .await;
        assert!(res.is_ok(), "reap_brain hung on an already-exited child");

        // Brain is reaped — no zombie leak (second waitpid → ECHILD).
        let r = unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) };
        assert_eq!(r, -1, "brain pid should already be reaped (ECHILD)");
    }

    // `brain_argv_is_canonical_and_has_no_role_token` and
    // `brain_argv_unsandboxed_emits_no_sandbox` moved to
    // `launch::tests` (spec-00-footguns.md item 4) — they now assert
    // against `BrainLaunch::to_argv()` directly, plus a round-trip parse
    // through `BrainArgs` that these two didn't have before.

    #[test]
    fn build_turn_profile_threads_result_schema() {
        let ws = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(ws.path().join("inferences/mock")).unwrap();
        std::fs::write(
            ws.path().join("inferences/mock/INFERENCE.yaml"),
            "model: mock/model\ncompaction_threshold_tokens: 128000\n",
        )
        .unwrap();
        std::fs::write(ws.path().join("gait.jsonc"), r#"{"inference":"mock"}"#).unwrap();
        let schema = serde_json::json!({"type":"object","required":["answer"]});
        let inputs = ProfileInputs {
            agent_name: None,
            inference_override: None,
            resolved_inference: None,
            explicit_inference_override: false,
            allow: vec![],
            deny: vec![],
            result_schema: Some(schema.clone()),
            hands_free: false,
        };
        let p = build_turn_profile(ws.path(), ws.path(), &inputs);
        assert_eq!(p.result_schema, Some(schema));
    }

    #[test]
    fn build_turn_profile_folds_cli_allow() {
        let dir = tempfile::TempDir::new().unwrap();
        // Write a minimal gait.jsonc so load_config picks up a known model.
        std::fs::create_dir_all(dir.path().join("inferences/mock")).unwrap();
        std::fs::write(
            dir.path().join("inferences/mock/INFERENCE.yaml"),
            "model: anthropic/x\ncompaction_threshold_tokens: 128000\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("gait.jsonc"), r#"{"inference":"mock"}"#).unwrap();
        let inputs = ProfileInputs {
            agent_name: None,
            inference_override: None,
            resolved_inference: None,
            explicit_inference_override: false,
            allow: vec!["read".into()],
            deny: vec![],
            result_schema: None,
            hands_free: false,
        };
        let p = build_turn_profile(dir.path(), dir.path(), &inputs);
        assert_eq!(p.model, "anthropic/x");
        assert!(p.allow.iter().any(|a| a == "read"));
    }

    #[test]
    #[should_panic(expected = "no inference")]
    fn build_turn_profile_panics_without_config_or_override() {
        let dir = tempfile::TempDir::new().unwrap();
        // No gait.jsonc — load_config falls back to GaitConfig::default() which has model: None.
        let inputs = ProfileInputs {
            agent_name: None,
            inference_override: None,
            resolved_inference: None,
            explicit_inference_override: false,
            allow: vec![],
            deny: vec![],
            result_schema: None,
            hands_free: false,
        };
        // Should panic: no model in config and no model_override supplied.
        let _ = build_turn_profile(dir.path(), dir.path(), &inputs);
    }

    /// A cancelled turn with an orphan ToolCall leaves a synthesized ToolResult
    /// in the log, so retrace::messages reconstructs with no orphan.
    ///
    /// This test also puts a turn-scoped event after the call. The repair must use
    /// the call's captured step rather than inventing step 0 from the log tail.
    #[test]
    fn cancelled_turn_synthesizes_tool_result_for_orphan_call_id() {
        use crate::proto::envelope::EventScope;
        use crate::proto::gait::log::v1::{event, payload};
        use crate::session::retrace;

        let log = crate::turn::SessionLog::ephemeral(Vec::new());

        // 1. Write a ToolCall (simulating what the brain emits mid-turn).
        let call_id = "call-orphan-1".to_string();
        let tc_body = crate::proto::events::tool_call(
            call_id.clone(),
            "read".into(),
            &serde_json::json!({"file_path": "foo.rs"}),
        );
        log.append_body(
            tc_body,
            EventScope::Step {
                run: 2,
                turn: 3,
                step: 7,
            },
        )
        .expect("append tool_call");
        log.append_body(
            crate::proto::events::error("turn ending".into(), "coordinator"),
            EventScope::Turn { run: 2, turn: 3 },
        )
        .expect("append turn-scoped tail");

        // 2. Simulate cancel using the call metadata captured by the relay.
        append_orphan_tool_result(
            &log,
            2,
            3,
            call_id.clone(),
            PendingCall {
                tool_name: "read".into(),
                step: Some(7),
            },
        )
        .expect("append synthesized tool_result");

        // 3. Assert: the last event is a ToolResult with the correct call_id.
        let events = log.events();
        assert_eq!(events.len(), 3, "log must have exactly 3 events");
        let last = events.last().unwrap();
        assert_eq!(last.run, Some(2));
        assert_eq!(last.turn, Some(3));
        assert_eq!(last.step, Some(7));
        match last.body.as_ref() {
            Some(event::Body::Payload(p)) => match p.kind.as_ref() {
                Some(payload::Kind::ToolResult(tr)) => {
                    assert_eq!(
                        tr.call_id, call_id,
                        "synthesized ToolResult must match call_id"
                    );
                    assert!(
                        tr.output.contains("LLM timed out") && tr.output.contains("read"),
                        "output must name the tool and the cause; got: {}",
                        tr.output
                    );
                }
                other => panic!("expected ToolResult, got {other:?}"),
            },
            other => panic!("expected Payload body, got {other:?}"),
        }

        // 4. Assert: retrace::messages reconstructs with no orphan tool calls.
        // Without the synthesized ToolResult, retrace would produce a ToolCall
        // message with no matching ToolResult (an orphan). With it, it should
        // produce a balanced assistant message followed by a tool result message.
        let messages = retrace::messages(&events).expect("retrace must succeed");
        // Every ToolCall in a messages list must have a corresponding ToolResult.
        // retrace folds them into paired assistant (tool_calls) + user (tool_results)
        // messages. Verify no dangling tool_call_id in the assistant message.
        let has_orphan = messages.iter().any(|m| {
            use crate::types::Message;
            match m {
                Message::Assistant { content, .. } => {
                    use crate::types::AssistantContent;
                    matches!(content, AssistantContent::Parts(parts) if {
                        parts.iter().any(|p| {
                            use crate::types::AssistantPart;
                            matches!(p, AssistantPart::ToolCall(_))
                        })
                    }) && !messages.iter().any(|m2| {
                        matches!(m2, Message::ToolResult { content, .. } if {
                            content.iter().any(|r| r.tool_call_id == call_id)
                        })
                    })
                }
                _ => false,
            }
        });
        assert!(
            !has_orphan,
            "retrace must find no orphan ToolCall after synthesis"
        );
    }

    // ── emit_terminal_failure ordering test ──────────────────────────────────

    #[test]
    #[serial_test::serial]
    fn terminal_failure_writes_error_event_and_diagnostic() {
        std::env::set_var("GAIT_DEBUG", "1");
        let tmp = tempfile::tempdir().unwrap();
        let gait_dir = tmp.path();
        let session = "sterm";
        std::fs::create_dir_all(crate::session::live::worktree_dir(gait_dir, session)).unwrap();
        let mut failed = false;
        let session_log = crate::turn::SessionLog::open(gait_dir, session).unwrap();
        emit_terminal_failure(
            &session_log,
            &crate::turn::DiagnosticSink::new(gait_dir, session),
            &mut failed,
            crate::diagnostic::Phase::Streaming,
            "absolute backstop exceeded",
        );

        assert!(failed, "brain_failed flag set for exit parity");
        let log_path = crate::session::live::log_path(gait_dir, session);
        let log = std::fs::read_to_string(&log_path).unwrap();
        assert!(
            log.to_lowercase().contains("error"),
            "Error event in log.jsonl"
        );
        let diag =
            std::fs::read_to_string(crate::session::live::diagnostic_path(gait_dir, session))
                .unwrap();
        assert!(diag.contains("streaming") && diag.contains("absolute backstop"));
        std::env::remove_var("GAIT_DEBUG");
    }
}

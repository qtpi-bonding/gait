//! Turn profile construction, brain argv, and the load-bearing post-loop
//! teardown sequence (drain → orphan-repair → EOF → reap → checkpoint → commit).

use std::path::Path;
use std::sync::Arc;

// ── ProfileInputs + build_turn_profile ───────────────────────────────────────

/// Inputs the spine needs to build the turn's `Profile` (parsed by the CLI/REPL
/// caller, since gait-core cannot parse clap args). Mirrors the brain's old
/// agent/model/allow/deny handling.
#[derive(Clone)]
pub struct ProfileInputs {
    pub agent_name: Option<String>,
    pub model_override: Option<String>,
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
    Persistent,
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
    let mut profile = crate::profile::Profile::from_config_and_agent(
        &cfg,
        gait_dir,
        Some(codebase),
        agent_path.as_deref(),
        inputs.model_override.clone(),
    );
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
    let evt = crate::proto::envelope::mint(body, prev.as_ref(), 0, 0);
    log.relay_event(&evt).ok();
    *brain_failed = true;

    // 2) DiagRecord → diagnostic.jsonl SECOND (always, even when GAIT_DEBUG is
    //    off — a terminal failure must always be explained).
    let rec = crate::diagnostic::DiagRecord::terminal(diag.session_id(), phase, detail);
    diag.terminal(&rec.to_json_line());
}

/// Post-loop epilogue. The step order is load-bearing:
///   0. sink.end_turn().
///   1. drain in-flight tasks (grace, then abort) — must precede reap so a
///      task abort never races the cascade kill.
///   2. synthesize orphan ToolResults for pending call_ids.
///   3. drop child_stdin (EOF to the brain's Nerve reader).
///   4. reap: group.kill_all(), drop Child WITHOUT awaiting (macOS kqueue
///      workaround — awaiting child.wait() hangs; see reap_brain).
///   5. git.checkpoint(codebase, ..) (best-effort).
///   6. git.commit(&msg) (message keyed on run_result kind).
///   7. sink.result(&result).
///
/// Steps 1–4 must complete before 5–6 so the transcript is final before commit.
/// Returns `brain_failed` (the value relay_brain_turn must return).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn teardown(
    sink: &mut dyn crate::port::output::TurnSink,
    inflight: tokio::task::JoinSet<()>,
    mut pending_calls: std::collections::HashMap<String, String>,
    child_stdin: Arc<tokio::sync::Mutex<tokio::process::ChildStdin>>,
    group: Arc<dyn crate::process::Containment>,
    child: tokio::process::Child,
    git: crate::turn::SessionGit,
    codebase: &Path,
    session_id: &str,
    log: &crate::turn::SessionLog,
    run_result: Option<crate::turn::TurnResult>,
    brain_failed: bool,
) -> anyhow::Result<bool> {
    sink.end_turn();

    // Bound how long an in-flight tool call (or spawn_agent handler) can keep
    // running after the read loop has already decided the
    // turn is over. Must run before reap_brain: this is the spine's own
    // in-process task leak, distinct from (and not covered by) reap_brain's
    // OS-process-group kill.
    drain_inflight(inflight, std::time::Duration::from_secs(3)).await;

    // Synthesize a ToolResult for each orphaned ToolCall (brain was killed before
    // it could emit results). Tools that completed already have real results in the
    // log; only call_ids still in pending_calls are orphans. Must run before
    // commit_session so the repair lands in the same commit as the transcript.
    for (call_id, tool_name) in pending_calls.drain() {
        let msg = format!("[LLM timed out before '{}' result was received]", tool_name);
        let body = crate::proto::events::tool_result(call_id, msg, None, None);
        let prev = log.last_event();
        let ev = crate::proto::envelope::mint(body, prev.as_ref(), 0, 0);
        log.relay_event(&ev).ok();
    }

    // Drop the brain's stdin writer to signal EOF — the brain's Nerve reader
    // task is blocking on read_signal from stdin and will exit on EOF, which
    // lets the brain's tokio runtime shut down and the process exit.
    drop(child_stdin);

    // Single authoritative reap for EVERY exit path (EOF / Terminated / watchdog).
    // Reaps via kill_all() + Child drop — never tokio's child.wait(), which is
    // fatal on macOS (see `reap_brain`).
    reap_brain(group, child).await;

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
        let events = log.events();
        let turn_n = crate::session::log::count_turn_complete(&events) as u32;
        let _ = git.checkpoint(codebase, turn_n);

        // Commit the transcript to the session worktree.
        let _ = git.commit(&msg);
    }

    if let Some(result) = run_result {
        sink.result(&result);
    }

    Ok(brain_failed)
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
        std::fs::write(ws.path().join("gait.jsonc"), r#"{"model":"m"}"#).unwrap();
        let schema = serde_json::json!({"type":"object","required":["answer"]});
        let inputs = ProfileInputs {
            agent_name: None,
            model_override: None,
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
        std::fs::write(dir.path().join("gait.jsonc"), r#"{"model":"anthropic/x"}"#).unwrap();
        let inputs = ProfileInputs {
            agent_name: None,
            model_override: None,
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
    #[should_panic(expected = "model must be set before building Profile")]
    fn build_turn_profile_panics_without_config_or_override() {
        let dir = tempfile::TempDir::new().unwrap();
        // No gait.jsonc — load_config falls back to GaitConfig::default() which has model: None.
        let inputs = ProfileInputs {
            agent_name: None,
            model_override: None,
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
    /// This test exercises the synthesis logic directly: write a ToolCall to the
    /// log, then simulate what the spine's teardown does (drain pending_calls and
    /// append synthetic ToolResults), and verify the log ends with a matching
    /// ToolResult and retrace sees no orphan tool calls.
    #[test]
    fn cancelled_turn_synthesizes_tool_result_for_orphan_call_id() {
        use crate::proto::gait::log::v1::{event, payload};
        use crate::proto::{envelope, wire};
        use crate::session::retrace;

        let dir = tempfile::TempDir::new().unwrap();
        let log_path = dir.path().join("log.jsonl");

        // 1. Write a ToolCall (simulating what the brain emits mid-turn).
        let call_id = "call-orphan-1".to_string();
        let tc_body = crate::proto::events::tool_call(
            call_id.clone(),
            "read".into(),
            &serde_json::json!({"file_path": "foo.rs"}),
        );
        wire::append(&log_path, tc_body).expect("append tool_call");

        // 2. Simulate cancel: the spine synthesizes a ToolResult for the orphan.
        // tool_name "read" is the value stored in pending_calls (call_id → tool_name).
        let body = crate::proto::events::tool_result(
            call_id.clone(),
            "[LLM timed out before 'read' result was received]".into(),
            None,
            None,
        );
        let prev = crate::session::log::last_event(&log_path).ok().flatten();
        let ev = envelope::mint(body, prev.as_ref(), 0, 0);
        wire::append_with_id(&log_path, &ev).expect("append synthesized tool_result");

        // 3. Assert: the last event is a ToolResult with the correct call_id.
        let events = crate::session::log::read_events(&log_path).unwrap();
        assert_eq!(events.len(), 2, "log must have exactly 2 events");
        let last = events.last().unwrap();
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
    fn terminal_failure_writes_error_event_and_diagnostic() {
        let tmp = tempfile::tempdir().unwrap();
        let gait_dir = tmp.path();
        let session = "sterm";
        let sdir = gait_dir.join("sessions").join(session);
        std::fs::create_dir_all(&sdir).unwrap();
        let mut failed = false;
        let session_log = crate::turn::SessionLog::open(&sdir).unwrap();
        emit_terminal_failure(
            &session_log,
            &crate::turn::DiagnosticSink::new(gait_dir, session),
            &mut failed,
            crate::diagnostic::Phase::Streaming,
            "absolute backstop exceeded",
        );

        assert!(failed, "brain_failed flag set for exit parity");
        let log_path = sdir.join("log.jsonl");
        let log = std::fs::read_to_string(&log_path).unwrap();
        assert!(
            log.to_lowercase().contains("error"),
            "Error event in log.jsonl"
        );
        let diag = std::fs::read_to_string(sdir.join("diagnostic.jsonl")).unwrap();
        assert!(diag.contains("streaming") && diag.contains("absolute backstop"));
    }
}

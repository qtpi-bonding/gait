//! The unified turn run-loop shared by the CLI (`cmd_run`) and, later, the REPL.
//! A `TurnSource` decides whether each iteration appends a new input event or
//! resumes from the log; the loop runs one `run_turn` per yield and routes its
//! live stream through a `TurnSink`. (Phase 4 input port.)
use std::ops::ControlFlow;
use std::path::Path;
use std::sync::Arc;
use crate::port::input::{TurnSource, TurnStart};
use crate::port::output::{drive, TurnSink};
use crate::turn::{TurnInput, TurnResult};

/// Per-turn config the loop can't infer — sourced from `config`/CLI flags/node,
/// NOT from `caps`. `auto` gates permissions in `run_turn`, so it must be honored
/// (never forced true).
pub struct LoopConfig {
    pub auto: bool,
    pub max_iter: Option<usize>,
    pub preview_policy: crate::config::PreviewPolicy,
}

/// Per-turn machinery the loop can't own generically. `?Send`: the driver runs on
/// the loop's own task (only `run_turn` is spawned), and a REPL driver/sink holds
/// non-`Send` view state.
#[async_trait::async_trait(?Send)]
pub trait TurnDriver {
    /// Build `caps` for this turn (`ResolvedProfile` is not `Clone`).
    async fn caps(&mut self) -> anyhow::Result<crate::profile::ResolvedProfile>;
    /// `auto`/`max_iter`/`preview` for this turn.
    fn turn_config(&self) -> LoopConfig;
    /// After-turn policy (CLI: Scheduler/Harness; REPL: none). Runs on success only.
    async fn after_turn(&mut self, _sink: &mut dyn TurnSink, _result: &TurnResult) {}
    /// Turn error/panic policy. CLI → `diagnostic` + `Break` (the post-loop
    /// `if sink.failed { exit(1) }` then fires); REPL → `eprintln` + `Continue`
    /// (return to the prompt). On error, `result`/`after_turn` do NOT run.
    fn on_turn_error(&mut self, sink: &mut dyn TurnSink, e: anyhow::Error) -> ControlFlow<()>;
}

/// Build a `user_turn` body carrying the same turn-time `UserTurnContext`
/// `run_turn` computes internally (codebase head hash + dirty files) — so the
/// input adapter, not `run_turn`, owns it (Phase 4 Option A).
pub fn user_turn_body(repos: &crate::git::Repos, codebase: &Path, task: String) -> crate::proto::gait::log::v1::event::Body {
    let codebase_hash = repos.codebase.head_hash(codebase);
    let dirty_files = repos.codebase.dirty_files(codebase);
    let ctx = crate::session::transcript::UserTurnContext {
        codebase_hash: Some(codebase_hash),
        codebase_dirty: Some(!dirty_files.is_empty()),
        dirty_files: if dirty_files.is_empty() { None } else { Some(dirty_files) },
        injected_files: None,
    };
    crate::proto::events::user_turn(task, None, Some(&ctx))
}

/// Run turns until the source is exhausted — one `run_turn` per `TurnStart`.
/// `Input` bodies are appended before the fold; `Resume` runs a bare fold turn.
pub async fn run_loop(
    mut source: impl TurnSource,
    sink: &mut impl TurnSink,
    session: Arc<crate::session::Session>,
    gait_dir: &Path,
    session_id: &str,
    driver: &mut impl TurnDriver,
) -> anyhow::Result<()> {
    // Ensure the session worktree (and thus the log dir) exists before we append
    // the first input event — `run_turn` also creates it (idempotent), but the
    // pre-turn append below would otherwise fail on a fresh session.
    session.repos.traces.create_session_worktree(gait_dir, session_id)?;
    let tx_path = gait_dir.join("sessions").join(session_id).join("log.jsonl");
    while let Some(start) = source.next().await {
        if let TurnStart::Input(body) = start {
            crate::proto::wire::append(&tx_path, body)?;   // append-then-fold
        }
        let caps = driver.caps().await?;
        let cfg = driver.turn_config();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let emitter = crate::Recorder::for_session(gait_dir, session_id, tx);
        let input = TurnInput { auto: cfg.auto, max_iter: cfg.max_iter, preview_policy: cfg.preview_policy };
        let session_for_turn = session.clone();
        let handle = tokio::spawn(async move {
            crate::turn::run_turn(&*session_for_turn, &emitter, caps, input).await
            // emitter drops here → channel closes → drive() returns
        });
        sink.begin_turn();
        drive(rx, sink).await;
        sink.end_turn();              // runs on success AND error (preserves flush-before-error)
        match handle.await {
            Ok(Ok(result)) => { sink.result(&result); driver.after_turn(sink, &result).await; }
            Ok(Err(e)) => { if driver.on_turn_error(sink, e).is_break() { break; } }
            Err(panic) => { if driver.on_turn_error(sink, anyhow::anyhow!("turn task panicked: {panic}")).is_break() { break; } }
        }
    }
    Ok(())
}

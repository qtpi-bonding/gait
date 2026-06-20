//! Embedding API — drive gait in-process by implementing the I/O ports
//! (`TurnSource`/`TurnSink`/`TurnDriver`) and calling [`run_loop`]. The CLI and
//! REPL are themselves embedders built on this surface. Output is pbjson (the
//! protobuf→JSON gait uses everywhere) via [`JsonSink`], so an FFI-style consumer
//! (Serverpod/Flutter) parses it with a model generated from the same `.proto`.
//! (Phase 5 — the in-process boundary the whole turn-I/O track was shaped for.)
//!
//! The unified run-loop runs one `run_turn` per `TurnSource` yield and routes its
//! live stream through a `TurnSink`.

// One-`use` embedding facade: re-export the port surface so a consumer needs only
// `use gait_ai::embed::*`.
pub use crate::config::{load_config, GaitConfig};
pub use crate::git::Repos;
pub use crate::port::input::{TurnSource, TurnStart};
pub use crate::port::output::{run_result_proto, Diag, TurnSink};
pub use crate::profile::ResolvedProfile;
pub use crate::proto::gait::log::v1::{RunResult, StreamItem};
pub use crate::session::Session;
pub use crate::turn::{TurnInput, TurnResult};
pub use crate::Recorder;

use crate::port::output::drive;
use std::ops::ControlFlow;
use std::path::Path;
use std::sync::Arc;

/// Per-turn config the loop can't infer — sourced from `config`/CLI flags/node,
/// NOT from `caps`. `auto` gates permissions in `run_turn`, so it must be honored
/// (never forced true).
pub struct LoopConfig {
    pub auto: bool,
    pub max_iter: Option<usize>,
    pub cancel: Option<tokio_util::sync::CancellationToken>,
    pub preview_policy: crate::config::PreviewPolicy,
}

/// Per-turn machinery the loop can't own generically. `?Send`: the driver runs on
/// the loop's own task (only `run_turn` is spawned), and a REPL driver/sink holds
/// non-`Send` view state.
#[async_trait::async_trait(?Send)]
pub trait TurnDriver {
    /// Resolve the `ResolvedProfile` for this turn (`ResolvedProfile` is not `Clone`).
    async fn resolve_profile(&mut self) -> anyhow::Result<crate::profile::ResolvedProfile>;
    /// `auto`/`max_iter`/`preview` for this turn.
    fn turn_config(&mut self) -> LoopConfig;
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
pub fn user_turn_events(
    repos: &crate::git::Repos,
    codebase: &Path,
    task: String,
) -> Vec<crate::proto::gait::log::v1::event::Body> {
    let codebase_hash = repos.codebase.head_hash(codebase).unwrap_or_default();
    let dirty_files = repos.codebase.dirty_files(codebase);
    let codebase = crate::proto::gait::log::v1::CodebaseState {
        dirty: !dirty_files.is_empty(),
        hash: codebase_hash,
        dirty_files,
    };
    vec![
        crate::proto::events::user_text(task, None),
        crate::proto::events::context_status(Some(codebase), vec![]),
    ]
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
    session
        .repos
        .traces
        .create_session_worktree(gait_dir, session_id)?;
    let tx_path = gait_dir.join("sessions").join(session_id).join("log.jsonl");
    while let Some(start) = source.next().await {
        if let TurnStart::Input(bodies) = start {
            for body in bodies {
                crate::proto::wire::append(&tx_path, body)?; // append-then-fold
            }
        }
        let profile = driver.resolve_profile().await?;
        let cfg = driver.turn_config();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let emitter = crate::Recorder::for_session(gait_dir, session_id, tx);
        let input = TurnInput {
            auto: cfg.auto,
            max_iter: cfg.max_iter,
            preview_policy: cfg.preview_policy,
            cancel: cfg.cancel,
        };
        let session_for_turn = session.clone();
        let handle = tokio::spawn(async move {
            crate::turn::run_turn(&session_for_turn, &emitter, profile, input).await
            // emitter drops here → channel closes → drive() returns
        });
        sink.begin_turn();
        drive(rx, sink).await;
        sink.end_turn(); // runs on success AND error (preserves flush-before-error)
        match handle.await {
            Ok(Ok(result)) => {
                sink.result(&result);
                driver.after_turn(sink, &result).await;
            }
            Ok(Err(e)) => {
                if driver.on_turn_error(sink, e).is_break() {
                    break;
                }
            }
            Err(panic) => {
                if driver
                    .on_turn_error(sink, anyhow::anyhow!("turn task panicked: {panic}"))
                    .is_break()
                {
                    break;
                }
            }
        }
    }
    Ok(())
}

/// Resolve a live profile (provider + tools + MCP) for one session from an
/// already-loaded `config` + optional agent file + optional model override — the
/// same resolution the CLI and REPL do. Embedders load config via [`load_config`]
/// and pass it in (reusing it across turns), instead of touching `Profile`.
#[allow(clippy::too_many_arguments)]
pub async fn resolve_profile(
    config: &crate::config::GaitConfig,
    gait_dir: &Path,
    codebase: &Path,
    session_id: &str,
    agent_path: Option<&Path>,
    model_override: Option<String>,
    repos: &crate::git::Repos,
    workflow_runner: Option<std::sync::Arc<dyn crate::workflow_runner::WorkflowRunner>>,
) -> anyhow::Result<crate::profile::ResolvedProfile> {
    let profile = crate::profile::Profile::from_config_and_agent(
        config,
        gait_dir,
        Some(codebase),
        agent_path,
        model_override,
    );
    let api_key = crate::auth::resolve_api_key(
        profile
            .model
            .split_once('/')
            .map(|(p, _)| p)
            .unwrap_or(&profile.model),
    );
    profile
        .resolve(
            codebase.to_path_buf(),
            session_id.to_string(),
            gait_dir.to_path_buf(),
            repos.clone(),
            api_key,
            workflow_runner,
        )
        .await
}

/// A frame handed to a [`JsonSink`] callback. Strings are pbjson (canonical
/// protobuf-JSON) the consumer parses with classes generated from the `.proto`.
/// Owns `String` (no borrowed lifetime — a borrowed `&str` would force a
/// higher-ranked `FnMut` bound bare closures fail to infer; the consumer copies
/// the string out anyway).
pub enum JsonFrame {
    /// pbjson `StreamItem` — a Trace/Error event or a text delta.
    Item(String),
    /// pbjson `RunResult` — `{message, result, changes}`.
    Result(String),
    Diagnostic(Diag, String),
}

/// A [`TurnSink`] that serializes each frame to pbjson exactly as the CLI's
/// `CliSink` serializes a frame, and hands the string to a callback. The
/// FFI-ready output sink: drive a turn, get pbjson out — a thin C-ABI shim maps an
/// `extern "C"` fn pointer + opaque ctx to this closure, and the consumer
/// (Serverpod/Flutter) parses the JSON with a model generated from the same
/// hands the consumer every item; the consumer classifies on its side. `failed`
/// is set on an error diagnostic (exit-code parity).
pub struct JsonSink<F: FnMut(JsonFrame)> {
    pub f: F,
    pub failed: bool,
}
impl<F: FnMut(JsonFrame)> JsonSink<F> {
    pub fn new(f: F) -> Self {
        Self { f, failed: false }
    }
}
impl<F: FnMut(JsonFrame)> TurnSink for JsonSink<F> {
    fn item(&mut self, item: &StreamItem) {
        if let Ok(j) = serde_json::to_string(item) {
            (self.f)(JsonFrame::Item(j));
        }
    }
    fn result(&mut self, r: &TurnResult) {
        if let Ok(j) = serde_json::to_string(&run_result_proto(r)) {
            (self.f)(JsonFrame::Result(j));
        }
    }
    fn session_id(&mut self, _id: &str) {} // embedder supplies the id to run_loop; nothing to emit
    fn diagnostic(&mut self, level: Diag, msg: &str) {
        if level == Diag::Error {
            self.failed = true;
        }
        (self.f)(JsonFrame::Diagnostic(level, msg.to_string()));
    }
}

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
pub use crate::port::output::{run_result_proto, CliSink, Diag, TurnSink};
pub use crate::profile::ResolvedProfile;
pub use crate::proto::gait::log::v1::{RunResult, StreamItem};
pub use crate::session::Session;
pub use crate::turn::{TurnInput, TurnResult};
pub use crate::Recorder;

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
    pub sandbox_on: bool,
    pub sandbox_cfg: crate::config::SandboxConfig,
    pub relay: Option<crate::turn::nerve::NerveWriter>,
    pub initial_git: Option<crate::turn::GitSnapshot>,
    pub daemons: Vec<(String, i32, bool, String)>,
}

/// All driver-supplied args that `relay_brain_turn` needs for one brain spawn.
/// Returned by `TurnDriver::brain_launch()`; consumed by `run_loop`.
///
/// Fields match `relay_brain_turn`'s real signature (spine.rs:123); `gait_dir`,
/// `session_id`, `sink`, and `signal_slot` come from `run_loop`'s own params.
/// The easy one to forget: `codebase` (PathBuf, not just cli args).
pub struct BrainLaunch {
    /// Argv forwarded to `gait __brain` (already has --session injected by the driver).
    pub brain_args: Vec<String>,
    /// Absolute path to the codebase root (required by relay_brain_turn).
    pub codebase: std::path::PathBuf,
    /// Resolved LLM API key (empty string if none).
    pub api_key: String,
    /// Absolute path to the gait binary (current_exe or resolved path).
    pub gait_bin: std::path::PathBuf,
    /// Sandbox configuration to apply to the brain process.
    pub sandbox_cfg: crate::config::SandboxConfig,
    /// Whether the sandbox is active for this turn.
    pub sandbox_on: bool,
    /// Profile-level inputs forwarded to BrainInit (agent/model/allow/deny/result_schema).
    pub profile_inputs: crate::turn::spine::ProfileInputs,
    /// Optional cascade group handle string (pgid or cgroup path) passed from a parent spine.
    pub cascade_group_arg: Option<String>,
}

/// Per-turn machinery the loop can't own generically. `?Send`: the driver runs on
/// the loop's own task (only the brain spawn is async), and a REPL driver/sink holds
/// non-`Send` view state.
///
/// Post-Task-2: `resolve_profile` and `make_recorder` have been REMOVED — the brain
/// resolves its own profile from `BrainInit`. The only public method is `brain_launch()`,
/// which supplies the data `relay_brain_turn` needs to spawn a sealed brain.
#[async_trait::async_trait(?Send)]
pub trait TurnDriver {
    /// All data needed to spawn `gait __brain` for one turn.
    fn brain_launch(&self, session_id: &str) -> BrainLaunch;
    /// `auto`/`max_iter`/`preview` for this turn (used to build BrainInit).
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

/// Run turns until the source is exhausted — one sealed `gait __brain` per `TurnStart`.
///
/// `run_loop` is the **spine loop**: per `TurnSource::next()` it calls
/// `relay_brain_turn` (spawn sealed brain, relay Nerve, serve tools, write log).
/// The brain runs `run_turn` in-process; the spine relays events to `sink`.
///
/// `Input` bodies are pre-written before the brain spawns (spine is the sole writer).
/// `cascade` is an optional process-lifetime `SignalSlot` for the top-level kill
/// handle; `run_loop` stores the group per turn and clears it after each turn.
pub async fn run_loop(
    mut source: impl TurnSource,
    sink: &mut impl TurnSink,
    _session: Arc<crate::session::Session>,
    gait_dir: &Path,
    session_id: &str,
    driver: &mut impl TurnDriver,
    cascade: Option<&crate::turn::spine::SignalSlot>,
) -> anyhow::Result<()> {
    let tx_path = gait_dir.join("sessions").join(session_id).join("log.jsonl");
    while let Some(start) = source.next().await {
        if let TurnStart::Input(bodies) = start {
            // Pre-write input events (spine is the sole gait-dir writer).
            // relay_brain_turn will create the worktree if needed (idempotent).
            for body in bodies {
                // Ensure session worktree exists before first append.
                let repos = crate::git::Repos::production();
                repos.traces.create_session_worktree(gait_dir, session_id)?;
                crate::proto::wire::append(&tx_path, body)?;
            }
        }
        let launch = driver.brain_launch(session_id);
        let failed = crate::turn::spine::relay_brain_turn(
            &launch.brain_args,
            gait_dir,
            &launch.codebase,
            session_id,
            &launch.api_key,
            &launch.gait_bin,
            &launch.sandbox_cfg,
            launch.sandbox_on,
            &launch.profile_inputs,
            sink,
            launch.cascade_group_arg,
            cascade,
        )
        .await;
        // Clear the cascade slot after each turn (parity with spine.rs:248).
        if let Some(slot) = cascade {
            *slot.lock().unwrap() = None;
        }
        match failed {
            Ok(true) => {
                // Brain signalled failure — surface as a turn error.
                if driver
                    .on_turn_error(sink, anyhow::anyhow!("brain turn failed"))
                    .is_break()
                {
                    break;
                }
            }
            Ok(false) => {
                // Success: after_turn policy (Scheduler/Harness).
                // Note: sink.result() was already called by relay_brain_turn (spine.rs:517).
                // We pass a synthetic Done result to after_turn; the real result was
                // captured by sink during relay.
                let dummy = TurnResult::Done {
                    message: String::new(),
                    result: None,
                    changes: None,
                };
                driver.after_turn(sink, &dummy).await;
            }
            Err(e) => {
                if driver.on_turn_error(sink, e).is_break() {
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
///
/// `api_key` must already be resolved by the caller (the spine in Phase 3, the
/// REPL driver before that). Keeping key resolution outside this function ensures
/// the brain never needs to read `auth.json` directly.
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
    api_key: Option<String>,
) -> anyhow::Result<crate::profile::ResolvedProfile> {
    let profile = crate::profile::Profile::from_config_and_agent(
        config,
        gait_dir,
        Some(codebase),
        agent_path,
        model_override,
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

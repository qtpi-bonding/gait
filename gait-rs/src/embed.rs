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
pub use crate::port::output::{
    run_result_proto, CliSink, Diag, RecordingSink, SinkFrame, TurnSink,
};
pub use crate::profile::ResolvedProfile;
pub use crate::proto::gait::log::v1::{RunResult, StreamItem};
pub use crate::session::Session;
pub use crate::turn::spine::{strip_task_from_args, TurnMode};
pub use crate::turn::{TurnInput, TurnResult};
pub use crate::Recorder;
pub use cascade::CascadeKillHandle;

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
    /// Absolute path to the codebase root (required by relay_brain_turn).
    pub codebase: std::path::PathBuf,
    /// Resolved LLM API key (empty string if none).
    pub api_key: String,
    /// Absolute path to the gait binary (current_exe or resolved path).
    pub gait_bin: std::path::PathBuf,
    /// Sandbox configuration to apply to the brain process.
    pub sandbox_cfg: crate::config::SandboxConfig,
    /// Whether the sandbox is active for this turn (drives the brain's seal flag).
    pub sandbox_on: bool,
    /// Whether the brain runs non-interactively (`--auto`).
    pub auto: bool,
    /// Per-turn iteration cap (`--max-iter`), if any.
    pub max_iter: Option<usize>,
    /// Profile-level inputs forwarded to BrainInit (agent/model/allow/deny/result_schema).
    pub profile_inputs: crate::turn::spine::ProfileInputs,
    /// Optional cascade group handle string (pgid or cgroup path) from a parent spine.
    pub cascade_group_arg: Option<String>,
}

impl BrainLaunch {
    /// Construct a `BrainLaunch` without naming `ProfileInputs` in the call site.
    /// `gait-repl` uses this so it doesn't import the sealed `turn::spine` type.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        codebase: std::path::PathBuf,
        api_key: String,
        gait_bin: std::path::PathBuf,
        sandbox_cfg: crate::config::SandboxConfig,
        sandbox_on: bool,
        auto: bool,
        max_iter: Option<usize>,
        cascade_group_arg: Option<String>,
        agent_name: Option<String>,
        model_override: Option<String>,
        allow: Vec<String>,
        deny: Vec<String>,
        result_schema: Option<serde_json::Value>,
    ) -> Self {
        Self {
            codebase,
            api_key,
            gait_bin,
            sandbox_cfg,
            sandbox_on,
            auto,
            max_iter,
            cascade_group_arg,
            profile_inputs: crate::turn::spine::ProfileInputs {
                agent_name,
                model_override,
                allow,
                deny,
                result_schema,
                hands_free: false,
            },
        }
    }
}

/// Per-turn machinery the loop can't own generically. `?Send`: the driver runs on
/// the loop's own task (only the brain spawn is async), and a REPL driver/sink holds
/// non-`Send` view state.
/// Post-Task-2: `resolve_profile` and `make_recorder` have been REMOVED — the brain
/// resolves its own profile from `BrainInit`. The only public method is `brain_launch()`,
/// which supplies the data `relay_brain_turn` needs to spawn a sealed brain.
/// The `?Send` here is a deliberate contract mirrored on [`TurnSource`]: the loop
/// is awaited inline, never spawned across threads. Do not `tokio::spawn` a
/// driver. See `architecture-mapping/spec-05-interactive-suspension.md`.
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
    session: Arc<crate::session::Session>,
    gait_dir: &Path,
    session_id: &str,
    driver: &mut impl TurnDriver,
    cascade: Option<&CascadeKillHandle>,
) -> anyhow::Result<()> {
    let tx_path = crate::session::live::log_path(gait_dir, session_id);
    while let Some(start) = source.next().await {
        if let TurnStart::Input(bodies) = start {
            // Pre-write input events (spine is the sole gait-dir writer).
            // Ensure session worktree exists before first append (once per turn, not per body).
            session
                .repos
                .traces
                .create_session_worktree(gait_dir, session_id)?;
            for body in bodies {
                crate::proto::wire::append(&tx_path, body)?;
            }
        }
        let launch = driver.brain_launch(session_id);
        let failed = crate::turn::spine::relay_brain_turn(
            gait_dir,
            &launch.codebase,
            session_id,
            &launch.api_key,
            &launch.gait_bin,
            &launch.sandbox_cfg,
            launch.sandbox_on,
            launch.auto,
            launch.max_iter,
            &launch.profile_inputs,
            sink,
            launch.cascade_group_arg,
            cascade.map(|h| &h.0),
            &session.repos,
            &crate::turn::spine::TurnMode::Persistent,
        )
        .await;
        // Clear the cascade slot after each turn (parity with spine.rs:248).
        if let Some(handle) = cascade {
            *handle.0.lock().unwrap() = None;
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
                    implicit: true,
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
        )
        .await
}

// ── CascadeKillHandle ────────────────────────────────────────────────────────

mod cascade {
    use crate::turn::spine::SignalSlot;

    /// Opaque, process-level teardown for the active sealed tree. Held by the
    /// gait binary's top-level signal handler. The cascade group it wraps stays
    /// private — callers invoke `kill()` and know nothing about the group type.
    ///
    /// Construct with [`CascadeKillHandle::new`] once (process lifetime), hand a
    /// clone to the signal handler, and pass `Some(&handle)` to [`run_loop`].
    #[derive(Clone)]
    pub struct CascadeKillHandle(pub(super) SignalSlot);

    impl CascadeKillHandle {
        /// Create a handle backed by an empty slot (no group enrolled yet).
        pub fn new() -> Self {
            Self(std::sync::Arc::new(std::sync::Mutex::new(None)))
        }

        /// Kill all processes enrolled in the active cascade group, if any.
        /// If no turn is running the slot is empty and this is a no-op.
        pub fn kill(&self) {
            if let Some(group) = self.0.lock().unwrap().clone() {
                let _ = group.kill_all();
            }
        }
    }

    impl Default for CascadeKillHandle {
        fn default() -> Self {
            Self::new()
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// Enrolled child is killed; detached peer survives — through the opaque handle.
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        #[test]
        fn cascade_kill_handle_kills_enrolled_spares_detached() {
            let handle = CascadeKillHandle::new();
            let group = crate::process::establish(None);

            // Store the group into the slot (mirrors relay_brain_turn).
            *handle.0.lock().unwrap() = Some(std::sync::Arc::clone(&group));

            // Enrolled child.
            let mut enrolled_cmd = std::process::Command::new("sleep");
            enrolled_cmd
                .arg("60")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            let mut enrolled = group.spawn_enrolled(enrolled_cmd).expect("spawn enrolled");
            let enrolled_pid = enrolled.id() as i32;

            // Detached peer.
            let mut peer_cmd = std::process::Command::new("sleep");
            peer_cmd
                .arg("60")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            let (mut peer, peer_pid) = group.spawn_detached(peer_cmd).expect("spawn detached");

            // Kill via the opaque handle.
            handle.kill();
            std::thread::sleep(std::time::Duration::from_millis(300));

            let _ = enrolled.wait();
            let enrolled_alive = unsafe { libc::kill(enrolled_pid, 0) };
            let peer_alive = unsafe { libc::kill(peer_pid, 0) };

            unsafe { libc::kill(peer_pid, libc::SIGKILL) };
            let _ = peer.wait();

            assert_eq!(
                enrolled_alive, -1,
                "enrolled child (pid {enrolled_pid}) should be dead after handle.kill()"
            );
            assert_eq!(
                peer_alive, 0,
                "detached peer (pid {peer_pid}) should survive handle.kill()"
            );
        }
    }
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

/// Run ONE brain turn with no persistence (the `gait -e` one-off). Builds the
/// user-input event in memory, drives the shared `relay_brain_turn` with
/// `TurnMode::Ephemeral` + a hands-free profile, and returns the captured
/// `{cmd|output}` result from the brain's Terminated signal via a RecordingSink.
#[allow(clippy::too_many_arguments)]
pub async fn run_one_off_turn(
    codebase: &std::path::Path,
    api_key: &str,
    exe: &std::path::Path,
    sandbox_cfg: &crate::config::SandboxConfig,
    sandbox_on: bool,
    model: &str,
    input_text: String,
    schema: serde_json::Value,
) -> anyhow::Result<crate::turn::TurnResult> {
    // The brain requires a --gait-dir; a scratch tempdir is never written to.
    let scratch = tempfile::tempdir()?;
    let session_id = "ephemeral";

    // Seed the in-memory transcript with the user input event.
    let body = crate::proto::events::user_text(input_text, None);
    let input = vec![crate::proto::envelope::mint(body, None, 0, 0)];

    let profile_inputs = crate::turn::spine::ProfileInputs {
        agent_name: None,
        model_override: Some(model.to_string()),
        allow: vec![],
        deny: vec![],
        result_schema: Some(schema),
        hands_free: true,
    };

    let mut sink = RecordingSink::default();
    crate::turn::spine::relay_brain_turn(
        scratch.path(),
        codebase,
        session_id,
        api_key,
        exe,
        sandbox_cfg,
        sandbox_on,
        /* auto */ true,
        /* max_iter */ Some(2),
        &profile_inputs,
        &mut sink,
        /* cascade_group_arg */ None,
        /* signal_slot */ None,
        &crate::git::Repos::production(),
        &TurnMode::Ephemeral { input },
    )
    .await?;

    sink.results
        .pop()
        .ok_or_else(|| anyhow::anyhow!("model returned no structured result"))
}

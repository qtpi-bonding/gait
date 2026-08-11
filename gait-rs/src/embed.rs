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
pub use crate::port::output::{CliSink, Diag, RecordingSink, SinkFrame, TurnSink};
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
/// Embedder-supplied bridge to an editor's live buffers (ACP `fs/read_text_file`
/// / `fs/write_text_file`). Injected via [`BrainLaunch::fs_delegate`]; `None`
/// (CLI/REPL default) means "no delegation — tools use real disk".
///
/// Defined here (the embed surface), NOT in `turn::spine`, so embedders never
/// name the sealed mechanism (`no_spine_in_repl` boundary).
#[async_trait::async_trait]
pub trait FsDelegate: Send + Sync {
    /// Live buffer for `path`. `Ok(None)` = no buffer/file (a new file) — benign,
    /// never fails the tool. `Err` = transport/editor failure — fails the tool.
    async fn read_text_file(&self, path: &std::path::Path) -> anyhow::Result<Option<String>>;
    /// Apply `content` to `path`'s buffer (create if absent) and persist. Returns
    /// only after the editor saved to disk (keeps gait's checkpoint correct).
    async fn write_text_file(&self, path: &std::path::Path, content: &str) -> anyhow::Result<()>;
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
    /// Editor fs bridge for delegated read/write/edit. `None` = disk (CLI/REPL).
    pub fs_delegate: Option<std::sync::Arc<dyn FsDelegate>>,
    /// Launcher-set override for the in-tree safety guard (design §7). Sourced
    /// ONLY from the `--allow-intree` CLI flag — never from config/`gait.jsonc`,
    /// since that file lives in the (potentially in-tree, model-writable)
    /// gait-dir. Default `false` everywhere (REPL, ACP adapter, CLI without the flag).
    pub allow_intree: bool,
}
impl BrainLaunch {
    /// Construct a `BrainLaunch` without naming `ProfileInputs` in the call site.
    /// `gait-repl` uses this so it doesn't import the sealed `turn::spine` type.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        codebase: std::path::PathBuf,
        gait_bin: std::path::PathBuf,
        sandbox_cfg: crate::config::SandboxConfig,
        sandbox_on: bool,
        auto: bool,
        max_iter: Option<usize>,
        cascade_group_arg: Option<String>,
        agent_name: Option<String>,
        inference_override: Option<String>,
        explicit_inference_override: bool,
        allow: Vec<String>,
        deny: Vec<String>,
        result_schema: Option<serde_json::Value>,
    ) -> Self {
        Self {
            codebase,
            gait_bin,
            sandbox_cfg,
            sandbox_on,
            auto,
            max_iter,
            cascade_group_arg,
            fs_delegate: None,
            allow_intree: false,
            profile_inputs: crate::turn::spine::ProfileInputs {
                agent_name,
                inference_override,
                resolved_inference: None,
                explicit_inference_override,
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
/// driver.
#[async_trait::async_trait(?Send)]
pub trait TurnDriver {
    /// All data needed to spawn `gait __brain` for one turn.
    fn brain_launch(&self, session_id: &str) -> BrainLaunch;
    /// `auto`/`max_iter`/`preview` for this turn (used to build BrainInit).
    fn turn_config(&mut self) -> LoopConfig;
    /// After-turn policy (CLI: Scheduler/Harness; REPL: none). Runs on success only.
    async fn after_turn(&mut self, _sink: &mut dyn TurnSink, _result: &RunResult) {}
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
        crate::proto::events::user_text(task),
        crate::proto::events::context_status(Some(codebase), vec![]),
    ]
}

/// True if `gait_dir` resolves to a location inside (or equal to) `codebase`.
/// Canonicalizes both (resolving symlinks/`..`) so a moved-in or symlinked
/// workspace is caught; falls back to the raw path if canonicalize fails. At the
/// `run_loop` guard site the session's gait_dir always exists, so the fallback is
/// not a bypass in practice. `starts_with` on canonical `PathBuf`s is
/// component-wise, so `/repo-foo` does not match `/repo`.
fn gait_dir_inside_codebase(gait_dir: &Path, codebase: &Path) -> bool {
    let gd = gait_dir
        .canonicalize()
        .unwrap_or_else(|_| gait_dir.to_path_buf());
    let cb = codebase
        .canonicalize()
        .unwrap_or_else(|_| codebase.to_path_buf());
    gd.starts_with(&cb)
}

#[cfg(test)]
mod intree_guard_tests {
    use super::gait_dir_inside_codebase;

    #[test]
    fn detects_gait_dir_inside_codebase() {
        let t = tempfile::TempDir::new().unwrap();
        let codebase = t.path().join("repo");
        let inside = codebase.join(".gait");
        std::fs::create_dir_all(&inside).unwrap();
        assert!(gait_dir_inside_codebase(&inside, &codebase));
    }

    #[test]
    fn equal_paths_count_as_inside() {
        let t = tempfile::TempDir::new().unwrap();
        assert!(gait_dir_inside_codebase(t.path(), t.path()));
    }

    #[test]
    fn out_of_tree_and_sibling_prefix_are_not_inside() {
        let t = tempfile::TempDir::new().unwrap();
        let codebase = t.path().join("repo");
        let home = t.path().join("gait-home");
        let sibling = t.path().join("repo-foo"); // must NOT match /repo (component-wise)
        for p in [&codebase, &home, &sibling] {
            std::fs::create_dir_all(p).unwrap();
        }
        assert!(!gait_dir_inside_codebase(&home, &codebase));
        assert!(!gait_dir_inside_codebase(&sibling, &codebase));
    }
}

/// Persist session-control bodies through core's sole-writer boundary.
///
/// Worktree creation must happen before any path below `.active/<session>` is
/// created. Keeping that ordering here prevents frontends from producing a
/// plain directory that libgit2 can no longer attach as a worktree.
pub fn record_session_update(
    repos: &crate::git::Repos,
    gait_dir: &Path,
    session_id: &str,
    bodies: Vec<crate::proto::gait::log::v1::event::Body>,
) -> anyhow::Result<()> {
    if bodies.is_empty() {
        return Ok(());
    }
    repos.traces.create_session_worktree(gait_dir, session_id)?;
    let worktree = crate::session::live::worktree_dir(gait_dir, session_id);
    let _lock = crate::session::SessionLock::acquire(&worktree)?;
    let log_path = crate::session::live::log_path(gait_dir, session_id);
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    for body in bodies {
        crate::proto::wire::append(&log_path, body)?;
    }
    Ok(())
}

/// Run turns until the source is exhausted — one sealed `gait __brain` per
/// `Input` or `Resume`. `SessionUpdate` persists without spawning a brain.
///
/// `run_loop` is the **spine loop**: per `TurnSource::next()` it calls
/// `relay_brain_turn` (spawn sealed brain, relay Nerve, serve tools, write log).
/// The brain runs `run_turn` in-process; the spine relays events to `sink`.
///
/// `Input` and `SessionUpdate` bodies are pre-written by the spine, the sole
/// runtime writer. A session update then continues directly to the next source
/// item; it is not a model turn.
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
    while let Some(start) = source.next().await {
        let trigger = match start {
            TurnStart::SessionUpdate(bodies) => {
                record_session_update(&session.repos, gait_dir, session_id, bodies)?;
                continue;
            }
            TurnStart::Input(bodies) => bodies,
            TurnStart::Resume => Vec::new(),
        };
        let launch = driver.brain_launch(session_id);
        // Safety invariant (design §7): the gait-dir holds the agent's own config,
        // permissions, and audit log. If it is inside the codebase it is inside the
        // writable sandbox scope, so the model could rewrite its permissions /
        // disable its sandbox / edit its audit trail. Refuse here — spine-side,
        // before the sealed brain spawns, so `allow_intree` never crosses the seal.
        if launch.sandbox_on
            && !launch.allow_intree
            && gait_dir_inside_codebase(gait_dir, &launch.codebase)
        {
            anyhow::bail!(
                "gait-dir {} is inside the codebase {} — the agent could write to its \
                 own config. Move it out of tree (`gait init ~/gait-home && \
                 gait link ~/gait-home`), or pass --allow-intree.",
                gait_dir.display(),
                launch.codebase.display()
            );
        }
        let failed = crate::turn::spine::relay_brain_turn(
            gait_dir,
            &launch.codebase,
            session_id,
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
            &crate::turn::spine::TurnMode::Persistent { trigger },
            launch.fs_delegate.clone(),
        )
        .await;
        // Clear the cascade slot after each turn (parity with spine.rs:248).
        if let Some(handle) = cascade {
            *handle.0.lock().unwrap() = None;
        }
        match failed {
            Ok(result)
                if matches!(
                    result.state,
                    Some(crate::proto::gait::log::v1::run_result::State::Failed(_))
                ) =>
            {
                if driver
                    .on_turn_error(sink, anyhow::anyhow!("brain turn failed"))
                    .is_break()
                {
                    break;
                }
            }
            Ok(result) => {
                driver.after_turn(sink, &result).await;
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

/// Resolve a live profile for embedders that already hold a typed credential.
/// Raw API keys are represented only as `ResolvedCredential::ApiKey`; callers
/// cannot accidentally bypass subscription-aware provider construction.
#[allow(clippy::too_many_arguments)]
pub async fn resolve_profile(
    config: &crate::config::GaitConfig,
    gait_dir: &Path,
    codebase: &Path,
    session_id: &str,
    agent_path: Option<&Path>,
    inference_override: Option<String>,
    repos: &crate::git::Repos,
    credential: Option<gait_auth::ResolvedCredential>,
) -> anyhow::Result<crate::profile::ResolvedProfile> {
    let profile = crate::profile::Profile::from_config_and_agent(
        config,
        gait_dir,
        Some(codebase),
        agent_path,
        inference_override,
    );
    profile
        .resolve(
            codebase.to_path_buf(),
            session_id.to_string(),
            gait_dir.to_path_buf(),
            repos.clone(),
            credential,
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
    fn result(&mut self, r: &RunResult) {
        if let Ok(j) = serde_json::to_string(r) {
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
    exe: &std::path::Path,
    sandbox_cfg: &crate::config::SandboxConfig,
    sandbox_on: bool,
    inference: crate::proto::gait::log::v1::Inference,
    input_text: String,
    schema: serde_json::Value,
) -> anyhow::Result<RunResult> {
    // The brain requires a --gait-dir; a scratch tempdir is never written to.
    let scratch = tempfile::tempdir()?;
    let session_id = "ephemeral";

    // Seed the in-memory transcript with the user input event.
    let body = crate::proto::events::user_text(input_text);
    let input = vec![crate::proto::envelope::mint(
        body,
        None,
        crate::proto::envelope::EventScope::Session,
    )];

    let profile_inputs = crate::turn::spine::ProfileInputs {
        agent_name: None,
        inference_override: None,
        resolved_inference: Some(inference),
        explicit_inference_override: true,
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
        None,
    )
    .await?;

    sink.results
        .pop()
        .ok_or_else(|| anyhow::anyhow!("model returned no structured result"))
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;

    /// Compile-time + behavior test: the trait is `dyn`-usable (object-safe via
    /// `async_trait::async_trait`) and `read_text_file` returns `Ok(None)` /
    /// `Ok(Some(_))` correctly. Required by the fs-delegation seam: every embedder
    /// that injects an `FsDelegate` stores it as `Arc<dyn FsDelegate>`.
    #[tokio::test]
    async fn fs_delegate_is_dyn_object_safe() {
        use async_trait::async_trait;

        struct Fake;
        #[async_trait]
        impl super::FsDelegate for Fake {
            async fn read_text_file(&self, _p: &Path) -> anyhow::Result<Option<String>> {
                Ok(Some("x".into()))
            }
            async fn write_text_file(&self, _p: &Path, _c: &str) -> anyhow::Result<()> {
                Ok(())
            }
        }
        let d: Arc<dyn super::FsDelegate> = Arc::new(Fake);
        assert_eq!(
            d.read_text_file(Path::new("/a")).await.unwrap().as_deref(),
            Some("x")
        );
    }
}

//! Brain runtime: the sealed, per-turn inference process.
//!
//! The brain is spawned by the spine (`gait __brain`), self-seals to
//! `{read_all: false, readable: [], writable: [], network: All}`, and runs one turn via
//! `embed::run_loop`. All disk writes go through the spine via Nerve signals
//! (Append for events, Spawn for process launches, StreamForward for streaming).
//!
//! This module owns:
//! - `brain_scope()` — the seal spec: compute + network only, no disk reads
//! - `BrainSink` — a TurnSink that routes output to stderr (stdout is the Nerve)
//! - `BrainDriver` — a TurnDriver that wires the Nerve + remote-sink Recorder
//! - `run_brain()` — the brain's entry point (seal → nerve → run_turn → Terminated)

use std::path::PathBuf;

use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

use crate::embed::LoopConfig;
use crate::port::output::{CliSink, Diag, TurnSink};
use crate::proto::gait::log::v1::{Event, StreamItem};
use crate::proto::Recorder;
use crate::turn::nerve::{self, NerveWriter};

use gait_proto::gait::sandbox::v1::{
    signal, Append, Diagnostic, Signal, StreamForward, Terminated,
};

// `brain_scope()` now lives in `gait-sandbox::sandbox` (re-exported at the crate
// root as `crate::sandbox::brain_scope`) — it constructs a `SandboxScope`, so it
// belongs in `sandbox`, not here.
pub use crate::sandbox::brain_scope;

// The ping and Nerve feedback-reader tasks live for the lifetime of the brain
// process. These four relays are different: once their senders are dropped,
// they drain a finite queue and exit. Keeping their handles lets shutdown wait
// for every queued event before it publishes the terminal signal.
struct BrainRelaySenders {
    event_tx: mpsc::UnboundedSender<Event>,
    stream_fwd_tx: mpsc::UnboundedSender<StreamItem>,
    spawn_tx: mpsc::UnboundedSender<(Vec<String>, bool)>,
}

struct BrainRelayTasks {
    handles: Vec<tokio::task::JoinHandle<()>>,
}

impl BrainRelayTasks {
    async fn drain(self) {
        for handle in self.handles {
            let _join_result = handle.await;
        }
    }
}

async fn send_terminated_after_relays(
    relay_tasks: BrainRelayTasks,
    nerve_writer: &NerveWriter,
    terminated: &Signal,
) {
    relay_tasks.drain().await;
    nerve_writer.send_one_way(terminated).await.ok();
}

fn start_brain_relays(nerve_writer: &NerveWriter) -> (BrainRelaySenders, BrainRelayTasks) {
    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<Event>();
    let relay_writer = nerve_writer.clone();
    let append_handle = tokio::spawn(async move {
        while let Some(evt) = event_rx.recv().await {
            let sig = Signal {
                corr_id: 0,
                body: Some(signal::Body::Append(Append { event: Some(evt) })),
            };
            relay_writer.send_one_way(&sig).await.ok();
        }
    });

    let (stream_fwd_tx, mut stream_fwd_rx) = mpsc::unbounded_channel::<StreamItem>();
    let stream_relay_writer = nerve_writer.clone();
    let stream_handle = tokio::spawn(async move {
        while let Some(item) = stream_fwd_rx.recv().await {
            let sig = Signal {
                corr_id: 0,
                body: Some(signal::Body::StreamForward(StreamForward {
                    item: Some(item),
                })),
            };
            stream_relay_writer.send_one_way(&sig).await.ok();
        }
    });

    let (spawn_tx, mut spawn_rx) = mpsc::unbounded_channel::<(Vec<String>, bool)>();
    let spawn_relay_writer = nerve_writer.clone();
    let spawn_handle = tokio::spawn(async move {
        while let Some((argv, detached)) = spawn_rx.recv().await {
            let sig = Signal {
                corr_id: 0,
                body: Some(signal::Body::Spawn(gait_proto::gait::sandbox::v1::Spawn {
                    argv,
                    detached,
                })),
            };
            spawn_relay_writer.send_one_way(&sig).await.ok();
        }
    });

    (
        BrainRelaySenders {
            event_tx,
            stream_fwd_tx,
            spawn_tx,
        },
        BrainRelayTasks {
            handles: vec![append_handle, stream_handle, spawn_handle],
        },
    )
}

// ── BrainConfig ───────────────────────────────────────────────────────────────

/// Configuration extracted from CLI args, passed to `run_brain`.
/// Defined in core so `run_brain` doesn't depend on clap or gait-cli.
#[derive(Debug, Clone)]
pub struct BrainConfig {
    pub session_id: Option<String>,
    pub auto: bool,
    pub allow: Vec<String>,
    pub deny: Vec<String>,
    pub max_iter: Option<usize>,
    pub sandbox_locked: bool,
    pub no_sandbox: bool,
    pub sandbox: bool,
}

// ── run_brain ─────────────────────────────────────────────────────────────────

/// The brain's entry point. Called by `cmd_brain` in gait-cli after arg parsing.
///
/// 1. Apply the brain seal (read-all, zero writes, outbound network).
/// 2. Start the Nerve (stdin → Feedback, stdout → Signals).
/// 3. Spawn relay tasks (Append, StreamForward, Spawn, Notify).
/// 4. Load session + profile, create BrainDriver + BrainSink.
/// 5. Call `turn::run_turn`.
/// 6. Send Terminated signal to the spine.
pub async fn run_brain(config: BrainConfig, gait_dir: PathBuf, codebase_path: PathBuf) {
    // ── Apply brain seal ──────────────────────────────────────────────────────
    let seal_status = crate::sandbox::platform_sandbox()
        .apply(&brain_scope())
        .unwrap_or(crate::sandbox::SandboxStatus::Unavailable {
            reason: "apply() returned Err".to_string(),
        });
    let _seal_enforcement = match &seal_status {
        crate::sandbox::SandboxStatus::Active => "active",
        crate::sandbox::SandboxStatus::Noop => "noop",
        crate::sandbox::SandboxStatus::Unavailable { .. } => "unavailable",
    };

    // Read BrainInit as the first frame. MUST happen before start_nerve takes
    // ownership of stdin (start_nerve's demux reader would otherwise consume and
    // silently drop this unsolicited corr_id:0 frame).
    let mut stdin = tokio::io::stdin();
    let init = match nerve::read_signal(&mut stdin).await {
        Ok(sig) => match sig.body {
            Some(gait_proto::gait::sandbox::v1::signal::Body::BrainInit(b)) => b,
            other => {
                #[allow(clippy::print_stderr)]
                // tracked debt: sealed-brain stderr bypasses the Nerve/event channel — see docs/superpowers/plans/2026-07-05-repl-stderr-race-fix.md
                {
                    eprintln!("[brain] error: expected BrainInit as first frame, got {other:?}");
                }
                std::process::exit(1);
            }
        },
        Err(e) => {
            #[allow(clippy::print_stderr)]
            // tracked debt: sealed-brain stderr bypasses the Nerve/event channel — see docs/superpowers/plans/2026-07-05-repl-stderr-race-fix.md
            {
                eprintln!("[brain] error: failed to read BrainInit frame: {e}");
            }
            std::process::exit(1);
        }
    };

    // Brain reads Feedback from stdin (spine→brain), writes Signals to stdout (brain→spine).
    let (nerve_writer, _nerve_handle) = nerve::start_nerve(stdin, tokio::io::stdout());

    // Recorder and harness operations send through finite relay queues. Their
    // tasks are retained so shutdown can drain them before Terminated.
    let (relay_senders, relay_tasks) = start_brain_relays(&nerve_writer);
    let BrainRelaySenders {
        event_tx,
        stream_fwd_tx,
        spawn_tx,
    } = relay_senders;

    // Emit the brain-seal SandboxApplied event as the first Append signal.
    {
        let reason = match &seal_status {
            crate::sandbox::SandboxStatus::Unavailable { reason } => Some(reason.clone()),
            _ => None,
        };
        let evt_body = crate::proto::events::sandbox_applied(_seal_enforcement, vec![], reason);
        let evt =
            gait_proto::envelope::mint(evt_body, None, gait_proto::envelope::EventScope::Session);
        let _ = event_tx.send(evt);
    }

    let session_id = match config.session_id.clone() {
        Some(id) => id,
        None => {
            #[allow(clippy::print_stderr)]
            // tracked debt: sealed-brain stderr bypasses the Nerve/event channel — see docs/superpowers/plans/2026-07-05-repl-stderr-race-fix.md
            {
                eprintln!("[brain] error: --session is required for __brain");
            }
            std::process::exit(1);
        }
    };

    // Install the process-global phase cell and spawn the always-on ping task.
    let _phase_cell = crate::diagnostic::install_brain_phase_cell();
    // Identity cell (turn + model) — seeded here, updated by `setup::init` once
    // the turn number and model are resolved, so the ping below carries the same
    // turn/model that `log.jsonl` stamps instead of `0`/`""`.
    crate::diagnostic::install_brain_identity();
    let ping_writer = nerve_writer.clone();
    let ping_session = session_id.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_millis(15_000));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            let Some((phase, elapsed_ms, label)) = crate::diagnostic::current_phase_full() else {
                continue;
            };
            let (turn, model) = crate::diagnostic::current_identity().unwrap_or((0, String::new()));
            let mut rec =
                crate::diagnostic::DiagRecord::ping(&ping_session, turn, &model, phase, elapsed_ms);
            rec.detail = label;
            let sig = Signal {
                corr_id: 0,
                body: Some(signal::Body::Diagnostic(Diagnostic {
                    record_json: rec.to_json_line(),
                    phase: crate::diagnostic::phase_str(phase).to_string(),
                    elapsed_ms,
                })),
            };
            if ping_writer.send_one_way(&sig).await.is_err() {
                break;
            }
        }
    });

    let mut cfg: crate::config::GaitConfig = serde_json::from_str(&init.config_json)
        .unwrap_or_else(|e| {
            #[allow(clippy::print_stderr)]
            // tracked debt: sealed-brain stderr bypasses the Nerve/event channel — see docs/superpowers/plans/2026-07-05-repl-stderr-race-fix.md
            {
                eprintln!("[brain] warning: bad config_json in BrainInit ({e}); using defaults");
            }
            crate::config::GaitConfig::default()
        });
    for p in &config.allow {
        if !cfg.allow.contains(p) {
            cfg.allow.push(p.clone());
        }
    }
    for p in &config.deny {
        if !cfg.deny.contains(p) {
            cfg.deny.push(p.clone());
        }
    }
    if let Some(n) = config.max_iter {
        cfg.max_iter = Some(n);
    }
    if config.sandbox_locked {
        cfg.sandbox.on = true;
    } else if config.no_sandbox {
        cfg.sandbox.on = false;
    } else if config.sandbox {
        cfg.sandbox.on = true;
    }

    let profile: crate::profile::Profile =
        serde_json::from_str(&init.profile_json).unwrap_or_else(|e| {
            #[allow(clippy::print_stderr)]
            // tracked debt: sealed-brain stderr bypasses the Nerve/event channel — see docs/superpowers/plans/2026-07-05-repl-stderr-race-fix.md
            {
                eprintln!("[brain] error: bad profile_json in BrainInit: {e}");
            }
            std::process::exit(1);
        });
    let credential = parse_bootstrap_credential(&init.credential_json).unwrap_or_else(|error| {
        #[allow(clippy::print_stderr)]
        {
            eprintln!("[brain] error: invalid private credential bootstrap: {error}");
        }
        std::process::exit(1);
    });

    let repos = crate::git::Repos::production();

    // Seed the log mirror from BrainInit.events and derive session identity in-memory.
    let log_mirror = std::sync::Arc::new(std::sync::Mutex::new(init.events.clone()));
    let meta = crate::session::session_meta_from_events(&init.events, &session_id);
    let initial_git = crate::turn::GitSnapshot {
        head_hash: init
            .git
            .as_ref()
            .map(|g| g.head_hash.clone())
            .unwrap_or_default(),
        dirty_files: init
            .git
            .as_ref()
            .map(|g| g.dirty_files.clone())
            .unwrap_or_default(),
    };
    let daemons: Vec<(String, i32, bool, String)> = init
        .daemons
        .iter()
        .map(|d| (d.name.clone(), d.pid, d.alive, d.log_path.clone()))
        .collect();

    let session = std::sync::Arc::new(crate::session::Session {
        id: session_id.clone(),
        gait_dir: gait_dir.clone(),
        codebase_path: meta
            .codebase_path
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| codebase_path.clone()),
        assigned_by: meta.assigned_by,
        repos: repos.clone(),
    });

    let mut driver = BrainDriver {
        profile: Some(profile),
        codebase: codebase_path,
        session_id: session_id.clone(),
        gait_dir: gait_dir.clone(),
        repos,
        credential,
        auto: config.auto,
        max_iter: cfg.max_iter,
        preview_policy: cfg.preview,
        sandbox_on: cfg.sandbox.on,
        sandbox_cfg: cfg.sandbox.clone(),
        nerve_writer: nerve_writer.clone(),
        event_tx,
        stream_fwd_tx,
        spawn_tx,
        log_mirror,
        initial_git,
        daemons,
        cancel: tokio_util::sync::CancellationToken::new(),
    };

    // Teardown SIGTERM guard (mirrors `crate::turn::hand::install_sigterm_guard`,
    // but cancels the live turn gracefully instead of hard-exiting — the brain
    // owns an in-flight `run_turn`, not a short-lived tool call). The spine's
    // `kill_all` sends this SIGTERM before its 300ms→SIGKILL escalation; wiring
    // it to `driver.cancel` lets `run_turn` observe cancellation at its checked
    // points and return `TurnResult::Cancelled` instead of being signal-killed.
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        if let Ok(mut term) = signal(SignalKind::terminate()) {
            let cancel = driver.cancel.clone();
            tokio::spawn(async move {
                term.recv().await;
                cancel.cancel();
            });
        }
    }

    let mut sink = BrainSink {
        inner: CliSink::new(std::io::stderr(), std::io::stderr(), false),
    };

    // ── Brain rewire (Task 2 sub-step a): call run_turn directly ─────────────
    // The brain runs exactly one turn. It no longer calls run_loop (which would
    // spawn another brain — a fork bomb). Instead it drives run_turn in-process,
    // using the exact per-turn wrapper that run_loop used to provide.
    //
    // MUST OMIT: create_session_worktree + wire::append — both are gait-dir writes
    // forbidden by the brain's seal (writable: []); the spine already did them.
    //
    // MUST KEEP: relay handle in turn_config, make_recorder, drive, sink.result,
    // after_turn (Scheduler/Harness post-turn policy).
    let (run_result_json, failed) = {
        // 1. Resolve profile (single-shot).
        let profile_result = driver.resolve_profile().await;
        match profile_result {
            Err(e) => {
                #[allow(clippy::print_stderr)]
                // tracked debt: sealed-brain stderr bypasses the Nerve/event channel — see docs/superpowers/plans/2026-07-05-repl-stderr-race-fix.md
                {
                    eprintln!("[brain] error resolving profile: {e}");
                }
                (None, true)
            }
            Ok(profile) => {
                // 2. Build TurnInput from turn_config (KEEP relay handle — required for world tools).
                let cfg = driver.turn_config();
                let input = crate::turn::TurnInput {
                    auto: cfg.auto,
                    max_iter: cfg.max_iter,
                    preview_policy: cfg.preview_policy,
                    cancel: cfg.cancel,
                    sandbox_on: cfg.sandbox_on,
                    sandbox_cfg: cfg.sandbox_cfg.clone(),
                    relay: cfg.relay,
                    initial_git: cfg.initial_git.clone(),
                    daemons: cfg.daemons.clone(),
                };
                // 3. Build remote-sink Recorder (streams via Nerve, not to disk).
                let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
                let emitter = driver.make_recorder(&gait_dir, &session_id, tx);
                // 4. Spawn run_turn; drain the stream channel; collect result.
                crate::diagnostic::set_phase(crate::diagnostic::Phase::TurnBegin);
                let session_for_turn = session.clone();
                let handle = tokio::spawn(async move {
                    crate::turn::run_turn(&session_for_turn, &emitter, profile, input).await
                });
                sink.begin_turn();
                // drive() drains rx so emitter drops, closing the channel.
                // NOTE: for the brain the Recorder emits via stream_fwd (Nerve), so rx is
                // inert here — BrainSink::item is a no-op. Keep drive() anyway: it must
                // run so the emitter can drop and allow run_turn's task to complete.
                crate::port::output::drive(rx, &mut sink).await;
                sink.end_turn();
                match handle.await {
                    Ok(Ok(result)) => {
                        // Failed turns skip the success epilogue (after_turn /
                        // Scheduler/Harness policy) — the previous Fail/MaxSteps
                        // paths returned Err and hit the Ok(Err) arm, which never
                        // ran after_turn. Preserve that invariant.
                        if let crate::turn::TurnResult::Failed { message, .. } = &result {
                            sink.diagnostic(Diag::Error, &format!("error: {message}"));
                        }
                        let failed = sink.inner.failed;
                        let draft = crate::turn::spine::finalized_turn(&result);
                        crate::diagnostic::set_phase(crate::diagnostic::Phase::TurnEnd);
                        // Brain runs a single turn; after TurnEnd it awaits the
                        // Terminated signal being consumed by the spine (AwaitFeedback
                        // is the closest brain-owned seam before that implicit wait).
                        crate::diagnostic::set_phase(crate::diagnostic::Phase::AwaitFeedback);
                        (Some(draft), failed)
                    }
                    Ok(Err(e)) => {
                        sink.diagnostic(Diag::Error, &format!("error: {e}"));
                        (None, true)
                    }
                    Err(panic) => {
                        sink.diagnostic(Diag::Error, &format!("turn task panicked: {panic}"));
                        (None, true)
                    }
                }
            }
        }
    };

    // BrainDriver owns the persistent sender clones. Dropping it closes all
    // four finite queues once any turn-local clones have gone away; awaiting
    // their tasks guarantees Append/StreamForward/Spawn/Notify signals already
    // queued by finalization are written before Terminated can overtake them.
    drop(driver);
    let terminated = Signal {
        corr_id: 0,
        body: Some(signal::Body::Terminated(Terminated {
            turn: run_result_json,
            failed,
        })),
    };
    send_terminated_after_relays(relay_tasks, &nerve_writer, &terminated).await;
    // Drop the nerve writer to close stdout — this signals EOF to the spine's
    // relay loop as a fallback if it's still reading (in practice the spine
    // already breaks its loop on receiving Terminated itself, per spine.rs;
    // this drop is a defensive no-op on that path). The explicit exit below,
    // not this EOF signal, is what actually ends this process.
    drop(nerve_writer);
    let _ = tokio::io::stdout().flush().await;

    // Explicit exit, not a plain return: the long-lived Nerve feedback reader
    // and 15s ping loop only stop when their pipes close. The finite relays have
    // already drained above, so forced exit cannot discard queued turn events.
    std::process::exit(0);
}

// ── BrainSink ─────────────────────────────────────────────────────────────────

/// A TurnSink that routes output to stderr (brain's stdout is the Nerve pipe)
/// Diagnostics-only sink for the sealed brain. The brain returns a transient
/// FinalizedTurn directly; aggregate RunResult emission belongs to the spine.
struct BrainSink {
    inner: CliSink<std::io::Stderr, std::io::Stderr>,
}

impl TurnSink for BrainSink {
    fn item(&mut self, _item: &StreamItem) {
        // No-op: brain routes StreamItems via StreamForward Signals to the spine.
    }
    fn result(&mut self, _r: &crate::proto::gait::log::v1::RunResult) {}
    fn session_id(&mut self, _id: &str) {
        // Spine handles session_id announcement.
    }
    fn diagnostic(&mut self, level: Diag, msg: &str) {
        self.inner.diagnostic(level, msg);
    }
    fn begin_turn(&mut self) {
        self.inner.begin_turn();
    }
    fn end_turn(&mut self) {
        self.inner.end_turn();
    }
}

// ── BrainDriver ───────────────────────────────────────────────────────────────

struct BrainDriver {
    profile: Option<crate::profile::Profile>,
    codebase: PathBuf,
    session_id: String,
    gait_dir: PathBuf,
    repos: crate::git::Repos,
    credential: Option<gait_auth::ResolvedCredential>,
    auto: bool,
    max_iter: Option<usize>,
    preview_policy: crate::config::PreviewPolicy,
    sandbox_on: bool,
    sandbox_cfg: crate::config::SandboxConfig,
    nerve_writer: NerveWriter,
    event_tx: mpsc::UnboundedSender<Event>,
    stream_fwd_tx: mpsc::UnboundedSender<StreamItem>,
    spawn_tx: mpsc::UnboundedSender<(Vec<String>, bool)>,
    log_mirror: std::sync::Arc<std::sync::Mutex<Vec<crate::proto::gait::log::v1::Event>>>,
    initial_git: crate::turn::GitSnapshot,
    daemons: Vec<(String, i32, bool, String)>,
    /// Live per-turn cancellation token (Task: wire turn cancellation). A
    /// teardown SIGTERM (sent by the spine's `kill_all` before its 300ms→SIGKILL
    /// escalation — see `crate::turn::hand::install_sigterm_guard` for the same
    /// signal on the hand side) cancels this instead of the process dying by
    /// default disposition, so the in-flight turn gets a chance to finish via
    /// `TurnResult::Cancelled` instead of being hard-killed.
    cancel: tokio_util::sync::CancellationToken,
}

/// BrainDriver is a plain `pub(crate)` struct — NOT a TurnDriver impl.
/// The brain calls run_turn directly (Task 2 sub-step a); it no longer uses run_loop.
/// These are inherent methods called directly from run_brain.
impl BrainDriver {
    async fn resolve_profile(&mut self) -> anyhow::Result<crate::profile::ResolvedProfile> {
        let profile = self.profile.take().ok_or_else(|| {
            anyhow::anyhow!("BrainDriver::resolve_profile called more than once (single-shot)")
        })?;
        profile
            .resolve(
                self.codebase.clone(),
                self.session_id.clone(),
                self.gait_dir.clone(),
                self.repos.clone(),
                self.credential.clone(),
            )
            .await
    }

    fn turn_config(&mut self) -> LoopConfig {
        LoopConfig {
            auto: self.auto,
            max_iter: self.max_iter,
            preview_policy: self.preview_policy,
            cancel: Some(self.cancel.clone()),
            sandbox_on: self.sandbox_on,
            sandbox_cfg: self.sandbox_cfg.clone(),
            relay: Some(self.nerve_writer.clone()),
            initial_git: Some(self.initial_git.clone()),
            daemons: self.daemons.clone(),
        }
    }

    fn make_recorder(
        &self,
        _gait_dir: &std::path::Path,
        _session_id: &str,
        _stream_tx: mpsc::UnboundedSender<StreamItem>,
    ) -> Recorder {
        Recorder::remote_sink(self.stream_fwd_tx.clone(), self.event_tx.clone())
            .with_spawn_tx(self.spawn_tx.clone())
            .with_log_mirror(self.log_mirror.clone())
    }
}

fn parse_bootstrap_credential(
    value: &str,
) -> anyhow::Result<Option<gait_auth::ResolvedCredential>> {
    if value.is_empty() {
        return Ok(None);
    }
    let value: serde_json::Value = serde_json::from_str(value)?;
    match value.get("kind").and_then(serde_json::Value::as_str) {
        Some("api_key") => value
            .get("secret")
            .and_then(serde_json::Value::as_str)
            .filter(|key| !key.is_empty())
            .map(|key| Ok(gait_auth::ResolvedCredential::ApiKey(key.to_string())))
            .unwrap_or_else(|| Err(anyhow::anyhow!("empty API key")))
            .map(Some),
        Some("subscription") => serde_json::from_value(
            value
                .get("credential")
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("missing subscription credential"))?,
        )
        .map(gait_auth::ResolvedCredential::Subscription)
        .map(Some)
        .map_err(Into::into),
        _ => Err(anyhow::anyhow!("unknown credential kind")),
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use crate::proto::gait::log::v1::{payload, stream_item, StreamItem};
    use gait_proto::gait::sandbox::v1::{signal, Signal, Terminated};

    #[test]
    fn brain_init_seeds_identity_and_config() {
        let cfg = crate::config::GaitConfig::default();
        let cfg_json = serde_json::to_string(&cfg).unwrap();
        let parsed: crate::config::GaitConfig = serde_json::from_str(&cfg_json).unwrap();
        assert_eq!(parsed.max_iter, cfg.max_iter);

        let ev = gait_proto::envelope::mint(
            crate::proto::events::session_start(
                "s1234567".into(),
                None,
                Some("/code".into()),
                false,
            ),
            None,
            gait_proto::envelope::EventScope::Session,
        );
        let meta = crate::session::session_meta_from_events(&[ev], "s1234567");
        assert_eq!(meta.codebase_path.as_deref(), Some("/code"));
    }

    /// Wire turn cancellation (2026-07-06 plan): `BrainDriver::turn_config()`
    /// must hand `run_turn` a live, cancel-able token — not the hard-coded
    /// `None` that made the graceful `TurnResult::Cancelled` path unreachable
    /// in practice. Cancelling the driver's token and then reading it back off
    /// a fresh `turn_config()` call proves the same live token round-trips
    /// through `LoopConfig`, which is what the brain's SIGTERM guard (see
    /// `run_brain`) relies on to make an external teardown signal observable
    /// to the in-flight turn.
    #[tokio::test]
    async fn brain_driver_turn_config_carries_live_cancel_token() {
        let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
        let (stream_fwd_tx, _stream_fwd_rx) = tokio::sync::mpsc::unbounded_channel();
        let (spawn_tx, _spawn_rx) = tokio::sync::mpsc::unbounded_channel();
        // `start_nerve` just needs *some* AsyncRead/AsyncWrite pair to build a
        // `NerveWriter` around — this test never sends/receives real Signals,
        // so an inert reader/sink is enough.
        let (nerve_writer, _nerve_reader_handle) =
            crate::turn::nerve::start_nerve(tokio::io::empty(), tokio::io::sink());

        let cancel = tokio_util::sync::CancellationToken::new();
        let mut driver = super::BrainDriver {
            profile: None,
            codebase: std::path::PathBuf::from("/tmp/cb"),
            session_id: "s0000000".to_string(),
            gait_dir: std::path::PathBuf::from("/tmp/gd"),
            repos: crate::git::Repos::production(),
            credential: None,
            auto: false,
            max_iter: None,
            preview_policy: crate::config::PreviewPolicy::default(),
            sandbox_on: false,
            sandbox_cfg: crate::config::SandboxConfig::default(),
            nerve_writer,
            event_tx,
            stream_fwd_tx,
            spawn_tx,
            log_mirror: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            initial_git: crate::turn::GitSnapshot::default(),
            daemons: Vec::new(),
            cancel: cancel.clone(),
        };

        let cfg_before = driver.turn_config();
        assert!(
            !cfg_before.cancel.as_ref().unwrap().is_cancelled(),
            "fresh token must not report cancelled"
        );

        // Cancel via the *external* handle the SIGTERM guard would hold — not
        // through the LoopConfig clone — proving it's the same live token,
        // not an inert `CancellationToken::new()` stand-in.
        cancel.cancel();

        let cfg_after = driver.turn_config();
        assert!(
            cfg_after.cancel.as_ref().unwrap().is_cancelled(),
            "turn_config() must hand out a clone of the driver's live token, \
             so external cancellation is observable to a subsequent turn build"
        );
    }

    #[tokio::test]
    async fn finite_relays_flush_events_before_terminated() {
        let (brain_write, mut spine_read) = tokio::io::duplex(64 * 1024);
        let (nerve_writer, _reader_handle) =
            crate::turn::nerve::start_nerve(tokio::io::empty(), brain_write);
        let (senders, relay_tasks) = super::start_brain_relays(&nerve_writer);

        let relayed = gait_proto::envelope::mint(
            crate::proto::events::assistant_text("settled".into()),
            None,
            gait_proto::envelope::EventScope::Step {
                run: 0,
                turn: 0,
                step: 0,
            },
        );
        senders
            .event_tx
            .send(relayed.clone())
            .expect("append relay must be open");
        senders
            .stream_fwd_tx
            .send(StreamItem {
                item: Some(stream_item::Item::Event(relayed)),
            })
            .expect("stream relay must be open");

        // Production drops BrainDriver at this point. The test drops the
        // equivalent sender owner directly so every finite relay can finish.
        drop(senders);

        let terminated = Signal {
            corr_id: 0,
            body: Some(signal::Body::Terminated(Terminated {
                turn: Some(Default::default()),
                failed: false,
            })),
        };
        super::send_terminated_after_relays(relay_tasks, &nerve_writer, &terminated).await;

        let mut saw_append = false;
        let mut saw_stream = false;
        for _ in 0..3 {
            let signal = tokio::time::timeout(
                std::time::Duration::from_secs(1),
                crate::turn::nerve::read_signal(&mut spine_read),
            )
            .await
            .expect("timed out waiting for relayed signal")
            .expect("valid relayed signal");
            match signal.body {
                Some(signal::Body::Append(append)) => {
                    saw_append = append.event.is_some_and(|event| {
                        matches!(
                            crate::proto::event_kind(&event),
                            Some(payload::Kind::AssistantText(_))
                        )
                    });
                }
                Some(signal::Body::StreamForward(forward)) => {
                    saw_stream = forward.item.is_some_and(|item| {
                        matches!(
                            item.item,
                            Some(stream_item::Item::Event(ref event))
                                if matches!(
                                    crate::proto::event_kind(event),
                                    Some(payload::Kind::AssistantText(_))
                                )
                        )
                    });
                }
                Some(signal::Body::Terminated(_)) => {
                    assert!(
                        saw_append && saw_stream,
                        "Terminated must follow both durable and streamed event relays"
                    );
                    return;
                }
                other => panic!("unexpected signal before termination: {other:?}"),
            }
        }
        panic!("missing Terminated signal");
    }
}

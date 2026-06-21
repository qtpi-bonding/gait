//! Brain runtime: the sealed, per-turn inference process.
//!
//! The brain is spawned by the spine (`gait __brain`), self-seals to
//! `{read_all: true, writable: [], network: All}`, and runs one turn via
//! `embed::run_loop`. All disk writes go through the spine via Nerve signals
//! (Append for events, Spawn for process launches, StreamForward for streaming).
//!
//! This module owns:
//! - `brain_scope()` — the seal spec (what the brain can do)
//! - `BrainSink` — a TurnSink that routes output to stderr (stdout is the Nerve)
//! - `BrainDriver` — a TurnDriver that wires the Nerve + remote-sink Recorder
//! - `run_brain()` — the brain's entry point (seal → nerve → run_loop → Terminated)

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::mpsc;

use crate::embed::{LoopConfig, TurnDriver};
use crate::port::output::{CliSink, Diag, TurnSink};
use crate::proto::gait::log::v1::{Event, StreamItem};
use crate::proto::Recorder;
use crate::turn::nerve::{self, NerveWriter};
use crate::workflow_runner::WorkflowRunner;

use gait_proto::gait::sandbox::v1::{signal, Append, Signal, StreamForward, Terminated};

// ── brain_scope ───────────────────────────────────────────────────────────────

/// Build the brain's self-seal scope: read-all + zero writes + full outbound network.
///
/// Validated spike (macOS 15 / Darwin 24.5.0):
///   `(deny default)(allow file-read*)(allow process*)(allow network-outbound)
///    (allow system-socket)(allow mach-lookup …)`
/// DNS + TCP to api.anthropic.com:443 succeed under this policy with
/// reqwest+rustls+bundled webpki-roots (no cert-store reads needed).
pub fn brain_scope() -> crate::sandbox::SandboxScope {
    crate::sandbox::SandboxScope {
        read_all: true,
        readable: vec![],
        writable: vec![],
        network: crate::sandbox::NetworkPolicy::All,
    }
}

// ── BrainConfig ───────────────────────────────────────────────────────────────

/// Configuration extracted from CLI args, passed to `run_brain`.
/// Defined in core so `run_brain` doesn't depend on clap or gait-cli.
#[derive(Debug, Clone)]
pub struct BrainConfig {
    pub session_id: Option<String>,
    pub auto: bool,
    pub model_override: Option<String>,
    pub agent_name: Option<String>,
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
/// 3. Spawn relay tasks (Append, StreamForward, Spawn).
/// 4. Load session + profile, create BrainDriver + BrainSink.
/// 5. Call `embed::run_loop`.
/// 6. Send Terminated signal to the spine.
pub async fn run_brain(
    config: BrainConfig,
    gait_dir: PathBuf,
    codebase_path: PathBuf,
    api_key: Option<String>,
    workflow_runner: Option<Arc<dyn WorkflowRunner>>,
) {
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

    // Brain reads Feedback from stdin (spine→brain), writes Signals to stdout (brain→spine).
    let (nerve_writer, _nerve_handle) = nerve::start_nerve(tokio::io::stdin(), tokio::io::stdout());

    // Channel for AppendRelay: Recorder sends Events here; relay task forwards as Append signals.
    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<Event>();

    // Emit the brain-seal SandboxApplied event as the first Append signal.
    {
        let reason = match &seal_status {
            crate::sandbox::SandboxStatus::Unavailable { reason } => Some(reason.clone()),
            _ => None,
        };
        let evt_body = crate::proto::events::sandbox_applied(_seal_enforcement, vec![], reason);
        let evt = gait_proto::envelope::mint(evt_body, None, 0, 0);
        let _ = event_tx.send(evt);
    }

    // AppendRelay task: drain event channel, send Append Signals to spine.
    let relay_writer = nerve_writer.clone();
    tokio::spawn(async move {
        while let Some(evt) = event_rx.recv().await {
            let sig = Signal {
                corr_id: 0,
                body: Some(signal::Body::Append(Append { event: Some(evt) })),
            };
            relay_writer.send_one_way(&sig).await.ok();
        }
    });

    // StreamForward relay: Recorder's stream_tx feeds this; relay task sends
    // StreamForward Signals to the spine so it can call sink.item().
    let (stream_fwd_tx, mut stream_fwd_rx) = mpsc::unbounded_channel::<StreamItem>();
    let stream_relay_writer = nerve_writer.clone();
    tokio::spawn(async move {
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

    // Spawn relay: tools (spawn_agent) and harness send argv here; relay task
    // forwards as Spawn Signals so the spine (not the sealed brain) executes
    // the child process. Spine appends --sandbox-locked when sandbox_on.
    let (spawn_tx, mut spawn_rx) = mpsc::unbounded_channel::<Vec<String>>();
    let spawn_relay_writer = nerve_writer.clone();
    tokio::spawn(async move {
        while let Some(argv) = spawn_rx.recv().await {
            let sig = Signal {
                corr_id: 0,
                body: Some(signal::Body::Spawn(gait_proto::gait::sandbox::v1::Spawn {
                    argv,
                })),
            };
            spawn_relay_writer.send_one_way(&sig).await.ok();
        }
    });

    let session_id = match config.session_id.clone() {
        Some(id) => id,
        None => {
            eprintln!("[brain] error: --session is required for __brain");
            std::process::exit(1);
        }
    };

    let mut cfg = crate::config::load_config(&gait_dir);
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

    let agent_path: Option<PathBuf> = if let Some(ref name) = config.agent_name {
        Some(crate::agent_frontmatter::resolve_markdown_context_path(
            &gait_dir,
            crate::agent_frontmatter::MarkdownKind::Agent,
            name,
        ))
    } else {
        let default = crate::agent_frontmatter::resolve_markdown_context_path(
            &gait_dir,
            crate::agent_frontmatter::MarkdownKind::Agent,
            "default",
        );
        if default.exists() {
            Some(default)
        } else {
            None
        }
    };
    let profile = crate::profile::Profile::from_config_and_agent(
        &cfg,
        &gait_dir,
        Some(codebase_path.as_path()),
        agent_path.as_deref(),
        config.model_override.clone(),
    );

    let repos = crate::git::Repos::production();

    let source = crate::port::input::ArgSource::new(None);

    let session = std::sync::Arc::new(
        crate::session::Session::load(
            gait_dir.clone(),
            session_id.clone(),
            codebase_path.clone(),
            repos.clone(),
        )
        .unwrap_or_else(|e| {
            eprintln!("[brain] error: failed to load session.json: {e}");
            std::process::exit(1);
        }),
    );

    let mut driver = BrainDriver {
        profile: Some(profile),
        codebase: codebase_path,
        session_id: session_id.clone(),
        gait_dir: gait_dir.clone(),
        repos,
        api_key,
        auto: config.auto,
        max_iter: cfg.max_iter,
        preview_policy: cfg.preview,
        sandbox_on: cfg.sandbox.on,
        nerve_writer: nerve_writer.clone(),
        event_tx,
        stream_fwd_tx,
        spawn_tx,
        workflow_runner,
    };

    let mut sink = BrainSink {
        inner: CliSink::new(std::io::stderr(), std::io::stderr(), false),
        result_json: None,
    };

    let loop_result = crate::embed::run_loop(
        source,
        &mut sink,
        session,
        &gait_dir,
        &session_id,
        &mut driver,
    )
    .await;

    let (run_result_json, failed) = match loop_result {
        Err(e) => {
            eprintln!("[brain] error: {}", e);
            ("{}".to_string(), true)
        }
        Ok(()) => {
            let failed = sink.inner.failed;
            let json = sink.result_json.unwrap_or_else(|| "{}".to_string());
            (json, failed)
        }
    };

    let terminated = Signal {
        corr_id: 0,
        body: Some(signal::Body::Terminated(Terminated {
            run_result_json,
            failed,
        })),
    };
    nerve_writer.send_one_way(&terminated).await.ok();
}

// ── BrainSink ─────────────────────────────────────────────────────────────────

/// A TurnSink that routes output to stderr (brain's stdout is the Nerve pipe)
/// and captures the run_result JSON so `run_brain` can forward it in Terminated.
struct BrainSink {
    inner: CliSink<std::io::Stderr, std::io::Stderr>,
    result_json: Option<String>,
}

impl TurnSink for BrainSink {
    fn item(&mut self, _item: &StreamItem) {
        // No-op: brain routes StreamItems via StreamForward Signals to the spine.
    }
    fn result(&mut self, r: &crate::turn::TurnResult) {
        if let Ok(j) = serde_json::to_string(&crate::port::output::run_result_proto(r)) {
            self.result_json = Some(j);
        }
    }
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
    api_key: Option<String>,
    auto: bool,
    max_iter: Option<usize>,
    preview_policy: crate::config::PreviewPolicy,
    sandbox_on: bool,
    nerve_writer: NerveWriter,
    event_tx: mpsc::UnboundedSender<Event>,
    stream_fwd_tx: mpsc::UnboundedSender<StreamItem>,
    spawn_tx: mpsc::UnboundedSender<Vec<String>>,
    workflow_runner: Option<Arc<dyn WorkflowRunner>>,
}

#[async_trait::async_trait(?Send)]
impl TurnDriver for BrainDriver {
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
                self.api_key.clone(),
                self.workflow_runner.take(),
            )
            .await
    }

    fn turn_config(&mut self) -> LoopConfig {
        LoopConfig {
            auto: self.auto,
            max_iter: self.max_iter,
            preview_policy: self.preview_policy,
            cancel: None,
            sandbox_on: self.sandbox_on,
            relay: Some(self.nerve_writer.clone()),
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
    }

    async fn after_turn(
        &mut self,
        sink: &mut dyn crate::port::output::TurnSink,
        result: &crate::turn::TurnResult,
    ) {
        use crate::{HarnessBackend, Scheduler};
        let sj = crate::session::session_json::read_session_json(&self.gait_dir, &self.session_id)
            .unwrap_or_else(|e| {
                eprintln!("[brain] warning: could not read session.json in after_turn: {e}");
                crate::session::SessionJson {
                    id: self.session_id.clone(),
                    assigned_by: None,
                    created_at: 0,
                    codebase_path: None,
                    agent_name: None,
                    system_prompt: None,
                }
            });
        let scheduler = crate::NativeScheduler;
        let harness = crate::NativeHarnessBackend::new_with_spawn(
            self.gait_dir.clone(),
            self.spawn_tx.clone(),
        );
        let ended = crate::TurnEnded {
            session_id: &self.session_id,
            assigned_by: sj.assigned_by.as_deref(),
            result,
        };
        for action in scheduler.next(&ended) {
            if let Err(e) = harness.execute(&action).await {
                sink.diagnostic(
                    crate::port::output::Diag::Info,
                    &format!("warning: harness execute failed: {}", e),
                );
            }
        }
    }

    fn on_turn_error(
        &mut self,
        sink: &mut dyn crate::port::output::TurnSink,
        e: anyhow::Error,
    ) -> std::ops::ControlFlow<()> {
        sink.diagnostic(crate::port::output::Diag::Error, &format!("error: {}", e));
        std::ops::ControlFlow::Break(())
    }
}

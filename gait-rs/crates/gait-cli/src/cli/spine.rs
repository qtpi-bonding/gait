//! Phase 3 Task 4a: spine process (`gait run`) and brain subprocess (`gait __brain`).
//!
//! Spine: resolves API key, pre-writes input events, spawns brain, relay loop.
//! Brain: LLM inference + RelaySynapse tool calls + Append signals for events.

use std::path::PathBuf;
use std::sync::Arc;

use prost::Message;
use tokio::process::Command;
use tokio::sync::{mpsc, Mutex};

use gait_core::embed::{LoopConfig, TurnDriver};
use gait_core::port::output::{CliSink, Diag, TurnSink};
use gait_core::proto::gait::log::v1::{Event, StreamItem};
use gait_core::proto::Recorder;
use gait_core::turn::nerve::{self, NerveWriter};
use gait_proto::gait::sandbox::v1::{
    network_scope::Mode, signal, Append, NetworkScope, Scope, Signal, StreamForward, Terminated,
};

use crate::cli::run::RunArgs;

// ── Spine ─────────────────────────────────────────────────────────────────────

pub async fn cmd_spine(argv: &[String], gait_dir: PathBuf, codebase_path: PathBuf) {
    use clap::Parser;

    let args = RunArgs::parse_from(argv);
    let mut sink = gait_core::port::output::CliSink::stdio(false);

    let session_id = args.session_id.clone().unwrap_or_else(|| {
        let id = format!(
            "s{}",
            &uuid::Uuid::new_v4().to_string().replace('-', "")[..8]
        );
        sink.session_id(&id);
        id
    });

    let mut config = gait_core::config::load_config(&gait_dir);
    for p in &args.allow {
        if !config.allow.contains(p) {
            config.allow.push(p.clone());
        }
    }
    for p in &args.deny {
        if !config.deny.contains(p) {
            config.deny.push(p.clone());
        }
    }
    if let Some(n) = args.max_iter {
        config.max_iter = Some(n);
    }
    if args.sandbox_locked {
        config.sandbox.on = true;
    } else if args.no_sandbox {
        config.sandbox.on = false;
    } else if args.sandbox {
        config.sandbox.on = true;
    }

    // Build profile to extract provider name for API key resolution.
    let agent_path: Option<PathBuf> = if let Some(ref name) = args.agent_name {
        Some(gait_core::agent_frontmatter::resolve_markdown_context_path(
            &gait_dir,
            gait_core::agent_frontmatter::MarkdownKind::Agent,
            name,
        ))
    } else {
        let default = gait_core::agent_frontmatter::resolve_markdown_context_path(
            &gait_dir,
            gait_core::agent_frontmatter::MarkdownKind::Agent,
            "default",
        );
        if default.exists() {
            Some(default)
        } else {
            None
        }
    };
    let profile = gait_core::profile::Profile::from_config_and_agent(
        &config,
        &gait_dir,
        Some(codebase_path.as_path()),
        agent_path.as_deref(),
        args.model_override.clone(),
    );
    let model = profile.model.clone();
    let provider_name = match model.split_once('/') {
        Some((p, _)) => p.to_string(),
        None => {
            sink.diagnostic(
                Diag::Error,
                &format!("error: model must be 'provider/model', got: '{}'", model),
            );
            std::process::exit(1);
        }
    };
    let api_key = gait_core::auth::resolve_api_key(&provider_name).unwrap_or_default();

    let repos = gait_core::git::Repos::production();

    // Ensure session worktree exists — brain runs Resume mode (no worktree creation).
    repos
        .traces
        .create_session_worktree(&gait_dir, &session_id)
        .unwrap_or_else(|e| {
            sink.diagnostic(
                Diag::Error,
                &format!("error creating session worktree: {e}"),
            );
            std::process::exit(1);
        });

    let log_path = gait_dir
        .join("sessions")
        .join(&session_id)
        .join("log.jsonl");

    // Expand and pre-write input events (brain runs Resume, sees them on first fold).
    if let Some(task_text) = args.task.clone() {
        let hist = gait_dir
            .join("tmp")
            .join(format!("shell-{}.jsonl", &session_id));
        let events = gait_core::session::ingestor::Ingestor::from_session(&gait_dir, &session_id)
            .events()
            .unwrap_or_default();
        let last_blocks = gait_core::session::retrace::last_turn_blocks(&events);
        let expanded = gait_core::expand::expand_input(
            &task_text,
            &gait_core::expand::ExpandCtx {
                codebase: &codebase_path,
                history: &hist,
                last_blocks: &last_blocks,
            },
        );
        let bodies = gait_core::embed::user_turn_events(&repos, &codebase_path, expanded);
        for body in bodies {
            if let Err(e) = gait_core::proto::wire::append(&log_path, body) {
                sink.diagnostic(Diag::Error, &format!("error appending input event: {e}"));
                std::process::exit(1);
            }
        }
    }

    // Build brain argv: same as spine argv but with --session injected and --task stripped.
    let mut brain_args: Vec<String> = argv.to_vec();
    strip_task_from_args(&mut brain_args);
    if !brain_args.iter().any(|a| a == "--session") {
        brain_args.push("--session".into());
        brain_args.push(session_id.clone());
    }

    // Spawn brain: stdin=pipe (spine→brain Feedback), stdout=pipe (brain→spine Signals),
    // stderr=inherit (brain UI output goes to terminal).
    let exe = std::env::current_exe().unwrap_or_else(|e| {
        sink.diagnostic(Diag::Error, &format!("error: current_exe: {e}"));
        std::process::exit(1);
    });
    let mut child = Command::new(&exe)
        .arg("__brain")
        .args(&brain_args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .env("GAIT_BRAIN_API_KEY", &api_key)
        .spawn()
        .unwrap_or_else(|e| {
            sink.diagnostic(Diag::Error, &format!("error spawning __brain: {e}"));
            std::process::exit(1);
        });

    let child_stdin = Arc::new(Mutex::new(
        child.stdin.take().expect("brain stdin should be piped"),
    ));
    let mut child_stdout = child.stdout.take().expect("brain stdout should be piped");

    // Relay loop: read Signal frames from brain stdout.
    let mut run_result_json = String::new();
    let mut brain_failed = false;
    let sandbox_cfg = config.sandbox.clone();
    let sandbox_on = config.sandbox.on;

    loop {
        let signal = match nerve::read_signal(&mut child_stdout).await {
            Ok(s) => s,
            Err(_) => break, // brain exited / pipe closed
        };
        let corr_id = signal.corr_id;
        match signal.body {
            Some(signal::Body::Impulse(impulse)) => {
                // Spawn a task to run the tool and send Feedback back.
                let stdin_writer = Arc::clone(&child_stdin);
                let cfg_clone = sandbox_cfg.clone();
                let gait_dir_c = gait_dir.clone();
                let codebase_c = codebase_path.clone();
                let session_c = session_id.clone();
                let exe_c = exe.clone();
                tokio::spawn(async move {
                    let feedback = run_tool_for_spine(
                        impulse,
                        &cfg_clone,
                        &gait_dir_c,
                        &codebase_c,
                        &session_c,
                        sandbox_on,
                        &exe_c,
                    )
                    .await;
                    let resp = Signal {
                        corr_id,
                        body: Some(signal::Body::Feedback(feedback)),
                    };
                    let mut w = stdin_writer.lock().await;
                    nerve::write_signal(&mut *w, &resp).await.ok();
                });
            }
            Some(signal::Body::Append(append)) => {
                if let Some(evt) = append.event {
                    gait_proto::wire::append_with_id(&log_path, &evt).ok();
                }
            }
            Some(signal::Body::StreamForward(fwd)) => {
                // Brain sends all streaming items via StreamForward so that any
                // spine (CLI or REPL) can render them. CLI spine forwards to its
                // CliSink; REPL spine (relay_brain_turn) forwards to ReplSink.
                if let Some(item) = fwd.item {
                    sink.item(&item);
                }
            }
            Some(signal::Body::Terminated(t)) => {
                run_result_json = t.run_result_json;
                brain_failed = t.failed;
                break;
            }
            Some(signal::Body::Spawn(sp)) => {
                // Brain delegates process-spawn to spine (brain may be sealed).
                // Append --sandbox-locked so the child cannot disable sandboxing.
                let mut argv = sp.argv;
                if sandbox_on {
                    argv.push("--sandbox-locked".to_string());
                }
                let _ = Command::new(&exe)
                    .args(&argv)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn();
            }
            _ => {}
        }
    }

    let _ = child.wait().await;

    if !run_result_json.is_empty() {
        println!("{}", run_result_json);
    }
    if brain_failed {
        std::process::exit(1);
    }
}

/// Remove `--task <value>` from a brain argv slice (brain runs in Resume mode).
fn strip_task_from_args(args: &mut Vec<String>) {
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--task" {
            args.remove(i);
            if i < args.len() {
                args.remove(i); // remove the value
            }
        } else {
            i += 1;
        }
    }
}

/// Spine-side tool execution: compute scope ceiling, spawn `__run-tool`, return Feedback.
/// Mirrors `SubprocessSynapse::fire` but uses `bound_request` for scope and takes
/// the impulse directly from the Nerve rather than a `Tool` ref.
async fn run_tool_for_spine(
    impulse: gait_proto::gait::sandbox::v1::Impulse,
    sandbox_cfg: &gait_core::config::SandboxConfig,
    gait_dir: &PathBuf,
    codebase: &PathBuf,
    session_id: &str,
    sandbox_on: bool,
    exe: &std::path::Path,
) -> gait_proto::gait::sandbox::v1::Feedback {
    use tokio::io::AsyncWriteExt;

    // Create a per-call temp jail (same pattern as SubprocessSynapse).
    let jail_raw = std::env::temp_dir().join(format!(
        "gait-hand-{}",
        uuid::Uuid::new_v4().to_string().replace('-', "")
    ));
    if std::fs::create_dir_all(&jail_raw).is_err() {
        return error_feedback("failed to create temp jail");
    }
    let jail = jail_raw.canonicalize().unwrap_or_else(|_| jail_raw.clone());

    // Spine computes authoritative scope ceiling — ignores brain's claimed scope.
    let scope =
        gait_core::sandbox::ceiling::bound_request(&impulse.tool, sandbox_cfg, codebase, &jail);
    let requested = scope_to_proto(&scope);

    let impulse_with_scope = gait_proto::gait::sandbox::v1::Impulse {
        tool: impulse.tool.clone(),
        args: impulse.args,
        cwd: impulse.cwd,
        requested: Some(requested),
    };
    let bytes = impulse_with_scope.encode_to_vec();

    let mut child = match Command::new(exe)
        .arg("__run-tool")
        .arg("--gait-dir")
        .arg(gait_dir)
        .arg("--codebase")
        .arg(codebase)
        .arg("--session")
        .arg(session_id)
        .arg("--tool")
        .arg(&impulse.tool)
        .arg("--seal")
        .arg(if sandbox_on { "true" } else { "false" })
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .env_clear()
        .envs(std::env::vars().filter(|(k, _)| {
            matches!(
                k.as_str(),
                "PATH" | "HOME" | "USER" | "SHELL" | "TERM" | "LANG" | "LC_ALL" | "TMPDIR"
            )
        }))
        .env("TMPDIR", &jail)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&jail);
            return error_feedback(&format!("failed to spawn __run-tool: {e}"));
        }
    };

    {
        let mut stdin = match child.stdin.take() {
            Some(s) => s,
            None => {
                let _ = std::fs::remove_dir_all(&jail);
                return error_feedback("no stdin on __run-tool");
            }
        };
        if let Err(e) = stdin.write_all(&bytes).await {
            let _ = std::fs::remove_dir_all(&jail);
            return error_feedback(&format!("write impulse failed: {e}"));
        }
    }

    let out = match child.wait_with_output().await {
        Ok(o) => o,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&jail);
            return error_feedback(&format!("wait_with_output failed: {e}"));
        }
    };
    let _ = std::fs::remove_dir_all(&jail);

    if out.status.success() {
        gait_proto::gait::sandbox::v1::Feedback::decode(out.stdout.as_slice())
            .unwrap_or_else(|e| error_feedback(&format!("bad Feedback: {e}")))
    } else {
        error_feedback(&format!(
            "hand exited {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        ))
    }
}

/// Build a Feedback carrying a string error result.
fn error_feedback(msg: &str) -> gait_proto::gait::sandbox::v1::Feedback {
    gait_proto::gait::sandbox::v1::Feedback {
        result: Some(pbjson_types::Value {
            kind: Some(pbjson_types::value::Kind::StringValue(msg.to_string())),
        }),
        events: vec![],
        denied: None,
    }
}

/// Convert SandboxScope to proto Scope. Duplicated from synapse.rs (private there).
fn scope_to_proto(s: &gait_core::sandbox::SandboxScope) -> Scope {
    use gait_core::sandbox::NetworkPolicy;
    Scope {
        read_all: s.read_all,
        readable: s
            .readable
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect(),
        writable: s
            .writable
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect(),
        network: Some(match &s.network {
            NetworkPolicy::Deny => NetworkScope {
                mode: Mode::Deny as i32,
                hosts: vec![],
            },
            NetworkPolicy::All => NetworkScope {
                mode: Mode::All as i32,
                hosts: vec![],
            },
            NetworkPolicy::Hosts(h) => NetworkScope {
                mode: Mode::Hosts as i32,
                hosts: h.clone(),
            },
        }),
    }
}

// ── Brain ─────────────────────────────────────────────────────────────────────

/// Build the brain's self-seal scope: read-all + zero writes + full outbound network.
///
/// Validated spike (macOS 15 / Darwin 24.5.0):
///   `(deny default)(allow file-read*)(allow process*)(allow network-outbound)
///    (allow system-socket)(allow mach-lookup …)`
/// DNS + TCP to api.anthropic.com:443 succeed under this policy with
/// reqwest+rustls+bundled webpki-roots (no cert-store reads needed).
fn brain_scope() -> gait_core::sandbox::SandboxScope {
    gait_core::sandbox::SandboxScope {
        read_all: true,
        readable: vec![],
        writable: vec![],
        network: gait_core::sandbox::NetworkPolicy::All,
    }
}

pub async fn cmd_brain(argv: &[String], gait_dir: PathBuf, codebase_path: PathBuf) {
    use clap::Parser;

    let args = RunArgs::parse_from(argv);

    // API key passed from spine via env.
    let api_key = std::env::var("GAIT_BRAIN_API_KEY").ok();

    // ── Apply brain seal ──────────────────────────────────────────────────────
    // Self-seal before any I/O: read-all + zero writes + outbound network.
    // After this point the brain cannot write the codebase or gait-dir
    // (all log events go via Append signals to the spine which owns the log).
    let seal_status = gait_core::sandbox::platform_sandbox()
        .apply(&brain_scope())
        .unwrap_or(gait_core::sandbox::SandboxStatus::Unavailable {
            reason: "apply() returned Err".to_string(),
        });
    let _seal_enforcement = match &seal_status {
        gait_core::sandbox::SandboxStatus::Active => "active",
        gait_core::sandbox::SandboxStatus::Noop => "noop",
        gait_core::sandbox::SandboxStatus::Unavailable { .. } => "unavailable",
    };
    // The SandboxApplied event is emitted via the event_tx after the Nerve is
    // wired — we capture it here and emit it once the recorder is live.
    // ──────────────────────────────────────────────────────────────────────────

    // Brain reads Feedback from stdin (spine→brain), writes Signals to stdout (brain→spine).
    let (nerve_writer, _nerve_handle) = nerve::start_nerve(tokio::io::stdin(), tokio::io::stdout());

    // Channel for AppendRelay: Recorder sends Events here; relay task forwards as Append signals.
    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<Event>();

    // Emit the brain-seal SandboxApplied event as the first Append signal.
    {
        let reason = match &seal_status {
            gait_core::sandbox::SandboxStatus::Unavailable { reason } => Some(reason.clone()),
            _ => None,
        };
        let evt_body = gait_core::proto::events::sandbox_applied(_seal_enforcement, vec![], reason);
        // Mint with prev=None (brain has no log); spine's append_with_id updates seq/parents.
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

    // Channel for StreamForward: Recorder's stream_tx feeds this; relay task
    // sends StreamForward Signals to the spine so it can call sink.item().
    // BrainSink::item() is a no-op — the spine owns all streaming rendering.
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

    // Channel for Spawn: tools (spawn_agent) and harness send argv here; relay
    // task forwards as Spawn Signals so the spine (not the sealed brain) executes
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

    let session_id = match args.session_id.clone() {
        Some(id) => id,
        None => {
            eprintln!("[brain] error: --session is required for __brain");
            std::process::exit(1);
        }
    };

    let mut config = gait_core::config::load_config(&gait_dir);
    for p in &args.allow {
        if !config.allow.contains(p) {
            config.allow.push(p.clone());
        }
    }
    for p in &args.deny {
        if !config.deny.contains(p) {
            config.deny.push(p.clone());
        }
    }
    if let Some(n) = args.max_iter {
        config.max_iter = Some(n);
    }
    if args.sandbox_locked {
        config.sandbox.on = true;
    } else if args.no_sandbox {
        config.sandbox.on = false;
    } else if args.sandbox {
        config.sandbox.on = true;
    }

    let agent_path: Option<PathBuf> = if let Some(ref name) = args.agent_name {
        Some(gait_core::agent_frontmatter::resolve_markdown_context_path(
            &gait_dir,
            gait_core::agent_frontmatter::MarkdownKind::Agent,
            name,
        ))
    } else {
        let default = gait_core::agent_frontmatter::resolve_markdown_context_path(
            &gait_dir,
            gait_core::agent_frontmatter::MarkdownKind::Agent,
            "default",
        );
        if default.exists() {
            Some(default)
        } else {
            None
        }
    };
    let profile = gait_core::profile::Profile::from_config_and_agent(
        &config,
        &gait_dir,
        Some(codebase_path.as_path()),
        agent_path.as_deref(),
        args.model_override.clone(),
    );

    let repos = gait_core::git::Repos::production();

    // Brain runs in Resume mode — spine pre-wrote input events to the log.
    let source = gait_core::port::input::ArgSource::new(None);

    let session = std::sync::Arc::new(
        gait_core::session::Session::load(
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
        auto: args.auto,
        max_iter: config.max_iter,
        preview_policy: config.preview,
        sandbox_on: config.sandbox.on,
        nerve_writer: nerve_writer.clone(),
        event_tx,
        stream_fwd_tx,
        spawn_tx,
    };

    // Brain's UI/trace output goes to stderr (stdout is the Nerve pipe).
    let mut sink = BrainSink {
        inner: CliSink::new(std::io::stderr(), std::io::stderr(), false),
        result_json: None,
    };

    let loop_result = gait_core::embed::run_loop(
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

    // Signal completion to spine.
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
/// and captures the run_result JSON so cmd_brain can forward it in Terminated.
struct BrainSink {
    inner: CliSink<std::io::Stderr, std::io::Stderr>,
    result_json: Option<String>,
}

impl TurnSink for BrainSink {
    fn item(&mut self, _item: &StreamItem) {
        // No-op: brain routes StreamItems via StreamForward Signals to the spine.
        // The spine's sink (CliSink or ReplSink) owns all streaming rendering.
    }
    fn result(&mut self, r: &gait_core::turn::TurnResult) {
        if let Ok(j) = serde_json::to_string(&gait_core::port::output::run_result_proto(r)) {
            self.result_json = Some(j);
        }
        // Do NOT write to stdout — that's the Nerve pipe.
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
    profile: Option<gait_core::profile::Profile>,
    codebase: PathBuf,
    session_id: String,
    gait_dir: PathBuf,
    repos: gait_core::git::Repos,
    api_key: Option<String>,
    auto: bool,
    max_iter: Option<usize>,
    preview_policy: gait_core::config::PreviewPolicy,
    sandbox_on: bool,
    nerve_writer: NerveWriter,
    event_tx: mpsc::UnboundedSender<Event>,
    /// StreamForward relay: brain sends stream items here; relay task forwards
    /// them as StreamForward Signals so the spine's sink renders them.
    stream_fwd_tx: mpsc::UnboundedSender<StreamItem>,
    /// Spawn relay: tools and harness send argv here; relay task forwards as
    /// Spawn Signals so the sealed brain never calls Command::spawn directly.
    spawn_tx: mpsc::UnboundedSender<Vec<String>>,
}

#[async_trait::async_trait(?Send)]
impl TurnDriver for BrainDriver {
    async fn resolve_profile(&mut self) -> anyhow::Result<gait_core::profile::ResolvedProfile> {
        let profile = self.profile.take().ok_or_else(|| {
            anyhow::anyhow!("BrainDriver::resolve_profile called more than once (single-shot)")
        })?;
        let runner = Arc::new(gait_workflow::GaitWorkflowRunner::new(
            self.gait_dir.clone(),
            self.codebase.clone(),
        ));
        profile
            .resolve(
                self.codebase.clone(),
                self.session_id.clone(),
                self.gait_dir.clone(),
                self.repos.clone(),
                self.api_key.clone(),
                Some(runner),
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
        // Use stream_fwd_tx instead of the run_loop's internal stream_tx.
        // This routes StreamItems → StreamForward Signals → spine's sink.item().
        // The run_loop's drive(rx, BrainSink) will drain an empty channel and
        // return immediately; BrainSink::item() is intentionally a no-op.
        // Attach spawn_tx so tools like spawn_agent delegate process-launch to spine.
        Recorder::remote_sink(self.stream_fwd_tx.clone(), self.event_tx.clone())
            .with_spawn_tx(self.spawn_tx.clone())
    }

    async fn after_turn(
        &mut self,
        sink: &mut dyn gait_core::port::output::TurnSink,
        result: &gait_core::turn::TurnResult,
    ) {
        use gait_core::{HarnessBackend, Scheduler};
        let sj =
            gait_core::session::session_json::read_session_json(&self.gait_dir, &self.session_id)
                .unwrap_or_else(|e| {
                    eprintln!("[brain] warning: could not read session.json in after_turn: {e}");
                    gait_core::session::SessionJson {
                        id: self.session_id.clone(),
                        assigned_by: None,
                        created_at: 0,
                        codebase_path: None,
                        agent_name: None,
                        system_prompt: None,
                    }
                });
        let scheduler = gait_core::NativeScheduler;
        let harness = gait_core::NativeHarnessBackend::new_with_spawn(
            self.gait_dir.clone(),
            self.spawn_tx.clone(),
        );
        let ended = gait_core::TurnEnded {
            session_id: &self.session_id,
            assigned_by: sj.assigned_by.as_deref(),
            result,
        };
        for action in scheduler.next(&ended) {
            if let Err(e) = harness.execute(&action).await {
                sink.diagnostic(
                    gait_core::port::output::Diag::Info,
                    &format!("warning: harness execute failed: {}", e),
                );
            }
        }
    }

    fn on_turn_error(
        &mut self,
        sink: &mut dyn gait_core::port::output::TurnSink,
        e: anyhow::Error,
    ) -> std::ops::ControlFlow<()> {
        sink.diagnostic(
            gait_core::port::output::Diag::Error,
            &format!("error: {}", e),
        );
        std::ops::ControlFlow::Break(())
    }
}

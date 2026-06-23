//! Spine process (`gait run`).
//!
//! `cmd_spine` uses `SpineDriver` + `run_loop` (the sealed loop). One turn per
//! `TurnStart` from `SpineSource`. Brain launch is handled by `gait_core::__dispatch_role`.

use std::path::PathBuf;
use std::sync::Arc;

use gait_core::embed::{
    BrainLaunch, CliSink, Diag, LoopConfig, TurnDriver, TurnSink, TurnSource, TurnStart,
};

use crate::cli::run::RunArgs;

// ── SpineDriver ───────────────────────────────────────────────────────────────

/// `TurnDriver` impl for `gait run` (the spine process). Supplies `brain_launch()`
/// with all data `relay_brain_turn` needs; `turn_config` is informational only (the
/// actual config lands in `BrainInit` via `ProfileInputs`).
struct SpineDriver {
    /// Argv forwarded to `gait __brain` (--session injected by cmd_spine).
    brain_args: Vec<String>,
    codebase: PathBuf,
    api_key: String,
    gait_bin: PathBuf,
    sandbox_cfg: gait_core::config::SandboxConfig,
    sandbox_on: bool,
    // Profile-level inputs stored as individual fields to avoid naming the sealed type.
    agent_name: Option<String>,
    model_override: Option<String>,
    allow: Vec<String>,
    deny: Vec<String>,
    result_schema: Option<serde_json::Value>,
    cascade_group_arg: Option<String>,
    /// For after_turn: scheduler/harness policy on success.
    gait_dir: PathBuf,
    session_id: String,
    auto: bool,
    max_iter: Option<usize>,
}

#[async_trait::async_trait(?Send)]
impl TurnDriver for SpineDriver {
    fn brain_launch(&self, _session_id: &str) -> BrainLaunch {
        BrainLaunch::new(
            self.brain_args.clone(),
            self.codebase.clone(),
            self.api_key.clone(),
            self.gait_bin.clone(),
            self.sandbox_cfg.clone(),
            self.sandbox_on,
            self.cascade_group_arg.clone(),
            self.agent_name.clone(),
            self.model_override.clone(),
            self.allow.clone(),
            self.deny.clone(),
            self.result_schema.clone(),
        )
    }

    fn turn_config(&mut self) -> LoopConfig {
        LoopConfig {
            auto: self.auto,
            max_iter: self.max_iter,
            preview_policy: gait_core::config::PreviewPolicy::default(),
            cancel: None,
            sandbox_on: self.sandbox_on,
            sandbox_cfg: self.sandbox_cfg.clone(),
            relay: None,
            initial_git: None,
            daemons: vec![],
        }
    }

    async fn after_turn(&mut self, sink: &mut dyn TurnSink, _result: &gait_core::turn::TurnResult) {
        // The spine's after_turn reads the session meta and runs the Scheduler/Harness.
        // Note: relay_brain_turn already called sink.result() with the real TurnResult;
        // run_loop passes a synthetic dummy here — we skip harness for now (Task 3 wires this).
        let _ = (sink, &self.gait_dir, &self.session_id);
    }

    fn on_turn_error(
        &mut self,
        sink: &mut dyn TurnSink,
        e: anyhow::Error,
    ) -> std::ops::ControlFlow<()> {
        sink.diagnostic(Diag::Error, &format!("spine relay error: {e}"));
        std::ops::ControlFlow::Break(())
    }
}

// ── SpineSource ───────────────────────────────────────────────────────────────

/// Single-turn source: yields exactly one `Input([user_bodies])` then `None`.
/// For `--task`-less runs (resume), yields one `Resume`.
struct SpineSource {
    bodies: Option<Vec<gait_core::proto::gait::log::v1::event::Body>>,
    done: bool,
}

#[async_trait::async_trait(?Send)]
impl TurnSource for SpineSource {
    async fn next(&mut self) -> Option<TurnStart> {
        if self.done {
            return None;
        }
        self.done = true;
        match self.bodies.take() {
            Some(b) if !b.is_empty() => Some(TurnStart::Input(b)),
            _ => Some(TurnStart::Resume),
        }
    }
}

// ── Spine ─────────────────────────────────────────────────────────────────────

pub async fn cmd_spine(argv: &[String], gait_dir: PathBuf, codebase_path: PathBuf) {
    use clap::Parser;

    // ── Linux: self-assign to parent's cascade group (race-free enrollment) ──
    // Must happen before any fork/spawn so all descendants land in the group.
    #[cfg(target_os = "linux")]
    {
        // Peek at --cascade-group without full parse (full parse happens below).
        // We do a lightweight scan: find "--cascade-group" followed by its value.
        let mut cg_val: Option<&str> = None;
        let mut iter = argv.iter();
        while let Some(arg) = iter.next() {
            if arg == "--cascade-group" {
                cg_val = iter.next().map(|s| s.as_str());
                break;
            } else if let Some(val) = arg.strip_prefix("--cascade-group=") {
                cg_val = Some(val);
                break;
            }
        }
        if let Some(cg) = cg_val {
            if cg.starts_with("/sys/fs/cgroup") {
                // cgroup v2 path: write our pid into cgroup.procs
                let pid = std::process::id().to_string();
                let procs = format!("{cg}/cgroup.procs");
                let _ = std::fs::write(&procs, &pid);
            } else if let Ok(pgid) = cg.parse::<i32>() {
                // pgid degrade path
                unsafe {
                    libc::setpgid(0, pgid);
                }
            }
        }
    }

    let args = RunArgs::parse_from(argv);
    let mut sink = CliSink::stdio(false);

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

    // Expand and pre-write input events (brain runs Resume, sees them on first fold).
    let user_bodies: Vec<gait_core::proto::gait::log::v1::event::Body> = if let Some(task_text) =
        args.task.clone()
    {
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
        gait_core::embed::user_turn_events(&repos, &codebase_path, expanded)
    } else {
        vec![]
    };

    // Build brain argv: same as spine argv but with --session injected and --task stripped.
    let mut brain_args: Vec<String> = argv.to_vec();
    gait_core::embed::strip_task_from_args(&mut brain_args);
    if !brain_args.iter().any(|a| a == "--session") {
        brain_args.push("--session".into());
        brain_args.push(session_id.clone());
    }

    let exe = std::env::current_exe().unwrap_or_else(|e| {
        sink.diagnostic(Diag::Error, &format!("error: current_exe: {e}"));
        std::process::exit(1);
    });

    let sandbox_cfg = config.sandbox.clone();
    let sandbox_on = config.sandbox.on;

    // Read per-spawn result schema if the child session has one written by the parent spine.
    let result_schema: Option<serde_json::Value> = {
        let schema_path = gait_dir
            .join("sessions")
            .join(&session_id)
            .join("result_schema.json");
        std::fs::read(&schema_path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
    };

    // Process-lifetime cascade kill handle — held by the SIGINT/SIGTERM handler
    // and threaded through run_loop so relay_brain_turn can register the active group.
    let kill_handle = gait_core::embed::CascadeKillHandle::new();

    // Spawn SIGINT/SIGTERM handler: kill current group (if any) then exit.
    {
        use tokio::signal::unix::{signal, SignalKind};
        let handle = kill_handle.clone();
        tokio::spawn(async move {
            let mut sigint = signal(SignalKind::interrupt()).expect("SIGINT handler");
            let mut sigterm = signal(SignalKind::terminate()).expect("SIGTERM handler");
            tokio::select! {
                _ = sigint.recv() => {}
                _ = sigterm.recv() => {}
            }
            handle.kill();
            std::process::exit(0);
        });
    }

    // Build the Session for run_loop (spine is the trusted parent process).
    let session = Arc::new(gait_core::session::Session {
        id: session_id.clone(),
        gait_dir: gait_dir.clone(),
        codebase_path: codebase_path.clone(),
        assigned_by: None,
        repos: repos.clone(),
    });

    let source = SpineSource {
        bodies: if user_bodies.is_empty() {
            None
        } else {
            Some(user_bodies)
        },
        done: false,
    };

    let mut driver = SpineDriver {
        brain_args,
        codebase: codebase_path,
        api_key,
        gait_bin: exe,
        sandbox_cfg,
        sandbox_on,
        agent_name: args.agent_name.clone(),
        model_override: args.model_override.clone(),
        allow: args.allow.clone(),
        deny: args.deny.clone(),
        result_schema,
        cascade_group_arg: args.cascade_group.clone(),
        gait_dir: gait_dir.clone(),
        session_id: session_id.clone(),
        auto: args.auto,
        max_iter: config.max_iter,
    };

    let run_result = gait_core::embed::run_loop(
        source,
        &mut sink,
        session,
        &gait_dir,
        &session_id,
        &mut driver,
        Some(&kill_handle),
    )
    .await;

    if let Err(e) = run_result {
        sink.diagnostic(Diag::Error, &format!("spine run_loop error: {e}"));
        std::process::exit(1);
    }

    if sink.failed {
        std::process::exit(1);
    }
}

// ── Brain ─────────────────────────────────────────────────────────────────────
// The brain runtime lives in gait-core/src/turn/brain.rs.
// Dispatch is via gait_core::__dispatch_role (Task 5).

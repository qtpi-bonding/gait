//! `RunArgs` — shared arg parser for `gait run` / `gait __brain` dispatch.
//!
//! The `gait run` turn consumer.
use clap::Parser;
use std::path::PathBuf;
use std::sync::Arc;

use gait_core::embed::{
    BrainLaunch, CliSink, Diag, LoopConfig, TurnDriver, TurnSink, TurnSource, TurnStart,
};

#[derive(Parser, Debug)]
#[command(no_binary_name = true)]
pub struct RunArgs {
    #[arg(long = "session")]
    pub session_id: Option<String>,
    #[arg(long)]
    pub task: Option<String>,
    #[arg(long)]
    pub auto: bool,
    #[arg(long = "model")]
    pub model_override: Option<String>,
    #[arg(long = "agent")]
    pub agent_name: Option<String>,
    #[arg(long = "result-schema")]
    pub result_schema_path: Option<String>,
    #[arg(long, action = clap::ArgAction::Append)]
    pub allow: Vec<String>,
    #[arg(long, action = clap::ArgAction::Append)]
    pub deny: Vec<String>,
    #[arg(long, action = clap::ArgAction::Append)]
    pub unlock: Vec<String>,
    #[arg(long)]
    pub max_iter: Option<usize>,
    // Task 7: sandbox engagement flags. `--sandbox` forces on, `--no-sandbox` forces off.
    // When neither is passed, the config default (`config.sandbox.on`) applies.
    // `--sandbox-locked` is set by a parent process when spawning a child; it forces
    // sandbox on and makes `--no-sandbox` a no-op so the child cannot escape.
    #[arg(long)]
    pub sandbox: bool,
    #[arg(long = "no-sandbox")]
    pub no_sandbox: bool,
    #[arg(long = "sandbox-locked", hide = true)]
    pub sandbox_locked: bool,
    /// Cascade group handle passed by a parent process to a blocking subagent.
    /// `None` for the top-level turn; a parent's pgid string for subagents (Plan 4).
    #[arg(long = "cascade-group", hide = true)]
    pub cascade_group: Option<String>,
    // Consumed by caller (args.rs); included to prevent "unexpected argument" errors.
    #[arg(long, hide = true)]
    gait_dir: Option<PathBuf>,
    #[arg(long, hide = true)]
    codebase: Option<PathBuf>,
    // Accepted to prevent "unexpected argument" errors. Container spawning is
    // handled externally — gait never self-spawns into a container.
    #[arg(long = "container", hide = true)]
    container: bool,
}

// ── CliDriver ─────────────────────────────────────────────────────────────────

/// `TurnDriver` impl for `gait run`. Supplies `brain_launch()`
/// with all data `relay_brain_turn` needs; `turn_config` is informational only (the
/// actual config lands in `BrainInit` via `ProfileInputs`).
struct CliDriver {
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
impl TurnDriver for CliDriver {
    fn brain_launch(&self, _session_id: &str) -> BrainLaunch {
        BrainLaunch::new(
            self.codebase.clone(),
            self.api_key.clone(),
            self.gait_bin.clone(),
            self.sandbox_cfg.clone(),
            self.sandbox_on,
            self.auto,
            self.max_iter,
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
        // The run consumer's after_turn reads the session meta and runs the Scheduler/Harness.
        // Note: relay_brain_turn already called sink.result() with the real TurnResult;
        // run_loop passes a synthetic dummy here — we skip harness for now (Task 3 wires this).
        let _ = (sink, &self.gait_dir, &self.session_id);
    }

    fn on_turn_error(
        &mut self,
        sink: &mut dyn TurnSink,
        e: anyhow::Error,
    ) -> std::ops::ControlFlow<()> {
        sink.diagnostic(Diag::Error, &format!("run_loop error: {e}"));
        std::ops::ControlFlow::Break(())
    }
}

// ── CliSource ─────────────────────────────────────────────────────────────────

/// Single-turn source: yields exactly one `Input([user_bodies])` then `None`.
/// For `--task`-less runs (resume), yields one `Resume`.
struct CliSource {
    bodies: Option<Vec<gait_core::proto::gait::log::v1::event::Body>>,
    done: bool,
}

#[async_trait::async_trait(?Send)]
impl TurnSource for CliSource {
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

// ── cmd_run ───────────────────────────────────────────────────────────────────

pub async fn cmd_run(argv: &[String], gait_dir: PathBuf, codebase_path: PathBuf) {
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

    let exe = std::env::current_exe().unwrap_or_else(|e| {
        sink.diagnostic(Diag::Error, &format!("error: current_exe: {e}"));
        std::process::exit(1);
    });

    let sandbox_cfg = config.sandbox.clone();
    let sandbox_on = config.sandbox.on;

    // Read per-spawn result schema if the child session has one written by the parent process.
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

    // Build the Session for run_loop (this process is the trusted parent).
    let session = Arc::new(gait_core::session::Session {
        id: session_id.clone(),
        gait_dir: gait_dir.clone(),
        codebase_path: codebase_path.clone(),
        assigned_by: None,
        repos: repos.clone(),
    });

    let source = CliSource {
        bodies: if user_bodies.is_empty() {
            None
        } else {
            Some(user_bodies)
        },
        done: false,
    };

    let mut driver = CliDriver {
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
        sink.diagnostic(Diag::Error, &format!("run_loop error: {e}"));
        std::process::exit(1);
    }

    if sink.failed {
        std::process::exit(1);
    }
}

// ── Brain ─────────────────────────────────────────────────────────────────────
// The brain runtime lives in gait-core/src/turn/brain.rs.
// Dispatch is via gait_core::__dispatch_role (Task 5).

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parse_extracts_task_and_flags() {
        let argv: Vec<String> = [
            "--task",
            "hi",
            "--auto",
            "--model",
            "x/y",
            "--max-iter",
            "7",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let a = RunArgs::parse_from(&argv);
        assert_eq!(a.task.as_deref(), Some("hi"));
        assert!(a.auto);
        assert_eq!(a.model_override.as_deref(), Some("x/y"));
        assert_eq!(a.max_iter, Some(7));
    }
    #[test]
    fn parse_no_task_is_none() {
        assert!(RunArgs::parse_from(&["--auto".to_string()]).task.is_none());
    }
    #[test]
    fn sandbox_locked_parsed() {
        let a = RunArgs::parse_from(&["--sandbox-locked".to_string()]);
        assert!(a.sandbox_locked);
        assert!(!a.no_sandbox);
    }
}

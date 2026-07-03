//! `RunArgs` — shared arg parser for `gait run` / `gait __brain` dispatch.
//!
//! The `gait run` turn consumer.
use clap::Parser;
use std::path::PathBuf;

use gait_core::embed::{CliSink, Diag, TurnSink, TurnSource, TurnStart};

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

    // Entry point: surface gait.jsonc parse errors instead of silently
    // defaulting. CLI args layered on top still take precedence below.
    let mut config = match gait_core::config::load_config_with(
        &gait_dir,
        gait_core::config::ConfigLoadPolicy::Strict,
    ) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[gait] {e}");
            std::process::exit(1);
        }
    };
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
    // Validation only — `PromptNodeDriver::brain_launch` re-derives the
    // provider (and resolves the API key) from `self.model` itself now that
    // `cmd_run` no longer builds its own driver.
    if model.split_once('/').is_none() {
        sink.diagnostic(
            Diag::Error,
            &format!("error: model must be 'provider/model', got: '{}'", model),
        );
        std::process::exit(1);
    }

    // Expand the task text (`single_prompt_run`'s literal branch builds the
    // actual user_turn/context_status events from this, same as the old
    // direct-run_loop path did inline).
    let prompt: Option<String> = args.task.clone().map(|task_text| {
        let hist = gait_dir
            .join("tmp")
            .join(format!("shell-{}.jsonl", &session_id));
        let events = gait_core::session::ingestor::Ingestor::from_session(&gait_dir, &session_id)
            .events()
            .unwrap_or_default();
        let last_blocks = gait_core::session::retrace::last_turn_blocks(&events);
        gait_core::expand::expand_input(
            &task_text,
            &gait_core::expand::ExpandCtx {
                codebase: &codebase_path,
                history: &hist,
                last_blocks: &last_blocks,
            },
        )
    });

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
    // and threaded through run_loop (via SinglePromptOpts.cascade) so
    // relay_brain_turn can register the active group.
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

    // `prompt: Some(text)` -> literal one-shot node; `prompt: None` -> resume,
    // driven by a live CliSource yielding exactly one `Resume` turn (the CLI
    // has no interactive loop of its own — a single turn, then exit).
    let input: Option<Box<dyn TurnSource>> = if prompt.is_none() {
        Some(Box::new(CliSource {
            bodies: None,
            done: false,
        }))
    } else {
        None
    };

    // spec-06 §3 (one orchestrator, one BrainLaunch site): `cmd_run` no longer
    // builds its own driver/Session/run_loop call — it goes through the same
    // `single_prompt_run` -> `run_workflow` -> `PromptNodeDriver` path the REPL
    // uses. The CLI's resolved profile/sandbox/cascade travel as overrides
    // (Task 3.2's C1 fix) so they still reach the spawned brain.
    let profile_override = gait_workflow::ProfileOverride {
        model: args.model_override.clone(),
        allow: args.allow.clone(),
        deny: args.deny.clone(),
        agent: args.agent_name.clone(),
        max_iter: config.max_iter,
        result_schema,
    };
    let outcome = gait_workflow::single_prompt_run(
        gait_workflow::SinglePromptOpts {
            run_id: gait_workflow::convert::mint_run_id("chat"),
            continue_session: session_id.clone(),
            prompt,
            gait_dir: gait_dir.clone(),
            codebase: codebase_path.clone(),
            args: vec![],
            sandbox_on: Some(config.sandbox.on),
            cascade_group_arg: args.cascade_group.clone(),
            cascade: Some(kill_handle.clone()),
            profile: Some(profile_override),
        },
        &mut sink,
        &mut gait_workflow::observer::NullObserver,
        input,
    )
    .await;

    let outcome = match outcome {
        Ok(o) => o,
        Err(e) => {
            sink.diagnostic(Diag::Error, &format!("run error: {e}"));
            std::process::exit(1);
        }
    };

    if sink.failed {
        std::process::exit(1);
    }

    match outcome {
        // GatePaused is unreachable for a degenerate single-prompt run (no gate
        // node in the one-node graph `single_prompt_run` builds); mapped to 0
        // for now. If `cmd_run` ever drives a real multi-node graph, a paused
        // workflow is NOT "done" — revisit this arm then.
        gait_workflow::WorkflowOutcome::Completed
        | gait_workflow::WorkflowOutcome::NodeOpen
        | gait_workflow::WorkflowOutcome::GatePaused => {}
    }
}

// ── Brain ─────────────────────────────────────────────────────────────────────
// The brain runtime lives in gait-core/src/turn/brain.rs.
// Dispatch is via gait_core::__dispatch_role (Task 5).

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_driver_is_gone() {
        // Built via concat! so this assertion's own strings don't self-match
        // include_str!'s scan of this file.
        let src = include_str!("run.rs");
        let struct_needle = concat!("struct ", "CliDriver");
        let impl_needle = concat!("impl TurnDriver for ", "CliDriver");
        assert!(!src.contains(struct_needle), "CliDriver must be deleted");
        assert!(
            !src.contains(impl_needle),
            "CliDriver's TurnDriver impl must be deleted"
        );
    }

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

    // ── RunLaunch round-trip (spec-00-footguns.md item 4) ───────────────────
    //
    // `gait_core::launch::RunLaunch` is gait-core's typed builder for the
    // argv every self-invocation site that targets `RunArgs` builds
    // (gait-bin's bare-positional path, spine's launch_child/spawn_wake, and
    // spawn_agent's detached-agent spawn). The test lives here (not in
    // gait-core's launch.rs) because `RunArgs` is defined in gait-cli, which
    // depends on gait-core — gait-core cannot depend back on gait-cli.
    #[test]
    fn run_launch_round_trips_through_run_args() {
        let launch = gait_core::launch::RunLaunch {
            session: Some("s1".to_string()),
            task: Some("do the thing".to_string()),
            auto: true,
            model_override: Some("anthropic/x".to_string()),
            agent_name: Some("default".to_string()),
            result_schema_path: Some("/schema.json".to_string()),
            allow: vec!["Bash".to_string()],
            deny: vec!["Net".to_string()],
            unlock: vec!["/tmp".to_string()],
            max_iter: Some(9),
            sandbox: false,
            no_sandbox: false,
            sandbox_locked: true,
            cascade_group: Some("pg123".to_string()),
            gait_dir: Some(PathBuf::from("/g")),
            codebase: Some(PathBuf::from("/c")),
        };
        let argv = launch.to_argv();
        // Flags-only: no leading role/subcommand token (`RunArgs` is
        // `no_binary_name = true` and has no positional field).
        assert!(!argv.iter().any(|a| a == "run"));

        let parsed = RunArgs::parse_from(&argv);
        assert_eq!(parsed.session_id, launch.session);
        assert_eq!(parsed.task, launch.task);
        assert_eq!(parsed.auto, launch.auto);
        assert_eq!(parsed.model_override, launch.model_override);
        assert_eq!(parsed.agent_name, launch.agent_name);
        assert_eq!(parsed.result_schema_path, launch.result_schema_path);
        assert_eq!(parsed.allow, launch.allow);
        assert_eq!(parsed.deny, launch.deny);
        assert_eq!(parsed.unlock, launch.unlock);
        assert_eq!(parsed.max_iter, launch.max_iter);
        assert_eq!(parsed.sandbox, launch.sandbox);
        assert_eq!(parsed.no_sandbox, launch.no_sandbox);
        assert_eq!(parsed.sandbox_locked, launch.sandbox_locked);
        assert_eq!(parsed.cascade_group, launch.cascade_group);
        // `gait_dir`/`codebase` are private fields on `RunArgs` (consumed by
        // the caller in args.rs to prevent "unexpected argument" errors) —
        // parsing without error is the only thing observable from here, so
        // assert that plus the round-trip of everything that is `pub`.
    }

    #[test]
    fn run_launch_defaults_emit_no_optional_flags() {
        let launch = gait_core::launch::RunLaunch::default();
        let argv = launch.to_argv();
        assert!(argv.is_empty());
        let parsed = RunArgs::parse_from(&argv);
        assert_eq!(parsed.task, None);
        assert_eq!(parsed.session_id, None);
        assert!(!parsed.auto);
    }

    #[test]
    fn run_launch_to_process_argv_prepends_run_role_token() {
        let launch = gait_core::launch::RunLaunch {
            session: Some("s1".to_string()),
            auto: true,
            ..Default::default()
        };
        let argv = launch.to_process_argv();
        assert_eq!(argv[0], "run");
        // The remainder parses identically to `to_argv()` alone.
        assert_eq!(&argv[1..], launch.to_argv().as_slice());
    }
}

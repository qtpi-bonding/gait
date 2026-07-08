use clap::Parser;
use gait_bin::{Cli, Commands};
use std::path::PathBuf;

fn cwd_or_dot() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

#[tokio::main]
async fn main() {
    // Pre-check for IPC subcommands (__brain / __run-tool) before clap parsing.
    // These are spawned by the spine and must bypass the Commands enum to avoid
    // a crate DAG inversion (gait-core can't import from gait-bin).
    let raw: Vec<String> = std::env::args().collect();
    if raw.len() >= 2 && gait_core::is_role_argv(&raw[1..]) {
        // Sealed re-entry is fully owned by core: __dispatch_role parses its
        // own argv and runs run_brain/run_hand/cmd_daemon_exit. main() is a
        // pure OS entry stub here — no gait-cli hop (de-cycled 2026-07-08).
        gait_core::__dispatch_role(&raw[1..]).await;
        return;
    }

    // Pre-check for the `run` role token before clap parsing, for the same
    // DAG reason as above: `gait run …` is how a spine self-spawns another
    // `gait` process (`launch::RunLaunch::to_process_argv`, used by
    // launch_child/spawn_wake/spawn_agent) rather than calling `cmd_run`
    // in-process. `Cli`'s bare positional `task` field has no subcommand
    // named `run`, so an un-intercepted `"run"` token would otherwise bind
    // to `task` as a literal string and fail to parse the flags that follow.
    // `cmd_run` resolves `--gait-dir`/`--codebase` from argv itself.
    // `is_run_argv` is a pure, unit-tested function (gait-rs/src/lib.rs) —
    // see `is_run_argv_recognizes_the_run_role_token_and_only_that` for the
    // regression test guarding this exact dispatch.
    if raw.len() >= 2 && gait_core::is_run_argv(&raw[1..]) {
        gait_cli::cli::args::cmd_run(&raw[2..]).await;
        return;
    }

    let cli = Cli::parse();

    let resolve_gait_dir = || -> PathBuf {
        cli.gait_dir
            .clone()
            .or_else(|| {
                std::env::var("GAIT_DIR")
                    .ok()
                    .filter(|v| !v.is_empty())
                    .map(PathBuf::from)
            })
            .unwrap_or_else(cwd_or_dot)
    };

    // Mode dispatch: -e (exec), -i (interactive), bare positional (task)
    if let Some(exec_prompt) = cli.exec {
        let args = gait_cli::cli::exec::ExecArgs {
            model: cli.model,
            prompt: vec![exec_prompt],
        };
        gait_cli::cli::exec::cmd_exec(args).await;
        return;
    }

    if cli.interactive {
        let gait_dir = resolve_gait_dir();
        let codebase = cli.codebase.unwrap_or_else(cwd_or_dot);
        if let Err(e) = gait_repl::chat::run_chat(gait_dir, codebase, cli.agent, cli.session).await
        {
            eprintln!("[gait] error: {}", e);
            std::process::exit(1);
        }
        return;
    }

    if cli.task.is_some() {
        let gait_dir = resolve_gait_dir();
        let codebase = cli.codebase.clone().unwrap_or_else(cwd_or_dot);

        // Build argv for the existing cmd_run / RunArgs parser via the typed
        // launcher (spec-00-footguns.md item 4) — centralizes this site with
        // launch_child/spawn_wake/spawn_agent, which build the same argv shape
        // when self-spawning a `gait run` process.
        let run_argv = gait_core::launch::RunLaunch {
            task: cli.task.clone(),
            session: cli.session.clone(),
            auto: cli.auto_approve,
            model_override: cli.model.clone(),
            agent_name: cli.agent.clone(),
            allow: cli.allow.clone(),
            deny: cli.deny.clone(),
            unlock: cli.unlock.clone(),
            max_iter: cli.max_iter,
            result_schema_path: cli
                .result_schema
                .as_ref()
                .map(|rs| rs.to_string_lossy().to_string()),
            gait_dir: Some(gait_dir.clone()),
            codebase: Some(codebase.clone()),
            ..Default::default()
        }
        .to_argv();

        gait_cli::cli::run::cmd_run(&run_argv, gait_dir, codebase).await;
        return;
    }

    // Subcommand dispatch
    let command = match cli.command {
        Some(c) => c,
        None => {
            // No subcommand, no mode flag, no positional: print help.
            use clap::CommandFactory;
            Cli::command().print_help().unwrap();
            println!();
            std::process::exit(0);
        }
    };

    match command {
        Commands::Init(args) => gait_cli::cli::init::cmd_init(args),
        Commands::Sessions(args) => gait_cli::cli::session::cmd_sessions(args),
        Commands::Ps(args) => gait_cli::cli::session::cmd_ps(args),
        Commands::Stop(args) => gait_cli::cli::session::cmd_stop(args),
        Commands::Persist(args) => gait_cli::cli::session::cmd_persist(args),
        Commands::Discard(args) => gait_cli::cli::session::cmd_discard(args),
        Commands::Undo(args) => gait_cli::cli::session::cmd_undo(args),
        Commands::Stats(args) => gait_cli::cli::session::cmd_stats(args),
        Commands::Allow(args) => gait_cli::cli::session::cmd_allow(args),
        Commands::Deny(args) => gait_cli::cli::session::cmd_deny(args),
        Commands::Unlock(args) => gait_cli::cli::session::cmd_unlock(args),
        Commands::Reply(args) => gait_cli::cli::session::cmd_reply(args),
        Commands::Register(args) => {
            if let Err(e) = gait_cli::cli::admin::cmd_register(*args) {
                eprintln!("[gait] error: {}", e);
                std::process::exit(1);
            }
        }
        Commands::Unregister(args) => {
            if let Err(e) = gait_cli::cli::admin::cmd_unregister(*args) {
                eprintln!("[gait] error: {}", e);
                std::process::exit(1);
            }
        }
        Commands::Load(args) => gait_cli::cli::session::cmd_load(args),
        Commands::Unload(args) => gait_cli::cli::session::cmd_unload(args),
        Commands::Enable(args) => gait_cli::cli::admin::cmd_enable(args),
        Commands::Disable(args) => gait_cli::cli::admin::cmd_disable(args),
        Commands::Auth(args) => gait_cli::cli::admin::cmd_auth(args),
        Commands::Models(args) => gait_cli::cli::admin::cmd_models(args).await,
        Commands::Workflow(args) => gait_cli::cli::workflow::cmd_workflow(args).await,
        Commands::Daemons(args) => gait_cli::cli::daemons::cmd_daemons(args),
    }
}

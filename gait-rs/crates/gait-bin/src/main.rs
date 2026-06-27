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
        gait_cli::cli::args::cmd_dispatch_role(&raw[1..]).await;
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

        // Build argv for the existing cmd_run / RunArgs parser.
        let mut run_argv: Vec<String> = vec![];
        if let Some(ref task) = cli.task {
            run_argv.extend(["--task".to_string(), task.clone()]);
        }
        if let Some(ref s) = cli.session {
            run_argv.extend(["--session".to_string(), s.clone()]);
        }
        if cli.auto_approve {
            run_argv.push("--auto".to_string());
        }
        if let Some(ref m) = cli.model {
            run_argv.extend(["--model".to_string(), m.clone()]);
        }
        if let Some(ref a) = cli.agent {
            run_argv.extend(["--agent".to_string(), a.clone()]);
        }
        for p in &cli.allow {
            run_argv.extend(["--allow".to_string(), p.clone()]);
        }
        for p in &cli.deny {
            run_argv.extend(["--deny".to_string(), p.clone()]);
        }
        for p in &cli.unlock {
            run_argv.extend(["--unlock".to_string(), p.clone()]);
        }
        if let Some(n) = cli.max_iter {
            run_argv.extend(["--max-iter".to_string(), n.to_string()]);
        }
        if let Some(ref rs) = cli.result_schema {
            run_argv.extend([
                "--result-schema".to_string(),
                rs.to_string_lossy().to_string(),
            ]);
        }
        run_argv.extend([
            "--gait-dir".to_string(),
            gait_dir.to_string_lossy().to_string(),
        ]);
        run_argv.extend([
            "--codebase".to_string(),
            codebase.to_string_lossy().to_string(),
        ]);

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
            if let Err(e) = gait_cli::cli::admin::cmd_register(args) {
                eprintln!("[gait] error: {}", e);
                std::process::exit(1);
            }
        }
        Commands::Unregister(args) => {
            if let Err(e) = gait_cli::cli::admin::cmd_unregister(args) {
                eprintln!("[gait] error: {}", e);
                std::process::exit(1);
            }
        }
        Commands::Enable(args) => gait_cli::cli::admin::cmd_enable(args),
        Commands::Disable(args) => gait_cli::cli::admin::cmd_disable(args),
        Commands::Auth(args) => gait_cli::cli::admin::cmd_auth(args),
        Commands::Models(args) => gait_cli::cli::admin::cmd_models(args).await,
        Commands::Workflow(args) => gait_cli::cli::workflow::cmd_workflow(args).await,
        Commands::Daemons(args) => gait_cli::cli::daemons::cmd_daemons(args),
    }
}

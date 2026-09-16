use clap::{CommandFactory, Parser};
use gait_bin::min_cli::{MinCli, MinCommands};
use std::path::PathBuf;

fn cwd_or_dot() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

#[tokio::main]
async fn main() {
    // Pre-check for IPC subcommands (__brain / __run-tool) before clap
    // parsing, same DAG reason as full `gait`'s main.rs (gait-core can't
    // import from gait-bin). `__local-runner` is intentionally absent here:
    // `gait-min` has no `local` feature wiring.
    let raw: Vec<String> = std::env::args().collect();
    if raw.len() >= 2 && gait_core::is_role_argv(&raw[1..]) {
        gait_core::__dispatch_role(&raw[1..]).await;
        return;
    }

    // Pre-check for the `run` role token before clap parsing — see full
    // `gait`'s main.rs for the full rationale (`launch::RunLaunch`).
    if raw.len() >= 2 && gait_core::is_run_argv(&raw[1..]) {
        gait_cli::cli::args::cmd_run(&raw[2..]).await;
        return;
    }

    let cli = MinCli::parse();

    let cwd = cwd_or_dot();
    let gait_dir_arg = cli.gait_dir.clone();
    let resolve_gait_dir = || -> PathBuf {
        gait_core::gait_dir::resolve(gait_dir_arg.clone(), &cwd).unwrap_or_else(|e| {
            eprintln!("[gait] {e}");
            std::process::exit(1);
        })
    };

    // Mode dispatch: -e (exec), -i (interactive, plain REPL only), bare
    // positional (task)
    if let Some(exec_prompt) = cli.exec {
        let gait_dir = resolve_gait_dir();
        let codebase = cli.codebase.clone().unwrap_or_else(cwd_or_dot);
        let args = gait_cli::cli::exec::ExecArgs {
            inference: cli.inference,
            result_schema: cli.result_schema,
            result_schema_json: cli.result_schema_json,
            prompt: vec![exec_prompt],
        };
        gait_cli::cli::exec::cmd_exec(args, &gait_dir, &codebase).await;
        return;
    }

    if cli.interactive {
        let gait_dir = resolve_gait_dir();
        let codebase = cli.codebase.unwrap_or_else(cwd_or_dot);
        let args = gait_repl::plain::PlainReplArgs {
            gait_dir,
            codebase,
            agent_name: cli.agent,
            session_id: cli.session,
            model_override: cli.inference,
        };
        if let Err(e) = gait_repl::plain::run_plain_repl(args).await {
            eprintln!("[gait] error: {}", e);
            std::process::exit(1);
        }
        return;
    }

    if cli.task.is_some() {
        let gait_dir = resolve_gait_dir();
        let codebase = cli.codebase.clone().unwrap_or_else(cwd_or_dot);

        let run_argv = gait_core::launch::RunLaunch {
            task: cli.task.clone(),
            session: cli.session.clone(),
            auto: cli.auto_approve,
            inference: cli.inference.clone(),
            agent_name: cli.agent.clone(),
            allow: cli.allow.clone(),
            deny: cli.deny.clone(),
            unlock: cli.unlock.clone(),
            max_iter: cli.max_iter,
            result_schema_path: cli
                .result_schema
                .as_ref()
                .map(|rs| rs.to_string_lossy().to_string()),
            result_schema_json: cli.result_schema_json.clone(),
            gait_dir: Some(gait_dir.clone()),
            codebase: Some(codebase.clone()),
            allow_intree: cli.allow_intree,
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
            MinCli::command().print_help().unwrap();
            println!();
            std::process::exit(0);
        }
    };

    match command {
        MinCommands::Manual(args) => {
            // `gait-min manual print` writes to stdout only; it creates no
            // files, so it needs no gait-dir resolution (unlike full
            // `gait manual install`).
            if let Err(e) =
                gait_bin::manual::print(args, MinCli::command(), gait_repl::plain::MIN_REPL_COMMANDS)
            {
                eprintln!("[gait] error: {e}");
                std::process::exit(1);
            }
        }
        MinCommands::Init(args) => gait_cli::cli::init::cmd_init(args),
        MinCommands::Link(args) => gait_cli::cli::link::cmd_link(args),
        MinCommands::Sessions(args) => gait_cli::cli::session::cmd_sessions(args),
        MinCommands::Ps(args) => gait_cli::cli::session::cmd_ps(args),
        MinCommands::Stop(args) => gait_cli::cli::session::cmd_stop(args),
        MinCommands::Persist(args) => gait_cli::cli::session::cmd_persist(args),
        MinCommands::Discard(args) => gait_cli::cli::session::cmd_discard(args),
        MinCommands::Undo(args) => gait_cli::cli::session::cmd_undo(args),
        MinCommands::Stats(args) => gait_cli::cli::session::cmd_stats(args),
        MinCommands::Log(args) => gait_cli::cli::session::cmd_log(args),
        MinCommands::Allow(args) => gait_cli::cli::session::cmd_allow(args),
        MinCommands::Deny(args) => gait_cli::cli::session::cmd_deny(args),
        MinCommands::Unlock(args) => gait_cli::cli::session::cmd_unlock(args),
        MinCommands::Reply(args) => gait_cli::cli::session::cmd_reply(args),
        MinCommands::Config(args) => gait_cli::cli::admin::cmd_config(args),

        MinCommands::Register(args) => {
            if let Err(e) = gait_cli::cli::admin::cmd_register(*args) {
                eprintln!("[gait] error: {}", e);
                std::process::exit(1);
            }
        }
        MinCommands::Unregister(args) => {
            if let Err(e) = gait_cli::cli::admin::cmd_unregister(*args) {
                eprintln!("[gait] error: {}", e);
                std::process::exit(1);
            }
        }
        MinCommands::Load(args) => gait_cli::cli::session::cmd_load(args),
        MinCommands::Unload(args) => gait_cli::cli::session::cmd_unload(args),
        MinCommands::Enable(args) => gait_cli::cli::admin::cmd_enable(args),
        MinCommands::Disable(args) => gait_cli::cli::admin::cmd_disable(args),
        MinCommands::Auth(args) => gait_cli::cli::admin::cmd_auth(args).await,
        MinCommands::Models(args) => gait_cli::cli::admin::cmd_models(args).await,
        MinCommands::Inference(args) => gait_cli::cli::admin::cmd_inference(args),
        MinCommands::Daemons(args) => gait_cli::cli::daemons::cmd_daemons(args),
    }
}

use clap::Parser;
use gait_bin::{Cli, Commands};
use gait_identity::IdentityContext;
use std::path::PathBuf;
use std::process::ExitCode;

fn cwd_or_dot() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

fn main() -> ExitCode {
    let raw: Vec<String> = std::env::args().collect();
    if let Ok(cli) = Cli::try_parse_from(&raw) {
        gait_bin::apply_agent_dir_flag(&cli);
    }
    let identity = IdentityContext::from_env();
    let agent_id = identity.as_ref().ok().and_then(|value| value.as_ref())
        .map(|ctx| ctx.agent_id().to_string());
    if let Err(error) = gait_bin::install_agent_identity(identity) {
        return gait_cli::cli::error::report(&error);
    }
    let is_role = raw.get(1).map(String::as_str) == Some("__local-runner")
        || (raw.len() >= 2 && gait_core::is_role_argv(&raw[1..]));
    let _guard = if !is_role {
        Some(gait_bin::init_telemetry("gait", env!("CARGO_PKG_VERSION"), agent_id))
    } else {
        None
    };
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to start the tokio runtime")
        .block_on(run(raw))
}

async fn run(raw: Vec<String>) -> ExitCode {
    if raw.get(1).map(String::as_str) == Some("__local-runner") {
        #[cfg(feature = "local")]
        {
            if let Err(error) = gait_llama_cpp::run_local_runner(&raw[2..]).await {
                eprintln!("[gait] local runner: {error}");
                return ExitCode::from(1);
            }
            return ExitCode::SUCCESS;
        }
        #[cfg(not(feature = "local"))]
        {
            eprintln!("[gait] local runner requires a Gait build with the 'local' feature");
            return ExitCode::from(1);
        }
    }
    if raw.len() >= 2 && gait_core::is_role_argv(&raw[1..]) {
        gait_core::__dispatch_role(&raw[1..]).await;
        return ExitCode::SUCCESS;
    }
    if raw.len() >= 2 && gait_core::is_run_argv(&raw[1..]) {
        #[cfg(feature = "telemetry")]
        {
            use tracing::Instrument;
            let span = tracing::info_span!("gait.run");
            concord_telemetry::propagation::set_parent_from_env(&span);
            return gait_bin::finish(
                gait_cli::cli::args::cmd_run(&raw[2..]).instrument(span).await,
            );
        }
        #[cfg(not(feature = "telemetry"))]
        {
            return gait_bin::finish(gait_cli::cli::args::cmd_run(&raw[2..]).await);
        }
    }

    let cli = Cli::parse();
    let cwd = cwd_or_dot();
    let gait_dir_arg = cli.gait_dir.clone();
    let resolve_gait_dir = || -> anyhow::Result<PathBuf> {
        gait_core::gait_dir::resolve(gait_dir_arg.clone(), &cwd)
    };

    if let Some(exec_prompt) = cli.exec {
        let gait_dir = match resolve_gait_dir() {
            Ok(dir) => dir,
            Err(e) => return gait_cli::cli::error::report(&e),
        };
        let codebase = cli.codebase.clone().unwrap_or_else(cwd_or_dot);
        let args = gait_cli::cli::exec::ExecArgs {
            inference: cli.inference,
            result_schema: cli.result_schema,
            result_schema_json: cli.result_schema_json,
            prompt: vec![exec_prompt],
        };
        return gait_bin::finish(gait_cli::cli::exec::cmd_exec(args, &gait_dir, &codebase).await);
    }

    if cli.interactive {
        let gait_dir = match resolve_gait_dir() {
            Ok(dir) => dir,
            Err(e) => return gait_cli::cli::error::report(&e),
        };
        let codebase = cli.codebase.unwrap_or_else(cwd_or_dot);
        return gait_bin::finish_unit(gait_repl::chat::run_chat(
            gait_dir, codebase, cli.agent, cli.session, cli.inference, cli.verified_goal,
        ).await);
    }

    if cli.task.is_some() {
        let gait_dir = match resolve_gait_dir() {
            Ok(dir) => dir,
            Err(e) => return gait_cli::cli::error::report(&e),
        };
        let codebase = cli.codebase.clone().unwrap_or_else(cwd_or_dot);
        let run_argv = gait_core::launch::RunLaunch {
            task: cli.task.clone(), session: cli.session.clone(), auto: cli.auto_approve,
            inference: cli.inference.clone(), agent_name: cli.agent.clone(), allow: cli.allow.clone(),
            deny: cli.deny.clone(), unlock: cli.unlock.clone(), max_iter: cli.max_iter,
            result_schema_path: cli.result_schema.as_ref().map(|rs| rs.to_string_lossy().to_string()),
            result_schema_json: cli.result_schema_json.clone(), gait_dir: Some(gait_dir.clone()),
            codebase: Some(codebase.clone()), allow_intree: cli.allow_intree, ..Default::default()
        }.to_argv();
        return gait_bin::finish(gait_cli::cli::run::cmd_run(&run_argv, gait_dir, codebase).await);
    }

    let command = match cli.command {
        Some(c) => c,
        None => {
            use clap::CommandFactory;
            Cli::command().print_help().unwrap();
            println!();
            return ExitCode::SUCCESS;
        }
    };

    match command {
        Commands::Manual(args) => {
            let manual_gait_dir = match &args.command {
                gait_bin::manual::ManualCommand::Install(options) => options.gait_dir.clone(),
            };
            let gait_dir = match gait_core::gait_dir::resolve(manual_gait_dir.or(gait_dir_arg), &cwd) {
                Ok(dir) => dir,
                Err(e) => return gait_cli::cli::error::report(&anyhow::anyhow!(e)),
            };
            gait_bin::finish_unit(gait_bin::manual::install(args, &gait_dir).map_err(|e| anyhow::anyhow!(e)))
        }
        Commands::Init(args) => gait_bin::finish_unit(gait_cli::cli::init::cmd_init(args)),
        Commands::Link(args) => gait_bin::finish_unit(gait_cli::cli::link::cmd_link(args)),
        Commands::Sessions(args) => gait_bin::finish_unit(gait_cli::cli::session::cmd_sessions(args)),
        Commands::Ps(args) => gait_bin::finish_unit(gait_cli::cli::session::cmd_ps(args)),
        Commands::Stop(args) => gait_bin::finish_unit(gait_cli::cli::session::cmd_stop(args)),
        Commands::Persist(args) => gait_bin::finish_unit(gait_cli::cli::session::cmd_persist(args)),
        Commands::Discard(args) => gait_bin::finish_unit(gait_cli::cli::session::cmd_discard(args)),
        Commands::Undo(args) => gait_bin::finish_unit(gait_cli::cli::session::cmd_undo(args)),
        Commands::Stats(args) => gait_bin::finish_unit(gait_cli::cli::session::cmd_stats(args)),
        Commands::Log(args) => gait_bin::finish_unit(gait_cli::cli::session::cmd_log(args)),
        Commands::Allow(args) => gait_bin::finish_unit(gait_cli::cli::session::cmd_allow(args)),
        Commands::Deny(args) => gait_bin::finish_unit(gait_cli::cli::session::cmd_deny(args)),
        Commands::Unlock(args) => gait_bin::finish_unit(gait_cli::cli::session::cmd_unlock(args)),
        Commands::Reply(args) => gait_bin::finish_unit(gait_cli::cli::session::cmd_reply(args)),
        Commands::Config(args) => gait_bin::finish_unit(gait_cli::cli::admin::cmd_config(args)),
        Commands::Register(args) => gait_bin::finish_unit(gait_cli::cli::admin::cmd_register(*args)),
        Commands::Unregister(args) => gait_bin::finish_unit(gait_cli::cli::admin::cmd_unregister(*args)),
        Commands::Load(args) => gait_bin::finish_unit(gait_cli::cli::session::cmd_load(args)),
        Commands::Unload(args) => gait_bin::finish_unit(gait_cli::cli::session::cmd_unload(args)),
        Commands::Enable(args) => gait_bin::finish_unit(gait_cli::cli::admin::cmd_enable(args)),
        Commands::Disable(args) => gait_bin::finish_unit(gait_cli::cli::admin::cmd_disable(args)),
        Commands::Auth(args) => gait_bin::finish_unit(gait_cli::cli::admin::cmd_auth(args).await),
        Commands::Models(args) => gait_bin::finish_unit(gait_cli::cli::admin::cmd_models(args).await),
        #[cfg(feature = "local")]
        Commands::Local(args) => gait_bin::finish_unit(gait_cli::cli::admin::cmd_local(args).await),
        Commands::Inference(args) => gait_bin::finish_unit(gait_cli::cli::admin::cmd_inference(args)),
        Commands::Mode(args) => gait_bin::finish_unit(gait_cli::cli::admin::cmd_mode(args)),
        #[cfg(feature = "workflow")]
        Commands::Workflow(args) => gait_bin::finish(gait_cli::cli::workflow::cmd_workflow(args).await),
        Commands::Daemons(args) => gait_bin::finish_unit(gait_cli::cli::daemons::cmd_daemons(args)),
    }
}

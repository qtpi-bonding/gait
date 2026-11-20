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
        return gait_bin::report(&error);
    }
    // Every process initialises telemetry, the hidden role subprocesses too,
    // each under its own service name. The spine hands the brain and the hand
    // their parent trace through TRACEPARENT/BAGGAGE.
    // The hand is the exception: it seals itself with Landlock, so its telemetry
    // starts after the seal and it runs on the main thread alone.
    let _guard = gait_bin::start_telemetry(&raw, "gait", agent_id);
    gait_bin::build_runtime(&raw)
        .block_on(run(raw))
}

async fn run(raw: Vec<String>) -> ExitCode {
    if raw.get(1).map(String::as_str) == Some("__local-runner") {
        #[cfg(feature = "local")]
        {
            if let Err(error) = gait_llama_cpp::run_local_runner(&raw[2..]).await {
                return gait_bin::report(&gait_cli::cli::error::CliError::failed(format!(
                    "local runner: {error}"
                )));
            }
            return ExitCode::SUCCESS;
        }
        #[cfg(not(feature = "local"))]
        {
            return gait_bin::report(&gait_cli::cli::error::CliError::usage(
                "local runner requires a Gait build with the 'local' feature",
            ));
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
            return gait_bin::finish_code(
                gait_cli::cli::args::cmd_run(&raw[2..]).instrument(span).await,
            );
        }
        #[cfg(not(feature = "telemetry"))]
        {
            return gait_bin::finish_code(gait_cli::cli::args::cmd_run(&raw[2..]).await);
        }
    }
    use clap::CommandFactory;
    let verbs = gait_bin::interim::verbs(&Cli::command());
    let registry = gait_bin::ops::registry();
    let registry_verbs = gait_bin::interim::registry_verbs(&registry);
    if gait_bin::interim::is_root_help(&raw) {
        println!("{}", gait_bin::cli().render_help());
        return ExitCode::SUCCESS;
    }
    match gait_bin::interim::route(&raw, &verbs, &registry_verbs) {
        gait_bin::interim::Route::Registry => {
            let argv = raw.iter().map(std::ffi::OsString::from).collect();
            op_registry::cli::run(&registry, argv, &gait_bin::ops::GaitHooks).await
        }
        gait_bin::interim::Route::NearMiss(detail) => {
            ExitCode::from(gait_bin::interim::near_miss_exit(detail))
        }
        gait_bin::interim::Route::Old => {
            #[cfg(feature = "telemetry")]
            {
                use tracing::Instrument;
                let span = tracing::info_span!("gait.cli");
                concord_telemetry::propagation::set_parent_from_env(&span);
                return run_cli(raw).instrument(span).await;
            }
            #[cfg(not(feature = "telemetry"))]
            {
                return run_cli(raw).await;
            }
        }
    }
}

/// The management verbs and the bare `gait`, still parsed by the old clap enum
/// (interim; Task 9 removes it). The turn is the registry's fallback.
async fn run_cli(raw: Vec<String>) -> ExitCode {
    let cli = match Cli::try_parse_from(&raw) {
        Ok(cli) => cli,
        Err(e) if e.use_stderr() => {
            return gait_bin::report(&gait_cli::cli::error::CliError::usage(
                e.to_string().trim_end(),
            ));
        }
        Err(e) => {
            let _ = e.print();
            return ExitCode::SUCCESS;
        }
    };
    let cwd = cwd_or_dot();
    let gait_dir_arg = cli.gait_dir.clone();

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
                Err(e) => return gait_bin::report(&anyhow::anyhow!(e)),
            };
            gait_bin::finish_unit(gait_bin::manual::install(args, &gait_dir).map_err(|e| anyhow::anyhow!(e)))
        }
        Commands::Init(args) => gait_bin::finish_unit(gait_cli::cli::init::cmd_init(args)),
        Commands::Link(args) => gait_bin::finish_unit(gait_cli::cli::link::cmd_link(args)),
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

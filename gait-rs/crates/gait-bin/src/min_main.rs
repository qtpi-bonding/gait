use clap::{CommandFactory, Parser};
use gait_bin::min_cli::{MinCli, MinCommands};
use std::process::ExitCode;

fn main() -> ExitCode {
    let raw: Vec<String> = std::env::args().collect();
    let identity = gait_identity::IdentityContext::from_env();
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
    let _guard = gait_bin::start_telemetry(&raw, "gait-min", agent_id);
    gait_bin::build_runtime(&raw)
        .block_on(run(raw))
}

async fn run(raw: Vec<String>) -> ExitCode {
    // Pre-check for IPC subcommands (__brain / __run-tool) before clap
    // parsing, same DAG reason as full `gait`'s main.rs (gait-core can't
    // import from gait-bin). `__local-runner` is intentionally absent here:
    // `gait-min` has no `local` feature wiring.
    // Installed before any dispatch so role children (brain, daemon-exit) verify
    // direct provenance the same way `gait` does.
    if raw.len() >= 2 && gait_core::is_role_argv(&raw[1..]) {
        gait_core::__dispatch_role(&raw[1..]).await;
        return ExitCode::SUCCESS;
    }

    // Pre-check for the `run` role token before clap parsing — see full
    // `gait`'s main.rs for the full rationale (`launch::RunLaunch`).
    if raw.len() >= 2 && gait_core::is_run_argv(&raw[1..]) {
        return gait_bin::finish_code(gait_cli::cli::args::cmd_run(&raw[2..]).await);
    }

    // Interim (Task 9 removes it): the old enum keeps the verbs, `--help`
    // and the bare command; the turn is the registry's fallback.
    let verbs = gait_bin::interim::verbs(&MinCli::command());
    match gait_bin::interim::route(&raw, &verbs) {
        gait_bin::interim::Route::Registry => {
            let argv = raw.iter().map(std::ffi::OsString::from).collect();
            return op_registry::cli::run(
                &gait_bin::ops::min_registry(),
                argv,
                &gait_bin::ops::GaitHooks,
            )
            .await;
        }
        gait_bin::interim::Route::NearMiss(detail) => {
            return ExitCode::from(gait_bin::interim::near_miss_exit(detail));
        }
        gait_bin::interim::Route::Old => {}
    }
    run_cli(raw).await
}

async fn run_cli(raw: Vec<String>) -> ExitCode {
    let cli = match MinCli::try_parse_from(&raw) {
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

    // Subcommand dispatch
    let command = match cli.command {
        Some(c) => c,
        None => {
            MinCli::command().print_help().unwrap();
            println!();
            return ExitCode::SUCCESS;
        }
    };

    match command {
        MinCommands::Manual(args) => {
            // `gait-min manual print` writes to stdout only; it creates no
            // files, so it needs no gait-dir resolution (unlike full
            // `gait manual install`).
            if let Err(e) = gait_bin::manual::print(
                args,
                MinCli::command(),
                gait_repl::plain::MIN_REPL_COMMANDS,
            ) {
                return gait_bin::report(&anyhow::anyhow!(e));
            }
            return ExitCode::SUCCESS;
        }
        MinCommands::Init(args) => gait_bin::finish_unit(gait_cli::cli::init::cmd_init(args)),
        MinCommands::Link(args) => gait_bin::finish_unit(gait_cli::cli::link::cmd_link(args)),
        MinCommands::Sessions(args) => gait_bin::finish_unit(gait_cli::cli::session::cmd_sessions(args)),
        MinCommands::Ps(args) => gait_bin::finish_unit(gait_cli::cli::session::cmd_ps(args)),
        MinCommands::Stop(args) => gait_bin::finish_unit(gait_cli::cli::session::cmd_stop(args)),
        MinCommands::Persist(args) => gait_bin::finish_unit(gait_cli::cli::session::cmd_persist(args)),
        MinCommands::Discard(args) => gait_bin::finish_unit(gait_cli::cli::session::cmd_discard(args)),
        MinCommands::Undo(args) => gait_bin::finish_unit(gait_cli::cli::session::cmd_undo(args)),
        MinCommands::Stats(args) => gait_bin::finish_unit(gait_cli::cli::session::cmd_stats(args)),
        MinCommands::Log(args) => gait_bin::finish_unit(gait_cli::cli::session::cmd_log(args)),
        MinCommands::Allow(args) => gait_bin::finish_unit(gait_cli::cli::session::cmd_allow(args)),
        MinCommands::Deny(args) => gait_bin::finish_unit(gait_cli::cli::session::cmd_deny(args)),
        MinCommands::Unlock(args) => gait_bin::finish_unit(gait_cli::cli::session::cmd_unlock(args)),
        MinCommands::Reply(args) => gait_bin::finish_unit(gait_cli::cli::session::cmd_reply(args)),
        MinCommands::Config(args) => gait_bin::finish_unit(gait_cli::cli::admin::cmd_config(args)),

        MinCommands::Register(args) => {
            if let Err(e) = gait_cli::cli::admin::cmd_register(*args) {
                return gait_bin::report(&anyhow::anyhow!(e));
            }
            ExitCode::SUCCESS
        }
        MinCommands::Unregister(args) => {
            if let Err(e) = gait_cli::cli::admin::cmd_unregister(*args) {
                return gait_bin::report(&anyhow::anyhow!(e));
            }
            ExitCode::SUCCESS
        }
        MinCommands::Load(args) => gait_bin::finish_unit(gait_cli::cli::session::cmd_load(args)),
        MinCommands::Unload(args) => gait_bin::finish_unit(gait_cli::cli::session::cmd_unload(args)),
        MinCommands::Enable(args) => gait_bin::finish_unit(gait_cli::cli::admin::cmd_enable(args)),
        MinCommands::Disable(args) => gait_bin::finish_unit(gait_cli::cli::admin::cmd_disable(args)),
        MinCommands::Auth(args) => gait_bin::finish_unit(gait_cli::cli::admin::cmd_auth(args).await),
        MinCommands::Models(args) => gait_bin::finish_unit(gait_cli::cli::admin::cmd_models(args).await),
        MinCommands::Inference(args) => gait_bin::finish_unit(gait_cli::cli::admin::cmd_inference(args)),
        MinCommands::Mode(args) => gait_bin::finish_unit(gait_cli::cli::admin::cmd_mode(args)),
        MinCommands::Daemons(args) => gait_bin::finish_unit(gait_cli::cli::daemons::cmd_daemons(args)),
    }
}

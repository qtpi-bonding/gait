use gait_identity::IdentityContext;
use std::process::ExitCode;

fn main() -> ExitCode {
    let raw: Vec<String> = std::env::args().collect();
    // `--identity-dir` must reach the environment before any thread exists, so
    // it is read from the raw arguments, ahead of the registry's own parse.
    gait_bin::apply_identity_dir_flag(&raw);
    let identity = IdentityContext::from_env();
    let agent_id = identity
        .as_ref()
        .ok()
        .and_then(|value| value.as_ref())
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
    gait_bin::build_runtime(&raw).block_on(run(raw))
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
                gait_cli::cli::args::cmd_run(&raw[2..])
                    .instrument(span)
                    .await,
            );
        }
        #[cfg(not(feature = "telemetry"))]
        {
            return gait_bin::finish_code(gait_cli::cli::args::cmd_run(&raw[2..]).await);
        }
    }
    gait_bin::run_registry(raw, gait_bin::ops::registry()).await
}

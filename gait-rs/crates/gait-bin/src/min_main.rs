use std::process::ExitCode;

fn main() -> ExitCode {
    let raw: Vec<String> = std::env::args().collect();
    // `--agent-dir` must reach the environment before any thread exists.
    gait_bin::apply_agent_dir_flag(&raw);
    let identity = gait_identity::IdentityContext::from_env();
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
    let _guard = gait_bin::start_telemetry(&raw, "gait-min", agent_id);
    gait_bin::build_runtime(&raw).block_on(run(raw))
}

async fn run(raw: Vec<String>) -> ExitCode {
    // The hidden role arguments (`__brain`, `__run-tool`) are matched before
    // the registry parses anything, as in full `gait`'s main.rs (gait-core
    // can't import from gait-bin). `__local-runner` is absent: `gait-min` has
    // no `local` feature wiring.
    if raw.len() >= 2 && gait_core::is_role_argv(&raw[1..]) {
        gait_core::__dispatch_role(&raw[1..]).await;
        return ExitCode::SUCCESS;
    }
    // The hidden turn token; see full `gait`'s main.rs (`launch::RunLaunch`).
    if raw.len() >= 2 && gait_core::is_run_argv(&raw[1..]) {
        return gait_bin::finish_code(gait_cli::cli::args::cmd_run(&raw[2..]).await);
    }
    gait_bin::run_registry(raw, gait_bin::ops::min_registry()).await
}

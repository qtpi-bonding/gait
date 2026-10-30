use anyhow::Context;
use clap::{ArgAction, CommandFactory, Parser, Subcommand};
use gait_identity::{DirectVerifier, IdentityContext};
use gait_state::session::retrace::install_process_verifier;
use std::path::PathBuf;
use std::sync::Arc;
use std::process::ExitCode;

#[cfg(feature = "telemetry")]
use concord_telemetry::{Config as TelemetryConfig, LocalOutput, Principal, PrincipalKind};

#[cfg(feature = "local")]
use gait_cli::cli::admin::LocalArgs;
use gait_cli::cli::admin::{
    AuthArgs, ConfigArgs, DisableArgs, EnableArgs, InferenceArgs, ModeArgs, ModelsArgs,
    RegisterArgs, UnregisterArgs,
};
use gait_cli::cli::daemons::DaemonsArgs;
use gait_cli::cli::init::InitArgs;
use gait_cli::cli::link::LinkArgs;
use gait_cli::cli::session::{
    AllowArgs, DenyArgs, DiscardArgs, LoadArgs, LogArgs, PersistArgs, PsArgs, ReplyArgs,
    SessionsArgs, StatsArgs, StopArgs, UndoArgs, UnloadArgs, UnlockArgs,
};
#[cfg(feature = "workflow")]
use gait_cli::cli::workflow::WorkflowArgs;

pub mod manual;
#[cfg(feature = "min")]
pub mod min_cli;

#[derive(Parser)]
#[command(
    name = "gait",
    about = "AI coding agent",
    version,
    args_conflicts_with_subcommands = true
)]
pub struct Cli {
    /// Task to run (bare positional: `gait "fix the login bug"`)
    #[arg(conflicts_with_all = ["interactive", "exec"])]
    pub task: Option<String>,

    /// Interactive REPL mode
    #[arg(short = 'i', long, conflicts_with_all = ["exec", "task"])]
    pub interactive: bool,
    /// Ephemeral one-off request
    #[arg(short = 'e', long, conflicts_with_all = ["interactive", "task"])]
    pub exec: Option<String>,

    /// Continue an existing session by id
    #[arg(long, short = 's')]
    pub session: Option<String>,
    /// Named inference selection.
    #[arg(long)]
    pub inference: Option<String>,
    /// Verified goal: fixed command run when a turn attempts completion to verify its goal.
    #[arg(long)]
    pub verified_goal: Option<String>,
    /// Agent file to load
    #[arg(long, short = 'a')]
    pub agent: Option<String>,
    /// auto-approve all tool-permission prompts; the sandbox ceiling still applies (kernel-enforced)
    #[arg(long)]
    pub auto_approve: bool,
    /// Pre-approve tool pattern (repeatable)
    #[arg(long, action = ArgAction::Append)]
    pub allow: Vec<String>,
    /// Pre-deny tool pattern (repeatable)
    #[arg(long, action = ArgAction::Append)]
    pub deny: Vec<String>,
    /// Pre-grant sandbox path access (repeatable)
    #[arg(long, action = ArgAction::Append)]
    pub unlock: Vec<String>,
    /// Cap agentic steps
    #[arg(long, short = 'n')]
    pub max_iter: Option<usize>,
    /// Path to structured output schema
    #[arg(long, short = 'R')]
    pub result_schema: Option<PathBuf>,
    /// Inline JSON structured output schema
    #[arg(long, conflicts_with = "result_schema")]
    pub result_schema_json: Option<String>,
    #[arg(long, short = 'd', hide = true)]
    pub gait_dir: Option<PathBuf>,
    #[arg(long, short = 'c', hide = true)]
    pub codebase: Option<PathBuf>,
    /// Override the in-tree safety guard (design §7): run even though the
    /// gait-dir is inside the codebase. The agent could then write to its own
    /// config/permissions/audit log — prefer `gait init ~/gait-home && gait
    /// link ~/gait-home` instead.
    #[arg(long)]
    pub allow_intree: bool,
    /// Agent identity directory. Sets GAIT_AGENT_DIR for this process and every child it spawns.
    #[arg(long, global = true, value_name = "PATH")]
    pub agent_dir: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Subcommand)]
pub enum Commands {
    Manual(manual::ManualArgs),
    Init(InitArgs),
    Link(LinkArgs),
    Sessions(SessionsArgs),
    Ps(PsArgs),
    Stop(StopArgs),
    Persist(PersistArgs),
    Discard(DiscardArgs),
    Undo(UndoArgs),
    Stats(StatsArgs),
    Log(LogArgs),
    Allow(AllowArgs),
    Deny(DenyArgs),
    Unlock(UnlockArgs),
    Reply(ReplyArgs),
    Config(ConfigArgs),

    Register(Box<RegisterArgs>),
    Unregister(Box<UnregisterArgs>),
    Load(LoadArgs),
    Unload(UnloadArgs),
    Enable(EnableArgs),
    Disable(DisableArgs),
    Auth(AuthArgs),
    Models(ModelsArgs),
    #[cfg(feature = "local")]
    Local(LocalArgs),
    Inference(InferenceArgs),
    Mode(ModeArgs),
    #[cfg(feature = "workflow")]
    Workflow(WorkflowArgs),
    Daemons(DaemonsArgs),
}

pub fn cli() -> clap::Command {
    Cli::command()
}

/// Record the error (code, class, fingerprint, never the text) and return its exit code.
pub fn report(error: &anyhow::Error) -> ExitCode {
    #[cfg(feature = "telemetry")]
    concord_telemetry::error_anyhow!(error, source = "gait.cli");
    gait_cli::cli::error::report(error)
}

pub fn finish(result: anyhow::Result<ExitCode>) -> ExitCode {
    match result {
        Ok(code) => code,
        Err(error) => report(&error),
    }
}

pub fn finish_unit(result: gait_cli::cli::error::CliResult) -> ExitCode {
    finish(result.map(|()| ExitCode::SUCCESS))
}

/// Makes `--agent-dir` visible to this process and to every child it spawns by
/// setting `GAIT_AGENT_DIR`. Call before any thread exists: the environment is
/// only safe to mutate while the process is single-threaded.
pub fn apply_agent_dir_flag(cli: &Cli) {
    if let Some(dir) = &cli.agent_dir {
        std::env::set_var("GAIT_AGENT_DIR", dir);
    }
}

/// Installs the process-wide provenance verifier for the agent identity. No
/// identity means nothing is installed. An identity directory that cannot be
/// loaded is a startup error that names the directory, rather than a fold that
/// silently verifies nothing.
pub fn install_agent_identity(
    identity: anyhow::Result<Option<IdentityContext>>,
) -> anyhow::Result<()> {
    let Some(ctx) = identity.context("GAIT_AGENT_DIR is set but cannot be loaded")? else {
        return Ok(());
    };
    install_process_verifier(Arc::new(DirectVerifier::from_context(&ctx)?));
    Ok(())
}

/// Keeps telemetry alive until the command's runtime has finished.
pub struct TelemetryGuard {
    // Held only so its Drop flushes at the end of main.
    #[cfg(feature = "telemetry")]
    #[allow(dead_code)]
    guard: Option<concord_telemetry::Guard>,
}

/// Initialise process telemetry without ever disturbing `gait run`'s stderr
/// JSONL protocol. Telemetry is deliberately optional for the lean binary.
pub fn init_telemetry(
    service: &'static str,
    version: &'static str,
    agent_id: Option<String>,
) -> TelemetryGuard {
    #[cfg(feature = "telemetry")]
    {
        let mut config = TelemetryConfig::new(service, version);
        config.local = LocalOutput::Off;
        config.principal = agent_id.map(|id| Principal::new(PrincipalKind::Agent, id));
        return TelemetryGuard { guard: concord_telemetry::init(config).ok() };
    }
    #[cfg(not(feature = "telemetry"))]
    {
        let _ = (service, version, agent_id);
        TelemetryGuard {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[serial_test::serial]
    fn agent_dir_flag_sets_gait_agent_dir_env() {
        let value = format!("/nonexistent/gait-agent-dir-flag-{}", std::process::id());
        let previous = std::env::var_os("GAIT_AGENT_DIR");
        let cli = Cli::try_parse_from(["gait", "--agent-dir", value.as_str()]).unwrap();
        apply_agent_dir_flag(&cli);
        let observed = std::env::var_os("GAIT_AGENT_DIR");
        match previous {
            Some(v) => std::env::set_var("GAIT_AGENT_DIR", v),
            None => std::env::remove_var("GAIT_AGENT_DIR"),
        }
        assert_eq!(observed.as_deref(), Some(std::ffi::OsStr::new(&value)));
    }

    #[test]
    fn unloadable_agent_dir_is_a_clear_startup_error() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("no-such-agent");
        let err = install_agent_identity(IdentityContext::new(&missing).map(Some)).unwrap_err();
        let text = format!("{err:#}");
        assert!(
            text.contains(&missing.display().to_string()),
            "the startup error names the directory: {text}"
        );
    }

    #[test]
    fn no_agent_identity_installs_nothing_and_succeeds() {
        install_agent_identity(Ok(None)).unwrap();
    }

    #[test]
    #[serial_test::serial]
    fn init_telemetry_without_endpoint_is_silent_and_a_second_init_yields_an_inert_guard() {
        let previous = std::env::var_os("OTEL_EXPORTER_OTLP_ENDPOINT");
        std::env::remove_var("OTEL_EXPORTER_OTLP_ENDPOINT");
        let _first = init_telemetry("gait-test", "0", None);
        let second = init_telemetry("gait-test", "0", Some("agent".to_string()));
        match previous {
            Some(value) => std::env::set_var("OTEL_EXPORTER_OTLP_ENDPOINT", value),
            None => std::env::remove_var("OTEL_EXPORTER_OTLP_ENDPOINT"),
        }
        // A global subscriber is installed by now (by the first call, or by
        // whoever got there first), so the second init is refused and the
        // guard it returns holds nothing.
        #[cfg(feature = "telemetry")]
        assert!(second.guard.is_none());
        #[cfg(not(feature = "telemetry"))]
        let _ = second;
    }

    #[test]
    fn identity_subcommand_is_gone() {
        assert!(Cli::try_parse_from(["gait", "identity", "self-proof"]).is_err());
        assert!(!cli().get_subcommands().any(|c| c.get_name() == "identity"));
    }

    #[test]
    fn finish_maps_ok_code_through() {
        assert_eq!(finish(Ok(ExitCode::from(12))), ExitCode::from(12));
    }

    #[test]
    fn finish_maps_logical_error_to_65() {
        assert_eq!(finish(Err(gait_cli::cli::error::CliError::not_found("missing"))), ExitCode::from(65));
    }

    #[test]
    fn finish_maps_infrastructure_error_to_69() {
        assert_eq!(finish(Err(gait_cli::cli::error::CliError::failed("broken"))), ExitCode::from(69));
    }

    #[test]
    fn finish_maps_uncoded_error_to_70() {
        assert_eq!(finish(Err(anyhow::anyhow!("unknown"))), ExitCode::from(70));
    }

    #[test]
    fn no_process_exit_left_in_cli_crates() {
        fn rust_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    rust_files(&path, out);
                } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
                    out.push(path);
                }
            }
        }

        let crates = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let mut files = Vec::new();
        for relative in ["gait-cli/src", "gait-repl/src"] {
            rust_files(&crates.join(relative), &mut files);
        }
        files.push(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/main.rs"));
        files.push(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/min_main.rs"));
        let offenders: Vec<_> = files
            .iter()
            .filter_map(|path| {
                let source = std::fs::read_to_string(path).unwrap();
                source.lines().any(|line| {
                    let code = line.split("//").next().unwrap_or("");
                    code.contains("process::exit")
                }).then_some(path.display().to_string())
            })
            .collect();
        assert!(offenders.is_empty(), "process::exit remains in: {offenders:?}");
    }
}

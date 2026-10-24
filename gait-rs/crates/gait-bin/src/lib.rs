use anyhow::Context;
use clap::{ArgAction, CommandFactory, Parser, Subcommand};
use gait_identity::{DirectVerifier, IdentityContext};
use gait_state::session::retrace::install_process_verifier;
use std::path::PathBuf;
use std::sync::Arc;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
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
    fn identity_subcommand_is_gone() {
        assert!(Cli::try_parse_from(["gait", "identity", "self-proof"]).is_err());
        assert!(!cli().get_subcommands().any(|c| c.get_name() == "identity"));
    }
}

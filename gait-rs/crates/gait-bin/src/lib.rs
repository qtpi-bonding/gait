use clap::{ArgAction, CommandFactory, Parser, Subcommand};
use std::path::PathBuf;

#[cfg(feature = "local")]
use gait_cli::cli::admin::LocalArgs;
use gait_cli::cli::admin::{
    AuthArgs, ConfigArgs, DisableArgs, EnableArgs, InferenceArgs, ModelsArgs, RegisterArgs,
    UnregisterArgs,
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

    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Subcommand)]
pub enum Commands {
    Manual(manual::ManualArgs),
    Init(InitArgs),
    Identity(gait_cli::cli::identity::IdentityArgs),
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
    #[cfg(feature = "workflow")]
    Workflow(WorkflowArgs),
    Daemons(DaemonsArgs),
}

pub fn cli() -> clap::Command {
    Cli::command()
}

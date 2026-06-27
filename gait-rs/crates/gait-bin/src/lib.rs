use clap::{ArgAction, CommandFactory, Parser, Subcommand};
use std::path::PathBuf;

use gait_cli::cli::admin::{
    AuthArgs, DisableArgs, EnableArgs, ModelsArgs, RegisterArgs, UnregisterArgs,
};
use gait_cli::cli::daemons::DaemonsArgs;
use gait_cli::cli::init::InitArgs;
use gait_cli::cli::session::{
    AllowArgs, DenyArgs, DiscardArgs, PersistArgs, PsArgs, ReplyArgs, SessionsArgs, StatsArgs,
    StopArgs, UndoArgs, UnlockArgs,
};
use gait_cli::cli::workflow::WorkflowArgs;

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
    /// Model override (`provider/model`)
    #[arg(long, short = 'm')]
    pub model: Option<String>,
    /// Agent file to load
    #[arg(long, short = 'a')]
    pub agent: Option<String>,
    /// Auto-approve all permission requests
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
    #[arg(long, short = 'd', hide = true)]
    pub gait_dir: Option<PathBuf>,
    #[arg(long, short = 'c', hide = true)]
    pub codebase: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Subcommand)]
pub enum Commands {
    Init(InitArgs),
    Sessions(SessionsArgs),
    Ps(PsArgs),
    Stop(StopArgs),
    Persist(PersistArgs),
    Discard(DiscardArgs),
    Undo(UndoArgs),
    Stats(StatsArgs),
    Allow(AllowArgs),
    Deny(DenyArgs),
    Unlock(UnlockArgs),
    Reply(ReplyArgs),
    Register(RegisterArgs),
    Unregister(UnregisterArgs),
    Enable(EnableArgs),
    Disable(DisableArgs),
    Auth(AuthArgs),
    Models(ModelsArgs),
    Workflow(WorkflowArgs),
    Daemons(DaemonsArgs),
}

pub fn cli() -> clap::Command {
    Cli::command()
}

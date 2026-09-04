//! `gait-min`'s CLI surface — a separate, deliberately small `clap` enum
//! rather than `#[cfg]`-hiding dozens of full-only variants on the shared
//! `Cli`/`Commands` in `lib.rs`. Excludes exactly the two genuinely
//! full-only command families: `workflow` (real YAML DAG runs, gated behind
//! `gait-cli`'s `workflow` feature, which is off in `min`) and `local`
//! (local-model management, gated behind `gait-cli`'s `local` feature,
//! likewise off by default in `min`). Everything else — including
//! `register`/`unregister`/`enable`/`disable`/`load`/`unload`, which stay
//! unconditional in `gait-cli` (skill/plugin registration, not workflow- or
//! local-specific) — is present, matching today's full `gait` behavior for
//! those commands.
use clap::{ArgAction, CommandFactory, Parser, Subcommand};
use std::path::PathBuf;

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

use crate::manual;

#[derive(Parser)]
#[command(
    name = "gait-min",
    about = "AI coding agent (minimal build)",
    args_conflicts_with_subcommands = true
)]
pub struct MinCli {
    /// Task to run (bare positional: `gait-min "fix the login bug"`)
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
    #[arg(long, short = 'd', hide = true)]
    pub gait_dir: Option<PathBuf>,
    #[arg(long, short = 'c', hide = true)]
    pub codebase: Option<PathBuf>,
    /// Override the in-tree safety guard (design §7): run even though the
    /// gait-dir is inside the codebase.
    #[arg(long)]
    pub allow_intree: bool,

    #[command(subcommand)]
    pub command: Option<MinCommands>,
}

#[derive(Subcommand)]
pub enum MinCommands {
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
    Inference(InferenceArgs),
    Daemons(DaemonsArgs),
}

pub fn cli() -> clap::Command {
    MinCli::command()
}

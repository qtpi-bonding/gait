pub mod theme;
pub mod git;
pub mod session;
pub mod config;
pub mod auth;
pub mod models;
pub mod turn;
pub mod types;
pub mod provider;
pub mod anthropic;
pub mod openai;
pub mod stream;
pub mod generate;
pub mod factory;
pub mod tools;
pub mod harness;
pub mod scheduler;
pub mod workflow;
pub mod permission;
pub mod render;
pub mod chat;
pub mod shell_history;
pub mod docker;
pub mod agent_frontmatter;
pub mod mcp_client;
pub mod repl_helper;
pub mod hashline;
pub mod prompt;
pub mod profile;
pub mod proto;
pub mod sandbox;
pub mod config_hook;

pub use types::*;
pub use stream::stream_text;
pub use generate::generate_text;
pub use factory::load_provider;
pub use tools::builtin_tools;
pub use turn::{run_turn, TurnResult, TurnInput};
pub use profile::{Profile, ResolvedProfile};
pub use session::Session;
pub use harness::{HarnessBackend, NoopHarnessBackend, NativeHarnessBackend};
pub use scheduler::{Action, Scheduler, NativeScheduler, TurnEnded};
pub use workflow::runner::{run_workflow, WorkflowRunOpts};
/// Default system prompt embedded from `prompts/gait-agent.md`.
/// Always prepended to the composed system prompt; additive with workspace AGENTS.md.
pub const GAIT_AGENT_PROMPT: &str = include_str!("../prompts/gait-agent.md");

/// Git hash of the gait binary at compile time. Used for system prompt attribution.
pub const GAIT_BUILD_GIT_HASH: &str = env!("GAIT_BUILD_GIT_HASH");

pub mod auth;
pub mod config;
pub mod factory;
pub mod git;
pub mod harness;
pub mod models;
pub mod provider;
pub mod scheduler;
pub mod session;
pub mod tools;
pub mod turn;
pub mod types;
pub mod workflow_runner;

pub mod embed;
pub mod port;
pub mod profile;
pub mod proto;

// Deferred — external callers in gait-cli/gait-repl; seal after public wrappers added
pub mod agent_frontmatter;
pub mod expand;
pub mod permission;
pub mod pty;
pub mod shell_history;
pub mod skills;

// No external callers — safe to seal now
pub(crate) mod anthropic;
pub(crate) mod config_hook;
pub(crate) mod hashline;
pub(crate) mod hook_runner;
pub(crate) mod mcp;
pub(crate) mod openai;
pub(crate) mod prompt;
pub mod sandbox;
#[cfg(test)]
mod test_utils;
pub(crate) mod text;
pub(crate) mod wasm_plugins;

// Types — explicit list replaces pub use types::*
pub use types::{
    AssistantContent, AssistantPart, ContentPart, ExecutionMode, ExecutionSignal, Hook, HookAction,
    HookContext, Message, MessageContent, OutputFilter, PermissionRequest, StepFinish, Tool,
    ToolCall, ToolDef, ToolPermission, ToolResult, ToolResultKind, Usage,
};

pub use factory::load_provider;
pub use harness::{HarnessBackend, NativeHarnessBackend, NoopHarnessBackend};
pub use profile::{Profile, ResolvedProfile};
pub use proto::Recorder;
pub use scheduler::{Action, NativeScheduler, Scheduler, TurnEnded};
pub use session::Ingestor;
pub use session::{new_session_id, Session};
pub use tools::builtin_tools;
pub use turn::nerve::NerveWriter;
pub use turn::{TurnInput, TurnResult}; // run_turn removed — internal only // Phase 3 Task 4a: spine needs NerveWriter from gait_cli

/// The built-in harness preamble, embedded from `prompts/gait-harness.md`.
/// Wrapped in the `<harness>` layer of the composed system prompt.
pub const GAIT_HARNESS_PROMPT: &str = include_str!("../prompts/gait-harness.md");

/// Git hash of the gait binary at compile time. Used for system prompt attribution.
pub const GAIT_BUILD_GIT_HASH: &str = env!("GAIT_BUILD_GIT_HASH");

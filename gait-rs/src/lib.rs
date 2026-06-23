pub mod auth;
pub mod config;
pub mod factory;
pub mod git;
pub mod harness;
pub mod models;
pub mod process;
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
pub use harness::{HarnessBackend, NativeHarnessBackend, NoopHarnessBackend, NotifyPayload};
pub use profile::{Profile, ResolvedProfile};
pub use proto::Recorder;
pub use scheduler::{Action, NativeScheduler, Scheduler, TurnEnded};
pub use session::Ingestor;
pub use session::{new_session_id, Session};
pub use tools::builtin_tools;

/// The built-in harness preamble, embedded from `prompts/gait-harness.md`.
/// Wrapped in the `<harness>` layer of the composed system prompt.
pub const GAIT_HARNESS_PROMPT: &str = include_str!("../prompts/gait-harness.md");

/// Git hash of the gait binary at compile time. Used for system prompt attribution.
pub const GAIT_BUILD_GIT_HASH: &str = env!("GAIT_BUILD_GIT_HASH");

// ── Role dispatch (gait-bin-only by convention) ───────────────────────────────

/// Returns true when `argv[0]` is a hidden role subcommand (`__brain` or `__run-tool`).
/// Pure function — testable without spawning a real provider.
pub fn is_role_argv(argv: &[String]) -> bool {
    matches!(
        argv.first().map(|s| s.as_str()),
        Some("__brain") | Some("__run-tool")
    )
}

/// Hidden entry point that runs the appropriate role for the given argv.
/// `runner` is injected by the caller (gait-cli) because gait-core cannot construct
/// `GaitWorkflowRunner` (that crate is downstream — constructing it here inverts the DAG).
///
/// # Panics
/// Exits the process on unrecognised role argv (should not happen if `is_role_argv` gates).
#[doc(hidden)]
pub async fn __dispatch_role(
    argv: &[String],
    runner: Option<std::sync::Arc<dyn workflow_runner::WorkflowRunner>>,
) {
    use clap::Parser as _;
    use turn::brain::{run_brain, BrainConfig};
    use turn::hand::{run_hand, HandConfig};

    match argv.first().map(|s| s.as_str()) {
        Some("__brain") => {
            // Resolve gait_dir and codebase from argv.
            fn extract(args: &[String], flag: &str) -> Option<std::path::PathBuf> {
                args.windows(2)
                    .find(|w| w[0] == flag)
                    .map(|w| std::path::PathBuf::from(&w[1]))
            }
            let gait_dir = extract(argv, "--gait-dir").unwrap_or_else(|| {
                std::env::var("GAIT_DIR")
                    .ok()
                    .filter(|v| !v.is_empty())
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(|| {
                        eprintln!("[gait] no gait dir: pass --gait-dir <path> or set $GAIT_DIR");
                        std::process::exit(1);
                    })
            });
            let codebase = extract(argv, "--codebase").unwrap_or_else(|| {
                std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
            });

            #[derive(clap::Parser)]
            #[command(no_binary_name = true)]
            struct BrainArgs {
                #[arg(long = "session")]
                session_id: Option<String>,
                #[arg(long)]
                auto: bool,
                #[arg(long = "model")]
                model_override: Option<String>,
                #[arg(long = "agent")]
                agent_name: Option<String>,
                #[arg(long, action = clap::ArgAction::Append)]
                allow: Vec<String>,
                #[arg(long, action = clap::ArgAction::Append)]
                deny: Vec<String>,
                #[arg(long)]
                max_iter: Option<usize>,
                #[arg(long)]
                sandbox: bool,
                #[arg(long = "no-sandbox")]
                no_sandbox: bool,
                #[arg(long = "sandbox-locked", hide = true)]
                sandbox_locked: bool,
                #[arg(long, hide = true)]
                gait_dir: Option<std::path::PathBuf>,
                #[arg(long, hide = true)]
                codebase: Option<std::path::PathBuf>,
                #[arg(long, hide = true)]
                task: Option<String>,
                #[arg(long = "result-schema", hide = true)]
                result_schema_path: Option<String>,
                #[arg(long = "cascade-group", hide = true)]
                cascade_group: Option<String>,
            }

            let args = BrainArgs::parse_from(&argv[1..]);
            let api_key = std::env::var("GAIT_BRAIN_API_KEY").ok();
            let config = BrainConfig {
                session_id: args.session_id,
                auto: args.auto,
                allow: args.allow,
                deny: args.deny,
                max_iter: args.max_iter,
                sandbox_locked: args.sandbox_locked,
                no_sandbox: args.no_sandbox,
                sandbox: args.sandbox,
            };
            run_brain(config, gait_dir, codebase, api_key, runner).await;
        }
        Some("__run-tool") => {
            #[derive(clap::Parser)]
            #[command(no_binary_name = true)]
            struct RunToolArgs {
                #[arg(long)]
                gait_dir: std::path::PathBuf,
                #[arg(long)]
                codebase: std::path::PathBuf,
                #[arg(long)]
                session: String,
                #[arg(long)]
                tool: String,
                #[arg(long, default_value = "true")]
                seal: String,
            }
            let a = RunToolArgs::parse_from(&argv[1..]);
            let config = HandConfig {
                gait_dir: a.gait_dir,
                codebase: a.codebase,
                session: a.session,
                tool: a.tool,
                seal: a.seal != "false",
            };
            run_hand(config).await;
        }
        _ => {
            eprintln!(
                "[gait] __dispatch_role: unrecognised argv[0]: {:?}",
                argv.first()
            );
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod role_dispatch_tests {
    use super::*;

    #[test]
    fn dispatch_role_recognizes_hidden_subcommands() {
        assert!(is_role_argv(&["__brain".into()]));
        assert!(is_role_argv(&["__run-tool".into()])); // the hand subcommand
        assert!(!is_role_argv(&["run".into()]));
    }
}

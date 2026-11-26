//! Graph-based workflow orchestrator for gait.

pub use gait_core::embed::{TurnDriver, TurnSink, TurnSource};

pub mod exec_branch;
pub mod exec_gate;
pub mod exec_prompt;
pub mod exec_shell;
pub mod hooks;
pub mod host;
pub mod launch;
pub mod runner;
pub mod schema;
pub mod state;
pub mod validate;
pub mod vars;

pub mod convert;
pub mod observer;
pub mod output;
pub mod record;
pub mod run_log;
pub mod script;
pub mod script_turns;
pub mod step;
pub mod turn_exec;
pub mod types;

pub mod status;

pub use runner::{run_workflow, WorkflowOutcome, WorkflowRunOpts};

pub use script::{run_script, ScriptRunOpts};

pub use status::workflow_status_block;

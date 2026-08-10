//! Graph-based workflow orchestrator for gait.

pub use gait_core::embed::{TurnDriver, TurnSink, TurnSource};

pub mod exec_branch;
pub mod exec_gate;
pub mod exec_prompt;
pub mod exec_shell;
pub mod hooks;
pub mod runner;
pub mod schema;
pub mod state;
pub mod validate;
pub mod vars;

pub mod convert;
pub mod observer;
pub mod output;
pub mod run_log;

pub mod single_prompt;
pub mod status;

pub use runner::{run_workflow, WorkflowOutcome, WorkflowRunOpts};
pub use single_prompt::{single_prompt_run, ProfileOverride, SinglePromptOpts};

pub use status::workflow_status_block;

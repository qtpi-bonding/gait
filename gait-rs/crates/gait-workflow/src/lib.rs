//! Graph-based workflow orchestrator for gait.

pub use gait_core::embed::{TurnDriver, TurnSink, TurnSource};

pub mod exec_branch;
pub mod exec_gate;
pub mod exec_prompt;
pub mod exec_shell;
pub mod hooks;
pub mod progress;
pub mod runner;
pub mod schema;
pub mod state;
pub mod vars;

pub mod convert;
pub mod observer;
pub mod run_log;

pub mod status;

pub mod runner_impl;

pub use runner::{run_workflow, WorkflowOutcome, WorkflowRunOpts};

pub use status::workflow_status_block;

pub use runner_impl::GaitWorkflowRunner;

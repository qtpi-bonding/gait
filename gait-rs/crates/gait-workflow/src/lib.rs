//! Script workflow engine for gait.

pub use gait_core::embed::{TurnDriver, TurnSink, TurnSource};

pub mod exec_shell;
pub mod hooks;
pub mod host;
pub mod launch;
pub mod state;

pub mod capturing_sink;
pub mod observer;
pub mod outcome;
pub mod output;
pub mod record;
pub mod run_log;
pub mod script;
pub mod script_turns;
pub mod step;
pub mod turn_exec;
pub mod types;

pub mod status;

pub use outcome::WorkflowOutcome;

pub use script::{run_script, ScriptRunOpts};

pub use status::workflow_status_block;

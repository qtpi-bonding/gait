//! Outcomes of a workflow engine run.

/// Outcome of a completed workflow engine run.
#[derive(Debug, PartialEq)]
pub enum WorkflowOutcome {
    /// All work completed successfully.
    Completed,
    /// Workflow paused at a gate node, awaiting external input to resume.
    GatePaused,
}

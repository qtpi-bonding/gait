//! Outcomes of a workflow engine run.

/// Outcome of a completed workflow engine run.
#[derive(Debug, PartialEq)]
pub enum WorkflowOutcome {
    /// All work completed successfully.
    Completed,
    /// Workflow paused at a gate node, awaiting external input to resume.
    GatePaused,
    /// Workflow paused at an open (absent-prompt) node awaiting interactive input.
    /// The node was started (`NodeStart` recorded) but not completed. Resume with
    /// an input source to continue from this point.
    NodeOpen,
}

use crate::workflow_runner::WorkflowRunner;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Clone, Debug)]
pub struct ToolCtx {
    pub codebase_path: PathBuf,
    pub gait_dir: PathBuf,
    pub session_id: String,
    /// When `Some`, `builtin_tools()` includes `RunWorkflowTool`.
    pub workflow_runner: Option<Arc<dyn WorkflowRunner>>,
}

impl ToolCtx {
    pub fn new(codebase_path: PathBuf, gait_dir: PathBuf, session_id: String) -> Self {
        Self {
            codebase_path,
            gait_dir,
            session_id,
            workflow_runner: None,
        }
    }

    /// Attach a `WorkflowRunner` so `builtin_tools()` includes `RunWorkflowTool`.
    pub fn with_workflow_runner(mut self, runner: Arc<dyn WorkflowRunner>) -> Self {
        self.workflow_runner = Some(runner);
        self
    }
}

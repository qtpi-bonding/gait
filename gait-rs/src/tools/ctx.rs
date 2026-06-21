use crate::workflow_runner::WorkflowRunner;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Clone, Debug)]
pub struct ToolCtx {
    pub codebase_path: PathBuf,
    pub gait_dir: PathBuf,
    pub session_id: String,
    pub workflow_runner: Option<Arc<dyn WorkflowRunner>>,
    /// When `Some`, webfetch only allows requests to these hosts.
    /// `None` means no app-level host restriction (kernel seal still applies).
    /// Set by the hand from `Impulse.requested.network` when mode is `Hosts`.
    pub network_allowlist: Option<Vec<String>>,
}

impl ToolCtx {
    pub fn new(codebase_path: PathBuf, gait_dir: PathBuf, session_id: String) -> Self {
        Self {
            codebase_path,
            gait_dir,
            session_id,
            workflow_runner: None,
            network_allowlist: None,
        }
    }

    pub fn with_workflow_runner(mut self, runner: Arc<dyn WorkflowRunner>) -> Self {
        self.workflow_runner = Some(runner);
        self
    }

    pub fn with_network_allowlist(mut self, hosts: Vec<String>) -> Self {
        self.network_allowlist = Some(hosts);
        self
    }
}

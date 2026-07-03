use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct ToolCtx {
    pub codebase_path: PathBuf,
    pub gait_dir: PathBuf,
    pub session_id: String,
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
            network_allowlist: None,
        }
    }

    pub fn with_network_allowlist(mut self, hosts: Vec<String>) -> Self {
        self.network_allowlist = Some(hosts);
        self
    }
}

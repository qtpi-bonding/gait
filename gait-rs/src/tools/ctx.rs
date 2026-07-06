use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use crate::tools::fs_backend::{FsBackend, LocalFs};

#[derive(Clone)]
pub struct ToolCtx {
    pub codebase_path: PathBuf,
    pub gait_dir: PathBuf,
    pub session_id: String,
    /// When `Some`, webfetch only allows requests to these hosts.
    /// `None` means no app-level host restriction (kernel seal still applies).
    /// Set by the hand from `Impulse.requested.network` when mode is `Hosts`.
    pub network_allowlist: Option<Vec<String>>,
    /// Swappable filesystem backend. Defaults to [`LocalFs`] (a direct
    /// pass-through to `std::fs`); a future `AcpFs` implementation can be
    /// swapped in via [`ToolCtx::with_fs`] without touching any tool body.
    pub fs: Arc<dyn FsBackend>,
}

// Manual Debug so the trait-object `fs` field (which doesn't impl Debug) is
// rendered as a stable placeholder rather than failing the derive.
impl fmt::Debug for ToolCtx {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ToolCtx")
            .field("codebase_path", &self.codebase_path)
            .field("gait_dir", &self.gait_dir)
            .field("session_id", &self.session_id)
            .field("network_allowlist", &self.network_allowlist)
            .field("fs", &"<dyn FsBackend>")
            .finish()
    }
}

impl ToolCtx {
    pub fn new(codebase_path: PathBuf, gait_dir: PathBuf, session_id: String) -> Self {
        Self {
            codebase_path,
            gait_dir,
            session_id,
            network_allowlist: None,
            fs: Arc::new(LocalFs),
        }
    }

    pub fn with_network_allowlist(mut self, hosts: Vec<String>) -> Self {
        self.network_allowlist = Some(hosts);
        self
    }

    /// Replace the filesystem backend. Used by tests that need a fake
    /// backend and, later, by the spine when wiring up an ACP client.
    pub fn with_fs(mut self, fs: Arc<dyn FsBackend>) -> Self {
        self.fs = fs;
        self
    }
}

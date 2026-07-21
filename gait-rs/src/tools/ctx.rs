use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use crate::sandbox::SandboxScope;
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
    /// When `Some`, the kernel sandbox scope the hand is about to seal (or has
    /// sealed) this process with. Lets tools reject an out-of-scope path
    /// up front instead of committing to it and finding out only when the
    /// sandboxed subprocess later fails. `None` means no scope was supplied
    /// (e.g. most unit tests) — callers should skip the containment check.
    pub scope: Option<SandboxScope>,
    /// Filesystem backend: [`LocalFs`] (disk, default) or [`BufferedFs`]
    /// (spine-supplied buffer, set by the hand for delegated ops).
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
            .field("scope", &self.scope)
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
            scope: None,
            fs: Arc::new(LocalFs),
        }
    }

    pub fn with_network_allowlist(mut self, hosts: Vec<String>) -> Self {
        self.network_allowlist = Some(hosts);
        self
    }

    pub fn with_scope(mut self, scope: SandboxScope) -> Self {
        self.scope = Some(scope);
        self
    }
}

pub mod codebase;
pub mod workspace;
pub mod rewind;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use crate::git::codebase::{CodebaseRepo, GitCodebaseRepo};
use crate::git::workspace::{WorkspaceRepo, GitWorkspaceRepo};
use crate::git::rewind::{RewindRepo, GitRewindRepo};

pub(crate) fn with_gait_trailer(message: &str) -> String {
    format!("{}\n\nCo-Authored-By: gait <gait@localhost>", message)
}

#[allow(dead_code)]
pub(crate) fn signature_or_default(repo: &git2::Repository) -> anyhow::Result<git2::Signature<'static>> {
    match repo.signature() {
        Ok(sig) => Ok(sig.to_owned()),
        Err(_) => Ok(git2::Signature::now("gait", "gait@localhost")?),
    }
}

#[allow(dead_code)]
pub(crate) fn rewind_git_dir(workspace: &Path, session_id: &str) -> PathBuf {
    workspace.join("sessions").join(session_id).join("codebase.git")
}

/// Bundle of the three git-context repos, injected from composition roots.
/// `run_turn` and tools receive a `Repos`; entry points build `Repos::production()`.
#[derive(Clone)]
pub struct Repos {
    pub codebase: Arc<dyn CodebaseRepo>,
    pub workspace: Arc<dyn WorkspaceRepo>,
    pub rewind: Arc<dyn RewindRepo>,
}

impl Repos {
    /// The real git-backed implementations used in production.
    pub fn production() -> Self {
        Repos {
            codebase: Arc::new(GitCodebaseRepo),
            workspace: Arc::new(GitWorkspaceRepo),
            rewind: Arc::new(GitRewindRepo),
        }
    }
}



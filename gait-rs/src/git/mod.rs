pub mod codebase;
pub mod session;
pub mod checkpoint;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use crate::git::codebase::{CodebaseRepo, GitCodebaseRepo};
use crate::git::session::{SessionRepo, GitSessionRepo};
use crate::git::checkpoint::{CheckpointRepo, GitCheckpointRepo};

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
pub(crate) fn checkpoint_git_dir(workspace: &Path, session_id: &str) -> PathBuf {
    workspace.join("sessions").join(session_id).join("codebase.git")
}

/// Bundle of the three git-context repos, injected from composition roots.
/// `run_turn` and tools receive a `Repos`; entry points build `Repos::production()`.
#[derive(Clone)]
pub struct Repos {
    pub codebase: Arc<dyn CodebaseRepo>,
    pub workspace: Arc<dyn SessionRepo>,
    pub checkpoints: Arc<dyn CheckpointRepo>,
}

impl Repos {
    /// The real git-backed implementations used in production.
    pub fn production() -> Self {
        Repos {
            codebase: Arc::new(GitCodebaseRepo),
            workspace: Arc::new(GitSessionRepo),
            checkpoints: Arc::new(GitCheckpointRepo),
        }
    }
}

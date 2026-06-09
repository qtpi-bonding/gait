pub mod codebase;
pub mod workspace;
pub mod rewind;

use std::path::{Path, PathBuf};

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



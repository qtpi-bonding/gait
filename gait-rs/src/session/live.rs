//! Paths for a session that is still *live* (its worktree mounted, not yet
//! persisted into `main`). The worktree mounts at `gait_dir/.active/<id>/`
//! rather than `gait_dir/sessions/<id>/` — the latter is reserved for the
//! *persisted* shape a session's branch merges into `main` as (see
//! `crate::session::log::log_path`, which is unchanged and means exactly
//! that persisted location).
use std::path::{Path, PathBuf};

/// The live worktree's mount point: `gait_dir/.active/<session_id>/`.
pub fn worktree_dir(gait_dir: &Path, session_id: &str) -> PathBuf {
    gait_dir.join(".active").join(session_id)
}

/// The live session's log, nested one level inside the worktree to match the
/// shape its branch's tree will have on `main` after persist:
/// `gait_dir/.active/<id>/sessions/<id>/log.jsonl`.
pub fn log_path(gait_dir: &Path, session_id: &str) -> PathBuf {
    worktree_dir(gait_dir, session_id)
        .join("sessions")
        .join(session_id)
        .join("log.jsonl")
}

/// Same shape as `log_path`, for `diagnostic.jsonl` (only present when
/// `GAIT_DEBUG` was set for the session).
pub fn diagnostic_path(gait_dir: &Path, session_id: &str) -> PathBuf {
    worktree_dir(gait_dir, session_id)
        .join("sessions")
        .join(session_id)
        .join("diagnostic.jsonl")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worktree_dir_is_dot_active_not_sessions() {
        let gait_dir = Path::new("/tmp/gd");
        assert_eq!(
            worktree_dir(gait_dir, "sabc123"),
            Path::new("/tmp/gd/.active/sabc123")
        );
    }

    #[test]
    fn log_path_nests_sessions_id_under_the_active_mount() {
        let gait_dir = Path::new("/tmp/gd");
        assert_eq!(
            log_path(gait_dir, "sabc123"),
            Path::new("/tmp/gd/.active/sabc123/sessions/sabc123/log.jsonl")
        );
    }

    #[test]
    fn diagnostic_path_mirrors_log_path_shape() {
        let gait_dir = Path::new("/tmp/gd");
        assert_eq!(
            diagnostic_path(gait_dir, "sabc123"),
            Path::new("/tmp/gd/.active/sabc123/sessions/sabc123/diagnostic.jsonl")
        );
    }
}

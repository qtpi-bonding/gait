//! `SessionGit` — session-scoped git persistence for one session.
//!
//! Holds only the `traces` + `checkpoints` Arcs (NOT the codebase repo), so:
//! 1. the codebase repo is structurally unreachable from here — no accidental
//!    mixing of "inspect the user's repo" with "persist gait's session";
//! 2. the `session_id` is bound at construction — a cross-session write (Notify)
//!    must name its target to build the object, so it can't happen by accident.
//!
//! Codebase reads (`head_hash`/`dirty_files`) stay on `repos.codebase.*` at
//! their call sites — they are introspection, not session persistence.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::git::{checkpoint::CheckpointRepo, trace::TraceRepo};

pub(crate) enum SessionGit {
    /// Production: session-scoped trace + checkpoint persistence.
    Persistent {
        gait_dir: PathBuf,
        session_id: String,
        traces: Arc<dyn TraceRepo>,
        checkpoints: Arc<dyn CheckpointRepo>,
    },
    /// Ephemeral one-off: every git side-effect is a no-op.
    Null,
}

impl SessionGit {
    pub(crate) fn for_session(
        repos: &crate::git::Repos,
        gait_dir: &Path,
        session_id: &str,
    ) -> Self {
        Self::Persistent {
            gait_dir: gait_dir.to_path_buf(),
            session_id: session_id.to_string(),
            traces: Arc::clone(&repos.traces),
            checkpoints: Arc::clone(&repos.checkpoints),
        }
    }

    pub(crate) fn null() -> Self {
        Self::Null
    }

    pub(crate) fn ensure_worktree(&self) -> anyhow::Result<()> {
        match self {
            Self::Persistent {
                traces,
                gait_dir,
                session_id,
                ..
            } => traces.create_session_worktree(gait_dir, session_id),
            Self::Null => Ok(()),
        }
    }

    pub(crate) fn checkpoint_init(&self, codebase: &Path) -> anyhow::Result<()> {
        match self {
            Self::Persistent {
                checkpoints,
                gait_dir,
                session_id,
                ..
            } => checkpoints.init(gait_dir, session_id, codebase),
            Self::Null => Ok(()),
        }
    }

    pub(crate) fn checkpoint(&self, codebase: &Path, turn: u32) -> anyhow::Result<String> {
        match self {
            Self::Persistent {
                checkpoints,
                gait_dir,
                session_id,
                ..
            } => checkpoints.checkpoint(gait_dir, session_id, codebase, turn),
            Self::Null => Ok(String::new()),
        }
    }

    pub(crate) fn diff_between(
        &self,
        baseline: &str,
        final_checkpoint: &str,
    ) -> anyhow::Result<Option<crate::proto::gait::log::v1::ChangeSummary>> {
        match self {
            Self::Persistent {
                checkpoints,
                gait_dir,
                session_id,
                ..
            } => checkpoints.diff_between(gait_dir, session_id, baseline, final_checkpoint),
            Self::Null => Ok(None),
        }
    }

    pub(crate) fn commit(&self, message: &str) -> anyhow::Result<()> {
        match self {
            Self::Persistent {
                traces,
                gait_dir,
                session_id,
                ..
            } => traces.commit_session(gait_dir, session_id, message),
            Self::Null => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::{checkpoint::CheckpointRepo, codebase::CodebaseRepo, trace::TraceRepo, Repos};
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct Rec(Arc<Mutex<Vec<String>>>);

    impl TraceRepo for Rec {
        fn init_workspace(&self, _: &Path) -> anyhow::Result<()> {
            Ok(())
        }
        fn create_session_worktree(&self, _: &Path, s: &str) -> anyhow::Result<()> {
            self.0.lock().unwrap().push(format!("worktree:{s}"));
            Ok(())
        }
        fn commit_session(&self, _: &Path, s: &str, _: &str) -> anyhow::Result<()> {
            self.0.lock().unwrap().push(format!("commit:{s}"));
            Ok(())
        }
        fn list_active_sessions(&self, _: &Path) -> anyhow::Result<Vec<String>> {
            Ok(vec![])
        }
        fn finish_session(&self, _: &Path, _: &str) -> anyhow::Result<()> {
            Ok(())
        }
        fn create_branch_worktree(
            &self,
            _: &Path,
            _: &str,
            _: &str,
            _: &Path,
        ) -> anyhow::Result<()> {
            unimplemented!()
        }
        fn remove_branch_worktree(&self, _: &Path, _: &Path) -> anyhow::Result<()> {
            unimplemented!()
        }
        fn merge_branch_ff(&self, _: &Path, _: &str, _: &str) -> anyhow::Result<()> {
            unimplemented!()
        }
        fn discard_session(&self, _gait_dir: &Path, _session_id: &str) -> anyhow::Result<()> {
            unimplemented!()
        }
    }

    impl CheckpointRepo for Rec {
        fn init(&self, _: &Path, s: &str, _: &Path) -> anyhow::Result<()> {
            self.0.lock().unwrap().push(format!("init:{s}"));
            Ok(())
        }
        fn checkpoint(&self, _: &Path, s: &str, _: &Path, t: u32) -> anyhow::Result<String> {
            self.0.lock().unwrap().push(format!("ckpt:{s}:{t}"));
            Ok(String::new())
        }
        fn restore_to(&self, _: &Path, _: &str, _: &Path, _: &str) -> anyhow::Result<()> {
            unimplemented!()
        }
        fn remove(&self, _: &Path, _: &str) -> anyhow::Result<()> {
            unimplemented!()
        }
        fn diff_between(
            &self,
            _: &Path,
            _: &str,
            _: &str,
            _: &str,
        ) -> anyhow::Result<Option<crate::proto::gait::log::v1::ChangeSummary>> {
            unimplemented!()
        }
    }

    struct NoCb;
    impl CodebaseRepo for NoCb {
        fn head_hash(&self, _: &Path) -> Option<String> {
            None
        }
        fn dirty_files(&self, _: &Path) -> Vec<String> {
            vec![]
        }
    }

    #[test]
    fn binds_session_id_and_routes_to_trace_and_checkpoint() {
        let rec = Rec::default();
        let repos = Repos {
            codebase: Arc::new(NoCb),
            traces: Arc::new(rec.clone()),
            checkpoints: Arc::new(rec.clone()),
        };
        let g = std::path::Path::new("/tmp/gd");
        let cb = std::path::Path::new("/tmp/cb");
        let git = SessionGit::for_session(&repos, g, "sess");
        git.ensure_worktree().unwrap();
        git.checkpoint_init(cb).unwrap();
        git.checkpoint(cb, 3).unwrap();
        git.commit("msg").unwrap();
        let calls = rec.0.lock().unwrap().clone();
        assert_eq!(
            calls,
            vec!["worktree:sess", "init:sess", "ckpt:sess:3", "commit:sess"]
        );
    }

    #[test]
    fn null_variant_no_ops_every_git_side_effect() {
        let git = SessionGit::null();
        git.ensure_worktree().unwrap();
        git.checkpoint_init(std::path::Path::new("/tmp/cb"))
            .unwrap();
        assert_eq!(
            git.checkpoint(std::path::Path::new("/tmp/cb"), 1).unwrap(),
            ""
        );
        git.commit("msg").unwrap();
        // No repos, no Arcs — nothing was touched.
        assert!(matches!(git, SessionGit::Null));
    }
}

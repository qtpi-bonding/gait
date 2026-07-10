//! The adapter's only state: an in-memory table of live sessions, keyed by the
//! resolved (gait_dir, session_id). Not authoritative — the on-disk log is.
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Discover a gait-dir from an ACP `cwd` by pure walk-up. `$GAIT_DIR` is
/// intentionally NOT consulted (that is `resolve`'s job for the single-cwd CLI):
/// the adapter is a multi-cwd process, so each session resolves from its own cwd.
pub fn discover_gait_dir_for_cwd(cwd: &Path) -> anyhow::Result<PathBuf> {
    gait_core::gait_dir::discover(cwd)
}

type SessionKey = (PathBuf, String); // (gait_dir, session_id)

/// Per-session bookkeeping. Empty in Phase 1; Phase 2 adds the in-flight
/// `CascadeKillHandle` used by `session/cancel`.
#[derive(Default)]
pub struct SessionEntry {}

#[derive(Clone, Default)]
pub struct SessionMap(Arc<Mutex<HashMap<SessionKey, SessionEntry>>>);

impl SessionMap {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&self, gait_dir: PathBuf, session_id: String) {
        self.0
            .lock()
            .unwrap()
            .insert((gait_dir, session_id), SessionEntry::default());
    }

    pub fn contains(&self, gait_dir: &Path, session_id: &str) -> bool {
        self.0
            .lock()
            .unwrap()
            .contains_key(&(gait_dir.to_path_buf(), session_id.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_then_contains() {
        let m = SessionMap::new();
        m.insert(PathBuf::from("/g/.gait"), "sess-1".into());
        assert!(m.contains(Path::new("/g/.gait"), "sess-1"));
        assert!(!m.contains(Path::new("/other/.gait"), "sess-1"));
    }

    #[test]
    fn same_session_id_different_gait_dirs_do_not_collide() {
        let m = SessionMap::new();
        m.insert(PathBuf::from("/a/.gait"), "sess".into());
        assert!(!m.contains(Path::new("/b/.gait"), "sess"));
    }

    #[test]
    fn resolve_follows_gaitref_pointer() {
        // The shipped convention: `.gaitref` is the pointer FILE (not `.gait`).
        let t = tempfile::TempDir::new().unwrap();
        let gait_dir = t.path().join("home");
        std::fs::write(
            t.path().join(".gaitref"),
            format!("gaitdir: {}\n", gait_dir.display()),
        )
        .unwrap();
        let got = discover_gait_dir_for_cwd(t.path()).unwrap();
        assert_eq!(got, gait_dir);
    }

    #[test]
    fn resolve_finds_dot_gait_directory() {
        // `.gait/` is a directory in the shipped convention.
        let t = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(t.path().join(".gait")).unwrap();
        assert_eq!(
            discover_gait_dir_for_cwd(t.path()).unwrap(),
            t.path().join(".gait")
        );
    }

    #[test]
    fn resolve_errors_when_no_workspace_above() {
        let t = tempfile::TempDir::new().unwrap(); // no .gait/ or .gaitref anywhere
        assert!(discover_gait_dir_for_cwd(t.path()).is_err());
    }

    #[test]
    fn map_is_send_sync_clone() {
        fn assert_bounds<T: Send + Sync + Clone>() {}
        assert_bounds::<SessionMap>();
    }
}

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

/// Per-session bookkeeping: the resolved workspace + the in-flight turn's kill
/// handle and joinable thread (present only while a turn runs; the cascade is
/// taken on completion/cancel, the join handle drained at connection teardown).
#[derive(Default)]
pub struct SessionEntry {
    pub gait_dir: PathBuf,
    pub codebase: PathBuf,
    pub cascade: Option<gait_core::embed::CascadeKillHandle>,
    pub join: Option<std::thread::JoinHandle<()>>,
}

#[derive(Clone, Default)]
pub struct SessionMap(Arc<Mutex<HashMap<String, SessionEntry>>>);

impl SessionMap {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a freshly-minted session. session-ids are globally unique, so the
    /// single-key index is unambiguous; `contains` still checks the (gait_dir,
    /// session_id) pair for the Phase-1 collision test.
    pub fn insert(&self, gait_dir: PathBuf, session_id: String, codebase: PathBuf) {
        self.0.lock().unwrap().insert(
            session_id,
            SessionEntry {
                gait_dir,
                codebase,
                cascade: None,
                join: None,
            },
        );
    }

    /// `(gait_dir, codebase)` for a known session, else `None`.
    pub fn resolve(&self, session_id: &str) -> Option<(PathBuf, PathBuf)> {
        self.0
            .lock()
            .unwrap()
            .get(session_id)
            .map(|e| (e.gait_dir.clone(), e.codebase.clone()))
    }

    pub fn set_cascade(&self, session_id: &str, handle: gait_core::embed::CascadeKillHandle) {
        if let Some(e) = self.0.lock().unwrap().get_mut(session_id) {
            e.cascade = Some(handle);
        }
    }

    pub fn take_cascade(&self, session_id: &str) -> Option<gait_core::embed::CascadeKillHandle> {
        self.0
            .lock()
            .unwrap()
            .get_mut(session_id)
            .and_then(|e| e.cascade.take())
    }

    /// Store the turn's joinable thread so `drain` can reap it on teardown.
    pub fn set_turn_thread(&self, session_id: &str, join: std::thread::JoinHandle<()>) {
        if let Some(e) = self.0.lock().unwrap().get_mut(session_id) {
            e.join = Some(join);
        }
    }

    /// Connection-teardown drain (design §4a): kill every in-flight brain and
    /// join its thread. Takes the handles out from under the lock FIRST, then
    /// kills+joins with the lock released — the turn threads themselves call
    /// `take_cascade`, so holding the lock across `join()` would deadlock.
    pub fn drain(&self) {
        let mut pending = Vec::new();
        {
            let mut g = self.0.lock().unwrap();
            for e in g.values_mut() {
                pending.push((e.cascade.take(), e.join.take()));
            }
        }
        for (cascade, join) in pending {
            if let Some(c) = cascade {
                c.kill();
            }
            if let Some(j) = join {
                let _ = j.join();
            }
        }
    }

    pub fn contains(&self, gait_dir: &Path, session_id: &str) -> bool {
        self.0
            .lock()
            .unwrap()
            .get(session_id)
            .is_some_and(|e| e.gait_dir == gait_dir)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_then_contains() {
        let m = SessionMap::new();
        m.insert(
            PathBuf::from("/g/.gait"),
            "sess-1".into(),
            PathBuf::from("/g"),
        );
        assert!(m.contains(Path::new("/g/.gait"), "sess-1"));
        assert!(!m.contains(Path::new("/other/.gait"), "sess-1"));
    }

    #[test]
    fn same_session_id_different_gait_dirs_do_not_collide() {
        let m = SessionMap::new();
        m.insert(
            PathBuf::from("/a/.gait"),
            "sess".into(),
            PathBuf::from("/a"),
        );
        assert!(!m.contains(Path::new("/b/.gait"), "sess"));
    }

    #[test]
    fn resolve_returns_gait_dir_and_codebase() {
        let m = SessionMap::new();
        m.insert(
            PathBuf::from("/g/.gait"),
            "sess-1".into(),
            PathBuf::from("/code"),
        );
        assert_eq!(
            m.resolve("sess-1"),
            Some((PathBuf::from("/g/.gait"), PathBuf::from("/code")))
        );
        assert_eq!(m.resolve("missing"), None);
    }

    #[test]
    fn set_then_take_cascade_is_one_shot() {
        let m = SessionMap::new();
        m.insert(PathBuf::from("/g/.gait"), "s".into(), PathBuf::from("/c"));
        m.set_cascade("s", gait_core::embed::CascadeKillHandle::new());
        assert!(m.take_cascade("s").is_some());
        assert!(
            m.take_cascade("s").is_none(),
            "cascade is taken exactly once"
        );
    }

    #[test]
    fn take_cascade_unknown_session_is_none() {
        let m = SessionMap::new();
        assert!(m.take_cascade("nope").is_none());
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

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
    /// Per-turn cancel signal, replaced by `set_cancel` each prompt. A
    /// `CancellationToken` cannot be un-cancelled, so it MUST be per-turn, not
    /// per-session, or a cancelled turn would poison later prompts.
    pub cancel: tokio_util::sync::CancellationToken,
    pub join: Option<std::thread::JoinHandle<()>>,
    /// `gait/idempotency_key` values already seen for this session's whole
    /// lifetime (not just the current turn) — see `record_idempotency_key`.
    pub seen_idempotency_keys: std::collections::HashSet<String>,
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
                cancel: tokio_util::sync::CancellationToken::new(),
                join: None,
                seen_idempotency_keys: std::collections::HashSet::new(),
            },
        );
    }

    /// Attach a persisted session after its caller has validated the durable
    /// identity. Returns whether a new entry was inserted. Re-attaching the
    /// same idle/completed session is harmless; a conflicting or still-running
    /// entry is rejected instead of dropping its turn coordination state.
    pub fn attach_existing(
        &self,
        gait_dir: PathBuf,
        session_id: String,
        codebase: PathBuf,
    ) -> Result<bool, String> {
        let mut sessions = self.0.lock().unwrap();
        let Some(entry) = sessions.get(&session_id) else {
            sessions.insert(
                session_id,
                SessionEntry {
                    gait_dir,
                    codebase,
                    cascade: None,
                    cancel: tokio_util::sync::CancellationToken::new(),
                    join: None,
                    seen_idempotency_keys: std::collections::HashSet::new(),
                },
            );
            return Ok(true);
        };

        if entry.gait_dir != gait_dir || entry.codebase != codebase {
            return Err(format!(
                "session is already attached to another workspace: {session_id}"
            ));
        }
        if entry.join.as_ref().is_some_and(|join| !join.is_finished()) {
            return Err(format!(
                "session already has an in-flight turn: {session_id}"
            ));
        }
        Ok(false)
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

    /// Mint-and-store the per-turn cancel token (mirrors `set_cascade`). Called
    /// at the start of each prompt so a prior turn's tripped token is discarded.
    pub fn set_cancel(&self, session_id: &str, token: tokio_util::sync::CancellationToken) {
        if let Some(e) = self.0.lock().unwrap().get_mut(session_id) {
            e.cancel = token;
        }
    }

    /// Fire both cancel signals for an in-flight turn: trip the token (unblocks a
    /// pending permission await) and `kill()` the cascade (SIGTERM a running
    /// brain). Peeks — does NOT take the cascade, which the turn thread's own
    /// completion path still calls `take_cascade` on. No-op on unknown session.
    pub fn cancel(&self, session_id: &str) {
        let guard = self.0.lock().unwrap();
        if let Some(entry) = guard.get(session_id) {
            entry.cancel.cancel();
            if let Some(c) = &entry.cascade {
                c.kill();
            }
        }
    }

    /// Did the current turn get cancelled? Read by the completion path to force
    /// `StopReason::Cancelled` regardless of what the sink captured.
    pub fn was_cancelled(&self, session_id: &str) -> bool {
        self.0
            .lock()
            .unwrap()
            .get(session_id)
            .is_some_and(|e| e.cancel.is_cancelled())
    }

    /// Whether this session already has an unfinished turn thread tracked.
    /// `PromptRequest`'s handler must check this BEFORE overwriting
    /// `cascade`/`cancel`/`join` for a session — otherwise a second concurrent
    /// request (which is going to be rejected once it reaches `SessionLock`
    /// anyway) clobbers the first turn's live bookkeeping in the window
    /// before its own rejection lands.
    pub fn has_live_turn(&self, session_id: &str) -> bool {
        self.0
            .lock()
            .unwrap()
            .get(session_id)
            .is_some_and(|e| e.join.as_ref().is_some_and(|j| !j.is_finished()))
    }

    /// Whether `key` has already been recorded as seen for `session_id` — a
    /// pure read, no mutation. The handler calls this FIRST, before any
    /// other admission check (e.g. `has_live_turn`), so a genuine repeat of
    /// an already-*admitted* prompt is always caught, regardless of what
    /// else is going on in the session. Returns `false` for an unknown
    /// session (nothing recorded there; `resolve()` rejects unknown sessions
    /// separately).
    pub fn has_seen_idempotency_key(&self, session_id: &str, key: &str) -> bool {
        self.0
            .lock()
            .unwrap()
            .get(session_id)
            .is_some_and(|e| e.seen_idempotency_keys.contains(key))
    }

    /// Record `key` as seen for `session_id`. Callers MUST call this only
    /// after a request has passed every other admission check (in
    /// particular `has_live_turn`) and is actually going to run — recording
    /// any earlier would permanently blacklist a key whose logical prompt
    /// was rejected for some unrelated reason and never executed, silently
    /// dropping a legitimate later retry of that same prompt. No-op for an
    /// unknown session — nothing to record against; `resolve()` already
    /// rejects unknown sessions separately.
    pub fn record_idempotency_key(&self, session_id: &str, key: &str) {
        if let Some(entry) = self.0.lock().unwrap().get_mut(session_id) {
            entry.seen_idempotency_keys.insert(key.to_string());
        }
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

    /// The distinct gait-dirs this connection has touched (one per workspace the
    /// client opened a session in). Used by `session/list` when the request omits
    /// a cwd filter, and by close/delete to locate a session's gait-dir.
    pub fn known_gait_dirs(&self) -> Vec<PathBuf> {
        let mut dirs: Vec<PathBuf> = self
            .0
            .lock()
            .unwrap()
            .values()
            .map(|e| e.gait_dir.clone())
            .collect();
        dirs.sort();
        dirs.dedup();
        dirs
    }

    /// Cancel any in-flight turn, join its thread, and drop the entry. Idempotent:
    /// a session not in the map (already closed, or persisted by a prior
    /// connection) is a no-op. Returns the entry's gait_dir if it was present.
    pub fn remove(&self, session_id: &str) -> Option<PathBuf> {
        let entry = self.0.lock().unwrap().remove(session_id)?;
        entry.cancel.cancel();
        if let Some(join) = entry.join {
            let _ = join.join();
        }
        Some(entry.gait_dir)
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
    fn attach_existing_inserts_then_is_idempotent() {
        let m = SessionMap::new();
        let gait_dir = PathBuf::from("/g/.gait");
        let codebase = PathBuf::from("/code");
        assert!(
            m.attach_existing(gait_dir.clone(), "sess-1".into(), codebase.clone())
                .unwrap()
        );
        assert!(
            !m.attach_existing(gait_dir.clone(), "sess-1".into(), codebase.clone())
                .unwrap()
        );
        assert_eq!(m.resolve("sess-1"), Some((gait_dir, codebase)));
    }

    #[test]
    fn attach_existing_rejects_conflicting_workspace() {
        let m = SessionMap::new();
        m.insert(
            PathBuf::from("/g/.gait"),
            "sess-1".into(),
            PathBuf::from("/code"),
        );
        assert!(
            m.attach_existing(
                PathBuf::from("/other/.gait"),
                "sess-1".into(),
                PathBuf::from("/other"),
            )
            .is_err()
        );
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
    fn cancel_unknown_session_is_noop() {
        let m = SessionMap::new();
        m.cancel("nope"); // must not panic
        assert!(!m.was_cancelled("nope"));
    }

    #[test]
    fn cancel_trips_the_token() {
        let m = SessionMap::new();
        m.insert(PathBuf::from("/g/.gait"), "s".into(), PathBuf::from("/c"));
        m.set_cancel("s", tokio_util::sync::CancellationToken::new());
        assert!(!m.was_cancelled("s"), "fresh token is not cancelled");
        m.cancel("s");
        assert!(m.was_cancelled("s"), "cancel() trips the entry token");
    }

    #[test]
    fn cancel_peeks_and_leaves_the_cascade_takeable() {
        let m = SessionMap::new();
        m.insert(PathBuf::from("/g/.gait"), "s".into(), PathBuf::from("/c"));
        m.set_cascade("s", gait_core::embed::CascadeKillHandle::new());
        m.cancel("s");
        assert!(
            m.take_cascade("s").is_some(),
            "cancel() must not consume the cascade the turn thread still takes"
        );
    }

    #[test]
    fn set_cancel_resets_a_tripped_token() {
        let m = SessionMap::new();
        m.insert(PathBuf::from("/g/.gait"), "s".into(), PathBuf::from("/c"));
        m.set_cancel("s", tokio_util::sync::CancellationToken::new());
        m.cancel("s");
        assert!(m.was_cancelled("s"));
        // Next prompt mints a fresh token -> the session is no longer cancelled.
        m.set_cancel("s", tokio_util::sync::CancellationToken::new());
        assert!(
            !m.was_cancelled("s"),
            "a fresh per-prompt token clears cancellation"
        );
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

    #[test]
    fn known_gait_dirs_dedups_and_sorts() {
        let m = SessionMap::new();
        m.insert(PathBuf::from("/b/.gait"), "s1".into(), PathBuf::from("/b"));
        m.insert(PathBuf::from("/a/.gait"), "s2".into(), PathBuf::from("/a"));
        m.insert(PathBuf::from("/a/.gait"), "s3".into(), PathBuf::from("/a"));
        assert_eq!(
            m.known_gait_dirs(),
            vec![PathBuf::from("/a/.gait"), PathBuf::from("/b/.gait")]
        );
    }

    #[test]
    fn known_gait_dirs_empty_map_is_empty() {
        let m = SessionMap::new();
        assert!(m.known_gait_dirs().is_empty());
    }

    #[test]
    fn remove_returns_gait_dir_and_drops_entry() {
        let m = SessionMap::new();
        m.insert(PathBuf::from("/g/.gait"), "s".into(), PathBuf::from("/c"));
        assert_eq!(m.remove("s"), Some(PathBuf::from("/g/.gait")));
        assert!(!m.contains(Path::new("/g/.gait"), "s"));
        assert_eq!(m.known_gait_dirs(), Vec::<PathBuf>::new());
    }

    #[test]
    fn remove_unknown_session_is_noop() {
        let m = SessionMap::new();
        assert_eq!(m.remove("nope"), None);
    }

    #[test]
    fn has_live_turn_false_for_unknown_session() {
        let m = SessionMap::new();
        assert!(!m.has_live_turn("nope"));
    }

    #[test]
    fn has_live_turn_false_when_no_thread_set() {
        let m = SessionMap::new();
        m.insert(PathBuf::from("/g/.gait"), "s".into(), PathBuf::from("/c"));
        assert!(!m.has_live_turn("s"));
    }

    #[test]
    fn has_live_turn_true_while_thread_running() {
        let m = SessionMap::new();
        m.insert(PathBuf::from("/g/.gait"), "s".into(), PathBuf::from("/c"));
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        m.set_turn_thread(
            "s",
            std::thread::spawn(move || {
                let _ = rx.recv();
            }),
        );
        assert!(m.has_live_turn("s"));
        drop(tx); // let the thread finish so the test doesn't leak it
    }

    #[test]
    fn has_live_turn_false_after_thread_finishes() {
        let m = SessionMap::new();
        m.insert(PathBuf::from("/g/.gait"), "s".into(), PathBuf::from("/c"));
        let join = std::thread::spawn(|| {});
        join.join().unwrap();
        // Re-spawn a finished handle isn't possible; store a fresh already-done one.
        m.set_turn_thread("s", std::thread::spawn(|| {}));
        // Give it a moment to actually finish (it does nothing, but avoid a race).
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert!(!m.has_live_turn("s"));
    }

    #[test]
    fn remove_cancels_and_joins_live_turn() {
        let m = SessionMap::new();
        m.insert(PathBuf::from("/g/.gait"), "s".into(), PathBuf::from("/c"));
        m.set_cancel("s", tokio_util::sync::CancellationToken::new());
        m.set_turn_thread("s", std::thread::spawn(|| {}));
        // Must not panic (join succeeds) and must return the gait_dir; the
        // thread's completion is implicit in remove() returning at all.
        assert_eq!(m.remove("s"), Some(PathBuf::from("/g/.gait")));
        assert!(
            !m.contains(Path::new("/g/.gait"), "s"),
            "entry dropped after remove"
        );
    }

    #[test]
    fn has_seen_idempotency_key_false_until_recorded() {
        let m = SessionMap::new();
        m.insert(PathBuf::from("/g/.gait"), "s".into(), PathBuf::from("/c"));
        assert!(!m.has_seen_idempotency_key("s", "k1"));
        m.record_idempotency_key("s", "k1");
        assert!(m.has_seen_idempotency_key("s", "k1"));
    }

    #[test]
    fn record_idempotency_key_distinguishes_keys() {
        let m = SessionMap::new();
        m.insert(PathBuf::from("/g/.gait"), "s".into(), PathBuf::from("/c"));
        m.record_idempotency_key("s", "k1");
        assert!(!m.has_seen_idempotency_key("s", "k2"));
    }

    #[test]
    fn has_seen_idempotency_key_false_for_unknown_session() {
        let m = SessionMap::new();
        assert!(!m.has_seen_idempotency_key("nope", "k1"));
    }

    #[test]
    fn record_idempotency_key_unknown_session_is_noop() {
        let m = SessionMap::new();
        m.record_idempotency_key("nope", "k1");
        assert!(!m.has_seen_idempotency_key("nope", "k1"));
    }
}

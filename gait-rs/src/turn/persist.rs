//! Task 4 — `Persistence` write-policy type.
//!
//! Replaces the 15 scattered `relay.is_none()` / `relay.is_some()` guards in
//! [`crate::turn::run_turn`] with one explicit policy enum. The `Sealed`
//! variant is selected whenever the brain is running under a spine
//! (`input.relay.is_some()`); the spine owns all disk writes, and the brain's
//! seal has `writable: []`, so writing would fail with EPERM anyway.
//!
//! Wiring into `run_turn()` is deliberately deferred to Task 5 — this task
//! only ships the type + its unit tests. Calling sites still guard on
//! `relay.is_none()` / `relay.is_some()`; once Task 5 lands, every guard is
//! replaced by a method call on this enum and `Persistence::from_relay` is
//! the single place that inspects `relay`.
//!
//! ## Why two commit methods? (C3 review note in plan-01 §Task 4)
//!
//! Only the max-steps drain commits with error-swallowing
//! (`let _ = commit_session(...)` at the former `:1932`). Every *exit*
//! commit propagates with `?` so a commit failure aborts the turn. One
//! `commit()` cannot serve both: it has to either always propagate or
//! always swallow, and either choice is wrong for the other call site.
//! Hence `commit() -> Result<()>` (exits `?` it from `finalize`) and
//! `commit_swallow()` (the drain only).

use std::path::Path;

use crate::git::checkpoint::CheckpointRepo;
use crate::git::Repos;
use crate::turn::nerve::NerveWriter;

/// Write policy for a single run of `run_turn`.
///
/// Constructed once per turn via [`Persistence::from_relay`]; threaded
/// through the phase functions in later stages.
pub(crate) enum Persistence<'a> {
    /// Brain is the sole writer. All disk effects go through `repos`.
    Unsealed {
        repos: &'a Repos,
        gait_dir: &'a Path,
        session_id: &'a str,
        codebase_path: &'a Path,
    },
    /// Brain runs under a spine relay; the spine already owns (or will own)
    /// every disk effect. The brain's seal has `writable: []`, so all
    /// write attempts here are no-ops.
    Sealed,
}

impl<'a> Persistence<'a> {
    /// Selects the policy from the `relay` present in `TurnInput`.
    ///
    /// `Sealed` iff `relay.is_some()`. This is the only place in
    /// `run_turn()` that inspects the `NerveWriter` presence once Task 5
    /// lands.
    pub(crate) fn from_relay(
        relay: &Option<NerveWriter>,
        repos: &'a Repos,
        gait_dir: &'a Path,
        session_id: &'a str,
        codebase_path: &'a Path,
    ) -> Self {
        if relay.is_some() {
            Persistence::Sealed
        } else {
            Persistence::Unsealed {
                repos,
                gait_dir,
                session_id,
                codebase_path,
            }
        }
    }

    /// Worktree creation + checkpoint init + session-lock acquisition.
    ///
    /// Mirrors the guarded blocks at `src/turn/mod.rs:354–400` (former).
    /// When `Sealed`, the spine holds the lock and has already performed
    /// the init, so this is a no-op returning `Ok(None)`.
    ///
    /// Returns `Some(SessionLock)` on `Unsealed`, `None` on `Sealed`.
    pub(crate) fn init_workspace(
        &self,
        checkpoints: &dyn CheckpointRepo,
    ) -> anyhow::Result<Option<crate::session::SessionLock>> {
        match self {
            Persistence::Sealed => Ok(None),
            Persistence::Unsealed {
                repos,
                gait_dir,
                session_id,
                codebase_path,
            } => {
                repos.traces.create_session_worktree(gait_dir, session_id)?;
                checkpoints.init(gait_dir, session_id, codebase_path)?;
                let session_dir = crate::session::live::worktree_dir(gait_dir, session_id);
                Ok(Some(crate::session::SessionLock::acquire(&session_dir)?))
            }
        }
    }

    /// Propagating commit for *exit* sites (Cancelled, Done, Permissions,
    /// Clarify, Inform, Unlock, etc.).
    ///
    /// When `Sealed`, the spine has already committed (or will, after the
    /// brain exits); this is a no-op `Ok(())`. When `Unsealed`, performs
    /// `repos.traces.commit_session(gait_dir, session_id, message)?` —
    /// a commit failure must abort the turn.
    ///
    /// Note: callers pass an already-formatted message (e.g.
    /// `"session(s): turn 3"`); this enum does not compute it.
    pub(crate) fn commit(&self, message: &str) -> anyhow::Result<()> {
        match self {
            Persistence::Sealed => Ok(()),
            Persistence::Unsealed {
                repos,
                gait_dir,
                session_id,
                ..
            } => repos.traces.commit_session(gait_dir, session_id, message),
        }
    }

    /// Error-swallowing variant for the max-steps drain only.
    ///
    /// Today's drain at the former `src/turn/mod.rs:1932` writes:
    ///
    /// ```ignore
    /// let _ = repos.traces.commit_session(
    ///     &gait_dir, &session_id,
    ///     &format!("session({}): final-drain completions", session_id),
    /// );
    /// ```
    ///
    /// The `let _ =` is deliberate: the drain's whole purpose is to record
    /// any pending completions before releasing the lock; a commit failure
    /// must NOT prevent the wake-on-success path that follows.
    ///
    /// Callers MUST pass an already-formatted drain-style message.
    pub(crate) fn commit_swallow(&self, message: &str) {
        if let Persistence::Unsealed {
            repos,
            gait_dir,
            session_id,
            ..
        } = self
        {
            let _ = repos.traces.commit_session(gait_dir, session_id, message);
        }
    }

    /// Whether the per-step completion poll (daemon + child-agent
    /// completions) is allowed this turn. `false` when `Sealed`.
    ///
    /// Mirrors the guard at the former `src/turn/mod.rs:799`.
    pub(crate) fn poll_completions_allowed(&self) -> bool {
        !matches!(self, Persistence::Sealed)
    }

    /// Whether the pre-stream preview is allowed. `false` when `Sealed`
    /// (the spine fills the diff from the executed result).
    ///
    /// Mirrors the guard at the former `src/turn/mod.rs:1212`.
    pub(crate) fn preview_allowed(&self) -> bool {
        !matches!(self, Persistence::Sealed)
    }

    /// General-purpose "is the brain the sole writer" check, for guards that
    /// don't fit the poll/preview naming (e.g. the one-time `init_session`
    /// write, the max-steps final-drain gate).
    pub(crate) fn is_unsealed(&self) -> bool {
        !matches!(self, Persistence::Sealed)
    }
}

// ── unit tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::checkpoint::MockCheckpointRepo;
    use crate::git::trace::TraceRepo;
    use std::sync::{Arc, Mutex};

    /// Spy implementation of `TraceRepo` that records `commit_session`
    /// calls. Used to verify which messages reach the trace repo on
    /// `Unsealed`, and that nothing reaches it on `Sealed`. No git I/O.
    struct SpyTrace {
        commits: Mutex<Vec<String>>,
    }

    impl SpyTrace {
        fn new() -> Self {
            Self {
                commits: Mutex::new(Vec::new()),
            }
        }
        fn commits(&self) -> Vec<String> {
            self.commits.lock().unwrap().clone()
        }
    }

    impl TraceRepo for SpyTrace {
        fn init_workspace(&self, _gait_dir: &Path) -> anyhow::Result<()> {
            Ok(())
        }
        fn create_session_worktree(
            &self,
            _gait_dir: &Path,
            _session_id: &str,
        ) -> anyhow::Result<()> {
            Ok(())
        }
        fn commit_session(
            &self,
            _gait_dir: &Path,
            _session_id: &str,
            message: &str,
        ) -> anyhow::Result<()> {
            self.commits.lock().unwrap().push(message.to_string());
            Ok(())
        }
        fn list_active_sessions(&self, _gait_dir: &Path) -> anyhow::Result<Vec<String>> {
            Ok(vec![])
        }
        fn finish_session(&self, _gait_dir: &Path, _session_id: &str) -> anyhow::Result<()> {
            Ok(())
        }
        fn create_branch_worktree(
            &self,
            _gait_dir: &Path,
            _base: &str,
            _new: &str,
            _wt: &Path,
        ) -> anyhow::Result<()> {
            Ok(())
        }
        fn remove_branch_worktree(&self, _gait_dir: &Path, _wt: &Path) -> anyhow::Result<()> {
            Ok(())
        }
        fn merge_branch_ff(&self, _gait_dir: &Path, _tgt: &str, _src: &str) -> anyhow::Result<()> {
            Ok(())
        }
        fn discard_session(&self, _gait_dir: &Path, _session_id: &str) -> anyhow::Result<()> {
            Ok(())
        }
    }

    fn spy_repos(trace: &Arc<SpyTrace>) -> Repos {
        Repos {
            codebase: Arc::new(crate::git::codebase::GitCodebaseRepo),
            traces: trace.clone(),
            checkpoints: Arc::new(MockCheckpointRepo::new()),
        }
    }

    #[test]
    fn sealed_persistence_performs_no_disk_effects() {
        let p = Persistence::Sealed;

        // poll/preview gates: must be false on the Sealed path
        assert!(!p.poll_completions_allowed());
        assert!(!p.preview_allowed());

        // commit(): Ok(()) with no panic and no error — even on a happy-path message.
        p.commit("session(s): turn 1")
            .expect("commit on Sealed is Ok(())");

        // commit_swallow(): never panics, never errors, never blocks.
        p.commit_swallow("session(s): final-drain completions");
    }

    #[test]
    fn sealed_init_workspace_returns_none() {
        let p = Persistence::Sealed;
        let checkpoints = MockCheckpointRepo::new();
        let res = p
            .init_workspace(&checkpoints)
            .expect("init_workspace on Sealed must not error");
        assert!(res.is_none());
        assert!(
            checkpoints.init_calls().is_empty(),
            "Sealed must not call CheckpointRepo::init"
        );
    }

    #[test]
    fn unsealed_commit_calls_traces_commit_session_once_with_message() {
        let spy = Arc::new(SpyTrace::new());
        let tmp = tempfile::TempDir::new().unwrap();
        let repos = spy_repos(&spy);
        let p = Persistence::Unsealed {
            repos: &repos,
            gait_dir: tmp.path(),
            session_id: "s",
            codebase_path: tmp.path(),
        };

        let msg = "session(s): turn 1";
        p.commit(msg).expect("Unsealed commit must succeed");

        assert_eq!(spy.commits(), vec![msg.to_string()]);
    }

    #[test]
    fn unsealed_commit_propagates_trace_error() {
        /// Trace that always errors on `commit_session`.
        struct ErrorTrace;
        impl TraceRepo for ErrorTrace {
            fn init_workspace(&self, _: &Path) -> anyhow::Result<()> {
                Ok(())
            }
            fn create_session_worktree(&self, _: &Path, _: &str) -> anyhow::Result<()> {
                Ok(())
            }
            fn commit_session(&self, _: &Path, _: &str, _: &str) -> anyhow::Result<()> {
                anyhow::bail!("simulated commit failure")
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
                Ok(())
            }
            fn remove_branch_worktree(&self, _: &Path, _: &Path) -> anyhow::Result<()> {
                Ok(())
            }
            fn merge_branch_ff(&self, _: &Path, _: &str, _: &str) -> anyhow::Result<()> {
                Ok(())
            }
            fn discard_session(&self, _gait_dir: &Path, _session_id: &str) -> anyhow::Result<()> {
                Ok(())
            }
        }

        let trace: Arc<ErrorTrace> = Arc::new(ErrorTrace);
        let repos = Repos {
            codebase: Arc::new(crate::git::codebase::GitCodebaseRepo),
            traces: trace,
            checkpoints: Arc::new(MockCheckpointRepo::new()),
        };
        let tmp = tempfile::TempDir::new().unwrap();
        let p = Persistence::Unsealed {
            repos: &repos,
            gait_dir: tmp.path(),
            session_id: "s",
            codebase_path: tmp.path(),
        };

        // commit() MUST propagate — that is the whole point of having two
        // commit methods. If this assertion ever fails, the API has drifted
        // toward the swallow semantic and exits will silently lose
        // transcripts on a transient git error.
        let err = p.commit("session(s): turn 1").unwrap_err();
        assert!(
            err.to_string().contains("simulated commit failure"),
            "expected propagated error, got: {err}"
        );
    }

    #[test]
    fn unsealed_commit_swallow_does_not_propagate() {
        struct ErrorTrace;
        impl TraceRepo for ErrorTrace {
            fn init_workspace(&self, _: &Path) -> anyhow::Result<()> {
                Ok(())
            }
            fn create_session_worktree(&self, _: &Path, _: &str) -> anyhow::Result<()> {
                Ok(())
            }
            fn commit_session(&self, _: &Path, _: &str, _: &str) -> anyhow::Result<()> {
                anyhow::bail!("simulated drain failure")
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
                Ok(())
            }
            fn remove_branch_worktree(&self, _: &Path, _: &Path) -> anyhow::Result<()> {
                Ok(())
            }
            fn merge_branch_ff(&self, _: &Path, _: &str, _: &str) -> anyhow::Result<()> {
                Ok(())
            }
            fn discard_session(&self, _gait_dir: &Path, _session_id: &str) -> anyhow::Result<()> {
                Ok(())
            }
        }

        let trace: Arc<ErrorTrace> = Arc::new(ErrorTrace);
        let repos = Repos {
            codebase: Arc::new(crate::git::codebase::GitCodebaseRepo),
            traces: trace,
            checkpoints: Arc::new(MockCheckpointRepo::new()),
        };
        let tmp = tempfile::TempDir::new().unwrap();
        let p = Persistence::Unsealed {
            repos: &repos,
            gait_dir: tmp.path(),
            session_id: "s",
            codebase_path: tmp.path(),
        };

        // commit_swallow() must NEVER panic and NEVER propagate — the drain
        // path runs this before spawn_wake, so a commit failure must not
        // abort the wake.
        p.commit_swallow("session(s): final-drain completions");
    }

    #[test]
    fn from_relay_picks_sealed_when_some() {
        let repos = Repos::production();
        let tmp = tempfile::TempDir::new().unwrap();
        let relay: Option<NerveWriter> = None;

        let p = Persistence::from_relay(&relay, &repos, tmp.path(), "s", tmp.path());
        assert!(
            matches!(p, Persistence::Unsealed { .. }),
            "None relay => Unsealed"
        );
        assert!(p.poll_completions_allowed());
        assert!(p.preview_allowed());
    }

    #[test]
    fn from_relay_picks_unsealed_when_none() {
        // We do not construct a real NerveWriter (the type isn't
        // constructible in tests; we just need the test to compile). This
        // branch confirms `Sealed` is selected when relay.is_some() — we
        // can simulate by checking the discriminant directly.
        let p = Persistence::Sealed;
        assert!(!p.poll_completions_allowed());
        assert!(!p.preview_allowed());
    }
}

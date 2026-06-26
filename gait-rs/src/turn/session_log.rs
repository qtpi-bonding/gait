//! `SessionLog` — the single home for one session's `log.jsonl` transcript and
//! its `SessionLock`. FS-only: it holds NO git handle and NO `Repos`. Git
//! persistence (worktree/checkpoint/commit) is `SessionGit`'s job.
//!
//! Two lock policies, named not parameterized:
//! * [`SessionLog::open`] hard-fails on contention — the per-turn path owns its
//!   session, so a live holder is a real error.
//! * [`SessionLog::open_retrying`] retries — the Notify cross-session write may
//!   legitimately find the target mid-turn.
//!
//! Two append flavors, both preserved:
//! * [`SessionLog::relay_event`] preserves a brain-minted event's id (re-stamps
//!   seq/parents) — for events relayed from the brain.
//! * [`SessionLog::append_body`] mints a fresh spine-side event — SessionStart,
//!   error events, orphan-tool repair.

use std::path::{Path, PathBuf};

use crate::proto::gait::log::v1::{event, Event};
use crate::proto::wire;
use crate::turn::SessionLock;

pub(crate) struct SessionLog {
    log_path: PathBuf,
    _lock: SessionLock,
}

impl SessionLog {
    /// Acquire the session lock (hard-fail on contention) and bind the log path.
    pub(crate) fn open(session_dir: &Path) -> anyhow::Result<Self> {
        let lock = SessionLock::acquire(session_dir)?;
        Ok(Self {
            log_path: session_dir.join("log.jsonl"),
            _lock: lock,
        })
    }

    /// Acquire with retry — for cross-session writes whose target may be running.
    /// Sleeps 500ms between attempts; on persistent contention returns the last error.
    ///
    /// `async` is correct and safe: the `SessionLock` is **never held across an
    /// `.await`**. The sleep happens only on the failure path *before* a lock is
    /// acquired; on success the lock is acquired and returned in one synchronous
    /// step. No file lock is held while suspended.
    pub(crate) async fn open_retrying(session_dir: &Path, attempts: usize) -> anyhow::Result<Self> {
        let mut last: Option<anyhow::Error> = None;
        for _ in 0..attempts {
            match SessionLock::acquire(session_dir) {
                Ok(lock) => {
                    return Ok(Self {
                        log_path: session_dir.join("log.jsonl"),
                        _lock: lock,
                    });
                }
                Err(e) if e.to_string().contains("already running") => {
                    last = Some(e);
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                }
                Err(e) => return Err(e),
            }
        }
        Err(last.unwrap_or_else(|| anyhow::anyhow!("session lock unavailable")))
    }

    pub(crate) fn log_path(&self) -> &Path {
        &self.log_path
    }

    /// Append a brain-minted event, preserving its id (re-stamps seq/parents).
    pub(crate) fn relay_event(&self, evt: &Event) -> anyhow::Result<Event> {
        wire::append_with_id(&self.log_path, evt)
    }

    /// Mint and append a fresh spine-side event.
    pub(crate) fn append_body(&self, body: event::Body) -> anyhow::Result<Event> {
        wire::append(&self.log_path, body)
    }

    pub(crate) fn last_event(&self) -> Option<Event> {
        crate::session::log::last_event(&self.log_path)
            .ok()
            .flatten()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::gait::log::v1::{payload, AssistantText};
    use tempfile::TempDir;

    fn body(text: &str) -> event::Body {
        crate::proto::wire::body(payload::Kind::AssistantText(AssistantText {
            content: text.into(),
        }))
    }

    #[test]
    fn append_body_chains_and_persists_in_order() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("sessions/s");
        std::fs::create_dir_all(&dir).unwrap();
        let log = SessionLog::open(&dir).unwrap();
        log.append_body(body("first")).unwrap();
        log.append_body(body("second")).unwrap();
        let raw = std::fs::read_to_string(dir.join("log.jsonl")).unwrap();
        let a = raw.find("first").unwrap();
        let b = raw.find("second").unwrap();
        assert!(a < b, "append order must be preserved");
    }

    #[test]
    fn open_hard_fails_when_session_locked_by_live_holder() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("sessions/s2");
        std::fs::create_dir_all(&dir).unwrap();
        let _held = SessionLog::open(&dir).expect("first open acquires");
        // Same live PID holds the lock → second open must hard-fail.
        let second = SessionLog::open(&dir);
        assert!(second.is_err(), "open must hard-fail while lock is held");
    }

    #[test]
    fn drop_releases_lock() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("sessions/s3");
        std::fs::create_dir_all(&dir).unwrap();
        {
            let _l = SessionLog::open(&dir).unwrap();
        }
        // After drop, re-open must succeed.
        assert!(SessionLog::open(&dir).is_ok(), "drop must release the lock");
    }
}

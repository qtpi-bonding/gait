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
use std::sync::Mutex;

use crate::proto::gait::log::v1::{event, Event};
use crate::proto::wire;
use crate::turn::SessionLock;

pub(crate) enum SessionLog {
    /// Production: a real `log.jsonl` plus the session lock (released on drop).
    Persistent {
        log_path: PathBuf,
        _lock: SessionLock,
    },
    /// Ephemeral one-off: an in-memory append-only event log. No file, no lock.
    Ephemeral { events: Mutex<Vec<Event>> },
}

impl SessionLog {
    /// Acquire the session lock (hard-fail on contention) and bind the log path.
    pub(crate) fn open(session_dir: &Path) -> anyhow::Result<Self> {
        let lock = SessionLock::acquire(session_dir)?;
        Ok(Self::Persistent {
            log_path: session_dir.join("log.jsonl"),
            _lock: lock,
        })
    }

    /// Acquire with retry — for cross-session writes whose target may be running.
    /// Sleeps 500ms between attempts; on persistent contention returns the last error.
    /// The `SessionLock` is never held across an `.await` (sleep is on the pre-acquire
    /// failure path only).
    pub(crate) async fn open_retrying(session_dir: &Path, attempts: usize) -> anyhow::Result<Self> {
        let mut last: Option<anyhow::Error> = None;
        for _ in 0..attempts {
            match SessionLock::acquire(session_dir) {
                Ok(lock) => {
                    return Ok(Self::Persistent {
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

    /// In-memory transcript seeded with `seed` (e.g. the one-off user input). No
    /// file, no lock — the ephemeral run persists nothing.
    pub(crate) fn ephemeral(seed: Vec<Event>) -> Self {
        Self::Ephemeral {
            events: Mutex::new(seed),
        }
    }

    /// Append a brain-minted event, preserving its id (re-stamps seq/parents in the
    /// persistent case; pushes verbatim in memory).
    pub(crate) fn relay_event(&self, evt: &Event) -> anyhow::Result<Event> {
        match self {
            Self::Persistent { log_path, .. } => wire::append_with_id(log_path, evt),
            Self::Ephemeral { events } => {
                events.lock().unwrap().push(evt.clone());
                Ok(evt.clone())
            }
        }
    }

    /// Mint and append a fresh spine-side event.
    pub(crate) fn append_body(&self, body: event::Body) -> anyhow::Result<Event> {
        match self {
            Self::Persistent { log_path, .. } => wire::append(log_path, body),
            Self::Ephemeral { events } => {
                let mut guard = events.lock().unwrap();
                let prev = guard.last();
                let evt = crate::proto::envelope::mint(body, prev, 0, 0);
                guard.push(evt.clone());
                Ok(evt)
            }
        }
    }

    /// The full transcript, in order. The brain's `BrainInit.events` and the
    /// post-turn `count_turn_complete` both source from here.
    pub(crate) fn events(&self) -> Vec<Event> {
        match self {
            Self::Persistent { log_path, .. } => {
                crate::session::log::read_events(log_path).unwrap_or_default()
            }
            Self::Ephemeral { events } => events.lock().unwrap().clone(),
        }
    }

    pub(crate) fn last_event(&self) -> Option<Event> {
        match self {
            Self::Persistent { log_path, .. } => {
                crate::session::log::last_event(log_path).ok().flatten()
            }
            Self::Ephemeral { events } => events.lock().unwrap().last().cloned(),
        }
    }

    /// The on-disk path, for the few persistent-only call sites (SessionStart
    /// bootstrap). `None` in ephemeral mode.
    pub(crate) fn log_path(&self) -> Option<&Path> {
        match self {
            Self::Persistent { log_path, .. } => Some(log_path),
            Self::Ephemeral { .. } => None,
        }
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

    fn envelope_mint(b: event::Body) -> Event {
        crate::proto::envelope::mint(b, None, 0, 0)
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

    #[test]
    fn ephemeral_accumulates_in_memory_and_writes_no_file() {
        let log = SessionLog::ephemeral(vec![]);
        log.append_body(body("first")).unwrap();
        log.relay_event(&envelope_mint(body("second"))).unwrap();
        let evs = log.events();
        assert_eq!(evs.len(), 2, "ephemeral log accumulates appends");
        assert!(matches!(log, SessionLog::Ephemeral { .. }));
    }

    #[test]
    fn ephemeral_events_returns_seed() {
        let seed = vec![envelope_mint(body("seed"))];
        let log = SessionLog::ephemeral(seed);
        assert_eq!(log.events().len(), 1);
        assert!(log.last_event().is_some());
    }
}

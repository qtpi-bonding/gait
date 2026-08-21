//! `SessionLog` — the single home for one session's `log.jsonl` transcript and
//! its `SessionLock`. FS-only: it holds NO git handle and NO `Repos`. Git
//! persistence (worktree/checkpoint/commit) is `SessionGit`'s job.
//!
//! One lock policy:
//! * [`SessionLog::open`] hard-fails on contention — the per-turn path owns its
//!   session, so a live holder is a real error.
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
use crate::session::SessionLock;

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
    /// Acquire the session lock (on the worktree-root mount) and bind the
    /// (nested) log path. These two are no longer siblings under one
    /// directory: the lock lives at `gait_dir/.active/<id>/lock`, the log at
    /// `gait_dir/.active/<id>/sessions/<id>/log.jsonl` — see
    /// `crate::session::live`.
    pub(crate) fn open(gait_dir: &Path, session_id: &str) -> anyhow::Result<Self> {
        let lock_dir = crate::session::live::worktree_dir(gait_dir, session_id);
        let lock = SessionLock::acquire(&lock_dir)?;
        let log_path = crate::session::live::log_path(gait_dir, session_id);
        // The nested `sessions/<id>/` directory may not exist yet on a brand
        // new session (create_session_worktree's orphan commit is empty) —
        // SessionLog owns log.jsonl, so it ensures its own parent exists.
        if let Some(parent) = log_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        Ok(Self::Persistent {
            log_path,
            _lock: lock,
        })
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
    pub(crate) fn append_body(
        &self,
        body: event::Body,
        scope: crate::proto::envelope::EventScope,
    ) -> anyhow::Result<Event> {
        match self {
            Self::Persistent { log_path, .. } => wire::write_to_transcript(log_path, body, scope),
            Self::Ephemeral { events } => {
                let mut guard = events.lock().unwrap();
                let prev = guard.last();
                let evt = crate::proto::envelope::mint(body, prev, scope);
                guard.push(evt.clone());
                Ok(evt)
            }
        }
    }

    pub(crate) fn append_body_with_provenance(
        &self,
        body: event::Body,
        scope: crate::proto::envelope::EventScope,
        provenance: crate::proto::gait::log::v1::Provenance,
    ) -> anyhow::Result<Event> {
        match self {
            Self::Persistent { log_path, .. } => {
                wire::append_with_given_provenance(log_path, body, provenance, scope)
            }
            Self::Ephemeral { events } => {
                let mut guard = events.lock().unwrap();
                let prev = guard.last();
                let mut evt = crate::proto::envelope::mint(body, prev, scope);
                evt.provenance = Some(provenance);
                guard.push(evt.clone());
                Ok(evt)
            }
        }
    }

    /// The full transcript, in order. The brain's `BrainInit.events` and the
    /// post-turn `count_run_complete` both source from here.
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
        crate::proto::envelope::mint(b, None, crate::proto::envelope::EventScope::Session)
    }

    #[test]
    fn append_body_chains_and_persists_in_order() {
        let gait_dir = TempDir::new().unwrap();
        std::fs::create_dir_all(crate::session::live::worktree_dir(gait_dir.path(), "s")).unwrap();
        let log = SessionLog::open(gait_dir.path(), "s").unwrap();
        log.append_body(body("first"), crate::proto::envelope::EventScope::Session)
            .unwrap();
        log.append_body(body("second"), crate::proto::envelope::EventScope::Session)
            .unwrap();
        let raw =
            std::fs::read_to_string(crate::session::live::log_path(gait_dir.path(), "s")).unwrap();
        let a = raw.find("first").unwrap();
        let b = raw.find("second").unwrap();
        assert!(a < b, "append order must be preserved");
    }

    #[test]
    fn open_hard_fails_when_session_locked_by_live_holder() {
        let gait_dir = TempDir::new().unwrap();
        std::fs::create_dir_all(crate::session::live::worktree_dir(gait_dir.path(), "s2")).unwrap();
        let _held = SessionLog::open(gait_dir.path(), "s2").expect("first open acquires");
        // Same live PID holds the lock → second open must hard-fail.
        let second = SessionLog::open(gait_dir.path(), "s2");
        assert!(second.is_err(), "open must hard-fail while lock is held");
    }

    #[test]
    fn drop_releases_lock() {
        let gait_dir = TempDir::new().unwrap();
        std::fs::create_dir_all(crate::session::live::worktree_dir(gait_dir.path(), "s3")).unwrap();
        {
            let _l = SessionLog::open(gait_dir.path(), "s3").unwrap();
        }
        // After drop, re-open must succeed.
        assert!(
            SessionLog::open(gait_dir.path(), "s3").is_ok(),
            "drop must release the lock"
        );
    }

    #[test]
    fn ephemeral_accumulates_in_memory_and_writes_no_file() {
        let log = SessionLog::ephemeral(vec![]);
        log.append_body(body("first"), crate::proto::envelope::EventScope::Session)
            .unwrap();
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

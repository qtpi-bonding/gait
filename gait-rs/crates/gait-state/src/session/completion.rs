//! Shared "notify parent completion" helper.
//!
//! Used by the daemon exit-hook (Task 5) and — in Part 2 — the subagent path.
//! On an idle parent, records a `*_update` event into the parent's `log.jsonl`,
//! commits, then spawns a fresh `gait run` to wake the parent. If the parent
//! is busy (a turn holds the session lock), the call is a no-op for the wake —
//! Part 2's per-step poll will surface the completion. Safe to call
//! speculatively: if a `*_update` for this completion is already present,
//! the call returns `AlreadySurfaced` without writing or waking.
//!
//! Concurrency contract: ONE-SHOT lock acquire (no retry). We hold the lock
//! for the duration of the append+commit, then drop it before spawning the
//! wake — the wake re-acquires. The append+commit path uses `wire::append`
//! directly (NOT `SessionLog::open`, which would re-acquire and deadlock).
//! This mirrors the "already hold the lock" pattern in `harness.rs:118-128`.

use std::path::{Path, PathBuf};

use crate::git::{build_trace_message, TraceTrailers};
use crate::proto::gait::log::v1::event::Body;

/// Pure-data wake intent returned by [`notify_parent`] on the `Woke` path.
/// The state layer builds this; the *engine* caller turns it into a
/// `WakeLaunch` + process spawn. Carrying intent rather than spawning here
/// keeps `session/` free of the engine `launch` module — the
/// state→engine dependency inversion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WakeRequest {
    pub gait_dir: PathBuf,
    pub parent_session: String,
}

/// A completion event to surface to a parent session.
///
/// `idempotency_key` is the producer-supplied stable identity of the specific
/// completion being reported (a daemon *run*, or an agent's state *at a given
/// log-tail event*) — NOT derived from payload field values. Two completions
/// with identical `payload` content but different `idempotency_key`s are
/// genuinely distinct events (e.g. two separate runs of a same-named daemon)
/// and must never be deduped against each other; see `matches()` below.
#[derive(Debug, Clone)]
pub struct Completion {
    /// Matches what `events::agent_update`/`events::daemon_update` already
    /// return — no extraction/wrapping needed.
    pub payload: crate::proto::gait::log::v1::event::Body,
    pub idempotency_key: String,
}

/// The outcome of `notify_parent`. Tells the caller what actually happened
/// so it can decide whether to log a warning (e.g. busy parent) or proceed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotifyOutcome {
    /// Idle parent → event written + commit done; the caller should spawn the
    /// wake using the carried [`WakeRequest`] (the engine builds the
    /// `WakeLaunch` from it).
    Woke(WakeRequest),
    /// A turn holds the session lock; do nothing (Part 2's poll will surface).
    ParentBusy,
    /// The parent session directory does not exist (unrecoverable: skip).
    NoParent,
    /// The completion has already been surfaced for this parent (dedup hit);
    /// no write, no wake — caller may treat as success.
    AlreadySurfaced,
}

impl Completion {
    /// Build the proto event body for this completion.
    pub fn body(&self) -> Body {
        self.payload.clone()
    }

    /// True if `ev` is a `*_update` event for this completion, compared by
    /// `idempotency_key` alone — NOT by payload field values (that was the
    /// bug: a legitimately-repeated status report with identical field
    /// values but a distinct idempotency_key must NOT be deduped away). Used
    /// for the speculative dedup check: after we acquire the lock we scan the
    /// parent log for a matching event and return `AlreadySurfaced` instead
    /// of writing + waking a second time.
    fn matches(&self, ev: &crate::proto::gait::log::v1::Event) -> bool {
        use crate::proto::gait::log::v1::payload;
        match crate::proto::event_kind(ev) {
            Some(payload::Kind::AgentUpdate(au)) => au.idempotency_key == self.idempotency_key,
            Some(payload::Kind::DaemonUpdate(du)) => du.idempotency_key == self.idempotency_key,
            _ => false,
        }
    }
}

/// Idle parent → write the completion event + commit, then spawn a wake turn.
/// Busy parent (lock held) → do nothing (the live turn polls — Part 2).
/// If a `*_update` for this completion is already in the parent log, return
/// `AlreadySurfaced` without writing or waking (safe to call speculatively).
pub fn notify_parent(
    gait_dir: &Path,
    repos: &crate::git::Repos,
    parent_session: &str,
    c: &Completion,
) -> NotifyOutcome {
    let target_dir = crate::session::live::worktree_dir(gait_dir, parent_session);
    if !target_dir.exists() {
        return NotifyOutcome::NoParent;
    }
    // One-shot lock acquire — the lock IS the "is a turn running?" check.
    let lock = match crate::session::SessionLock::acquire(&target_dir) {
        Ok(l) => l,
        Err(e) if e.to_string().contains("already running") => return NotifyOutcome::ParentBusy,
        Err(_) => return NotifyOutcome::NoParent, // unexpected I/O — leave registry as fallback
    };
    // Dedup: if the parent's log already has a matching *_update for this
    // completion, drop the lock and report it. Lets the caller be sure
    // notify_parent is safe to call speculatively (Part 2's post-drop scan
    // and final-drain coexist).
    let tx_path = crate::session::live::log_path(gait_dir, parent_session);
    // Unlike the old flat log.jsonl (a sibling of the always-present worktree
    // root), the nested sessions/<id>/ directory may not exist yet — wire::append
    // opens the file directly with no create_dir_all of its own (see
    // gait-proto's write_to_transcript). Ensure it exists before appending.
    if let Some(parent) = tx_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(events) = crate::proto::wire::read_events(&tx_path) {
        if events.iter().any(|ev| c.matches(ev)) {
            return NotifyOutcome::AlreadySurfaced;
        }
    }
    // Append the *_update event + commit. We hold the lock, so writing
    // directly via `wire::append` is the legal path (NOT SessionLog::open,
    // which would re-acquire and deadlock). Mirrors harness.rs:118-128.
    let signer = match gait_identity::Keyring::load(gait_dir).and_then(|kr| kr.signer()) {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "[gait] notify_parent: loading keyring at {}: {e}",
                gait_dir.display()
            );
            return NotifyOutcome::NoParent;
        }
    };
    use gait_identity::Signer as _;
    if let Err(e) = crate::proto::wire::append_with_provenance(
        &tx_path,
        c.body(),
        signer.public_key().to_vec(),
        signer.scheme() as i32,
        |msg| signer.sign(msg),
        vec![],
        String::new(),
        crate::proto::envelope::EventScope::Session,
    ) {
        // Log the append failure as a diagnostic warn — silently dropping
        // would leave the caller with no signal that the completion was lost.
        eprintln!("[gait] notify_parent: append to {tx_path:?} failed: {e}");
        let _ = repos;
        return NotifyOutcome::NoParent;
    }
    let (subagent_session, status) = {
        use crate::proto::gait::log::v1::{event, payload, AgentStatus};
        match &c.payload {
            event::Body::Payload(p) => match &p.kind {
                Some(payload::Kind::AgentUpdate(au)) => {
                    let status_str = AgentStatus::try_from(au.status)
                        .map(|s| match s {
                            AgentStatus::Done => "done",
                            AgentStatus::Clarify => "clarify",
                            AgentStatus::Inform => "inform",
                            AgentStatus::Unspecified => "unspecified",
                        })
                        .unwrap_or("unspecified");
                    (Some(au.session_id.clone()), Some(status_str.to_string()))
                }
                _ => (None, None),
            },
            _ => (None, None),
        }
    };
    if let Err(e) = repos.traces.commit_session(
        gait_dir,
        parent_session,
        &build_trace_message(
            &format!("session({}): completion", parent_session),
            &TraceTrailers {
                phase: "completion".to_string(),
                subagent_session,
                status,
                ..Default::default()
            },
        ),
    ) {
        eprintln!("[gait] notify_parent: commit_session failed: {e}");
    }
    drop(lock); // release before the wake re-acquires (the caller spawns it)
    NotifyOutcome::Woke(WakeRequest {
        gait_dir: gait_dir.to_path_buf(),
        parent_session: parent_session.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn completion_carries_idempotency_key_not_derived_from_payload_values() {
        let c1 = Completion {
            payload: crate::proto::events::daemon_update(
                "build".into(),
                0,
                "done",
                "/l".into(),
                "build@12345".into(),
            ),
            idempotency_key: "build@12345".into(),
        };
        let c2 = Completion {
            idempotency_key: "build@67890".into(), // same name, different started_at — a genuinely
            // distinct run of the same-named daemon
            ..c1.clone()
        };
        assert_ne!(
            c1.idempotency_key, c2.idempotency_key,
            "distinct runs must not share a key even with identical payload values"
        );
    }

    fn init_parent(gait_dir: &std::path::Path, id: &str) {
        gait_identity::Keyring::generate().save(gait_dir).unwrap();
        let dir = crate::session::live::worktree_dir(gait_dir, id);
        std::fs::create_dir_all(&dir).unwrap();
    }

    #[test]
    fn notify_parent_signs_the_appended_event() {
        let dir = TempDir::new().unwrap();
        let gait_dir = dir.path();
        init_parent(gait_dir, "parent1");

        let c = Completion {
            payload: crate::proto::events::agent_update(
                "child1".to_string(),
                "done",
                "did the thing".to_string(),
                None,
                "child1-evt1".to_string(),
            ),
            idempotency_key: "child1-evt1".to_string(),
        };
        let repos = crate::git::Repos::production();
        let outcome = notify_parent(gait_dir, &repos, "parent1", &c);
        assert!(matches!(outcome, NotifyOutcome::Woke(_)));

        let tx_path = crate::session::live::log_path(gait_dir, "parent1");
        let events = crate::proto::wire::read_events(&tx_path).unwrap();
        let appended = events.last().unwrap();
        let provenance = appended
            .provenance
            .as_ref()
            .expect("notify_parent must stamp provenance");
        assert!(gait_identity::verify_provenance(appended));
        assert_eq!(provenance.scheme, gait_identity::KeyScheme::Ed25519 as i32);
    }

    fn daemon_completion(name: &str, exit_code: i32, status: &str, log_path: &str) -> Completion {
        Completion {
            payload: crate::proto::events::daemon_update(
                name.to_string(),
                exit_code,
                status,
                log_path.to_string(),
                format!("{name}@1"),
            ),
            idempotency_key: format!("{name}@1"),
        }
    }

    fn agent_completion(from: &str, status: &str, summary: &str) -> Completion {
        Completion {
            payload: crate::proto::events::agent_update(
                from.to_string(),
                status,
                summary.to_string(),
                None,
                format!("{from}-evt1"),
            ),
            idempotency_key: format!("{from}-evt1"),
        }
    }

    #[test]
    fn idle_parent_gets_event_and_wake() {
        let gd = TempDir::new().unwrap();
        init_parent(gd.path(), "sparent");
        let repos = crate::git::Repos::production();
        let c = daemon_completion("build", 0, "done", "/tmp/build.log");
        let outcome = notify_parent(gd.path(), &repos, "sparent", &c);
        assert!(
            matches!(outcome, NotifyOutcome::Woke(_)),
            "expected Woke, got {outcome:?}"
        );
        let log =
            std::fs::read_to_string(crate::session::live::log_path(gd.path(), "sparent")).unwrap();
        assert!(
            log.contains("\"daemonUpdate\""),
            "expected daemonUpdate in log, got: {log}"
        );
    }

    #[test]
    fn busy_parent_is_left_alone() {
        let gd = TempDir::new().unwrap();
        init_parent(gd.path(), "sbusy");
        // Hold the lock to simulate a live turn.
        let _held = crate::session::SessionLock::acquire(&crate::session::live::worktree_dir(
            gd.path(),
            "sbusy",
        ))
        .unwrap();
        let repos = crate::git::Repos::production();
        let c = daemon_completion("x", 0, "done", "/tmp/x.log");
        let outcome = notify_parent(gd.path(), &repos, "sbusy", &c);
        assert!(
            matches!(outcome, NotifyOutcome::ParentBusy),
            "expected ParentBusy, got {outcome:?}"
        );
    }

    #[test]
    fn missing_parent_returns_no_parent() {
        let gd = TempDir::new().unwrap();
        let repos = crate::git::Repos::production();
        let c = daemon_completion("x", 0, "done", "/tmp/x.log");
        let outcome = notify_parent(gd.path(), &repos, "does_not_exist", &c);
        assert!(
            matches!(outcome, NotifyOutcome::NoParent),
            "expected NoParent, got {outcome:?}"
        );
    }

    #[test]
    fn second_notify_for_same_completion_returns_already_surfaced() {
        let gd = TempDir::new().unwrap();
        init_parent(gd.path(), "sdup");
        let repos = crate::git::Repos::production();
        let c = daemon_completion("build", 0, "done", "/tmp/build.log");
        let first = notify_parent(gd.path(), &repos, "sdup", &c);
        assert!(matches!(first, NotifyOutcome::Woke(_)), "got {first:?}");
        // Second call for the same completion must dedup, not double-write/wake.
        let second = notify_parent(gd.path(), &repos, "sdup", &c);
        assert!(
            matches!(second, NotifyOutcome::AlreadySurfaced),
            "expected AlreadySurfaced, got {second:?}"
        );
        let log =
            std::fs::read_to_string(crate::session::live::log_path(gd.path(), "sdup")).unwrap();
        // Exactly one event line: the dedup hit must not append a duplicate.
        let event_lines = log.lines().filter(|l| l.contains("daemonUpdate")).count();
        assert_eq!(event_lines, 1, "expected 1 event line, got: {log}");
    }

    #[test]
    fn agent_completion_commit_has_subagent_session_and_status_trailers() {
        let gd = TempDir::new().unwrap();
        gait_identity::Keyring::generate().save(gd.path()).unwrap();
        crate::git::Repos::production()
            .traces
            .init_workspace(gd.path())
            .unwrap();
        crate::git::Repos::production()
            .traces
            .create_session_worktree(gd.path(), "sparent2")
            .unwrap();
        let repos = crate::git::Repos::production();
        let c = agent_completion("schild99", "done", "finished the task");
        let outcome = notify_parent(gd.path(), &repos, "sparent2", &c);
        assert!(matches!(outcome, NotifyOutcome::Woke(_)), "got {outcome:?}");

        let parent_dir = crate::session::live::worktree_dir(gd.path(), "sparent2");
        let repo = git2::Repository::open(&parent_dir).unwrap();
        let msg = repo
            .head()
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .message()
            .unwrap()
            .to_string();
        assert!(msg.contains("Phase: completion"), "got: {msg}");
        assert!(msg.contains("Subagent-Session: schild99"), "got: {msg}");
        assert!(msg.contains("Status: done"), "got: {msg}");
    }

    #[test]
    fn daemon_completion_commit_has_no_subagent_session_trailer() {
        let gd = TempDir::new().unwrap();
        gait_identity::Keyring::generate().save(gd.path()).unwrap();
        crate::git::Repos::production()
            .traces
            .init_workspace(gd.path())
            .unwrap();
        crate::git::Repos::production()
            .traces
            .create_session_worktree(gd.path(), "sparent3")
            .unwrap();
        let repos = crate::git::Repos::production();
        let c = daemon_completion("build", 0, "done", "/tmp/build.log");
        let outcome = notify_parent(gd.path(), &repos, "sparent3", &c);
        assert!(matches!(outcome, NotifyOutcome::Woke(_)), "got {outcome:?}");

        let parent_dir = crate::session::live::worktree_dir(gd.path(), "sparent3");
        let repo = git2::Repository::open(&parent_dir).unwrap();
        let msg = repo
            .head()
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .message()
            .unwrap()
            .to_string();
        assert!(msg.contains("Phase: completion"), "got: {msg}");
        assert!(
            !msg.contains("Subagent-Session:"),
            "daemon completion must not carry a subagent-session trailer, got: {msg}"
        );
    }
}

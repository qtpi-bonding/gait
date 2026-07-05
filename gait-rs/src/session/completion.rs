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

use std::path::Path;

use crate::proto::gait::log::v1::event::Body;

/// A completion event to surface to a parent session.
#[derive(Debug, Clone)]
pub enum Completion {
    /// A detached daemon has exited.
    Daemon {
        name: String,
        exit_code: i32,
        status: String,
        log_path: String,
    },
    /// A subagent has completed (Part 2 will route subagents through here).
    Agent {
        from: String,
        status: String,
        summary: String,
    },
}

/// The outcome of `notify_parent`. Tells the caller what actually happened
/// so it can decide whether to log a warning (e.g. busy parent) or proceed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotifyOutcome {
    /// Idle parent → event written + commit done + wake spawned.
    Woke,
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
        match self {
            Completion::Daemon {
                name,
                exit_code,
                status,
                log_path,
            } => crate::proto::events::daemon_update(
                name.clone(),
                *exit_code,
                status,
                log_path.clone(),
            ),
            Completion::Agent {
                from,
                status,
                summary,
            } => crate::proto::events::agent_update(from.clone(), status, summary.clone(), None),
        }
    }

    /// True if `ev` is a `*_update` event for this completion. Used for the
    /// speculative dedup check: after we acquire the lock we scan the parent
    /// log for a matching event and return `AlreadySurfaced` instead of
    /// writing + waking a second time.
    fn matches(&self, ev: &crate::proto::gait::log::v1::Event) -> bool {
        use crate::proto::gait::log::v1::payload;
        match (self, crate::proto::event_kind(ev)) {
            (
                Completion::Daemon {
                    name, exit_code, ..
                },
                Some(payload::Kind::DaemonUpdate(du)),
            ) => du.name == *name && du.exit_code == *exit_code,
            (Completion::Agent { from, summary, .. }, Some(payload::Kind::AgentUpdate(au))) => {
                au.session_id == *from && au.summary == *summary
            }
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
    let lock = match crate::turn::SessionLock::acquire(&target_dir) {
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
    if let Err(e) = crate::proto::wire::append(&tx_path, c.body()) {
        // Log the append failure as a diagnostic warn — silently dropping
        // would leave the caller with no signal that the completion was lost.
        eprintln!("[gait] notify_parent: append to {tx_path:?} failed: {e}");
        let _ = repos;
        return NotifyOutcome::NoParent;
    }
    if let Err(e) = repos.traces.commit_session(
        gait_dir,
        parent_session,
        &format!("session({}): completion", parent_session),
    ) {
        eprintln!("[gait] notify_parent: commit_session failed: {e}");
    }
    drop(lock); // release before spawning the wake (the wake re-acquires)
    spawn_wake(gait_dir, parent_session);
    NotifyOutcome::Woke
}

/// Spawn a fresh `gait run --auto` for `session`. Best-effort:
/// a spawn failure is logged as a diagnostic warn — a missed wake must
/// never silently disappear.
///
/// `pub` so the final-drain in `turn::run_turn` can call it directly
/// (the drain runs while holding the session lock, so it writes its own
/// ledger entries and then hands off to a successor turn via this call).
pub fn spawn_wake(gait_dir: &Path, parent_session: &str) {
    let argv = crate::launch::WakeLaunch {
        gait_dir: gait_dir.to_path_buf(),
        parent_session: parent_session.to_string(),
    }
    .to_process_argv();
    let exe = std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("gait"));
    let mut cmd = std::process::Command::new(&exe);
    cmd.args(&argv)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    if let Err(e) = cmd.spawn() {
        // Diagnostic warn: a failed wake is invisible to the parent, so we
        // surface it via the diagnostic module + stderr. The next daemon
        // exit will retry, and the daemons.jsonl registry is the durable
        // fallback until then.
        let rec = crate::diagnostic::DiagRecord::terminal(
            parent_session,
            crate::diagnostic::Phase::Unknown,
            &format!("notify_parent wake spawn failed: {e}"),
        );
        eprintln!("[gait] {}", rec.to_json_line());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn init_parent(gait_dir: &std::path::Path, id: &str) {
        let dir = crate::session::live::worktree_dir(gait_dir, id);
        std::fs::create_dir_all(&dir).unwrap();
    }

    #[test]
    fn idle_parent_gets_event_and_wake() {
        let gd = TempDir::new().unwrap();
        init_parent(gd.path(), "sparent");
        let repos = crate::git::Repos::production();
        let c = Completion::Daemon {
            name: "build".into(),
            exit_code: 0,
            status: "done".into(),
            log_path: "/tmp/build.log".into(),
        };
        let outcome = notify_parent(gd.path(), &repos, "sparent", &c);
        assert!(
            matches!(outcome, NotifyOutcome::Woke),
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
        let _held = crate::turn::SessionLock::acquire(&crate::session::live::worktree_dir(
            gd.path(),
            "sbusy",
        ))
        .unwrap();
        let repos = crate::git::Repos::production();
        let c = Completion::Daemon {
            name: "x".into(),
            exit_code: 0,
            status: "done".into(),
            log_path: "/tmp/x.log".into(),
        };
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
        let c = Completion::Daemon {
            name: "x".into(),
            exit_code: 0,
            status: "done".into(),
            log_path: "/tmp/x.log".into(),
        };
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
        let c = Completion::Daemon {
            name: "build".into(),
            exit_code: 0,
            status: "done".into(),
            log_path: "/tmp/build.log".into(),
        };
        let first = notify_parent(gd.path(), &repos, "sdup", &c);
        assert!(matches!(first, NotifyOutcome::Woke), "got {first:?}");
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
}

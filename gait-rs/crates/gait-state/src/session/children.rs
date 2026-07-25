//! Child-session enumeration for the live-turn agent-completion poll.
//!
//! `children_of(gait_dir, parent)` scans the `.active/` mount directory and
//! returns all sessions whose `SessionStart.assigned_by` matches `parent` and
//! that are no longer running (dead or stale lock file, or no lock at all).
//!
//! Used by `turn::completion_poll::pending_agent_completions` to surface
//! finished child sessions to the parent mid-turn (or in the final drain).

use std::path::Path;

use crate::proto::gait::log::v1::payload;

/// Information about a child session that may be surfaced to its parent.
#[derive(Debug, Clone)]
pub struct ChildInfo {
    /// The child session id.
    pub id: String,
    /// True when the child is no longer running (lock absent or stale).
    pub is_done: bool,
    /// Best-effort summary from the child's last `AssistantText` content.
    pub summary: Option<String>,
    /// The child's own log-tail event id at the moment this was computed —
    /// used as the stable component of a completion's idempotency key
    /// (distinct once the child produces further events; stable across a
    /// retried check of the same child state). See completion.rs's Completion.
    pub last_event_id: String,
}

/// Return all sessions assigned to `parent_session_id` whose lock is no longer
/// held (i.e. done or crashed). Skips sessions without a readable log.
///
/// "Done" = no lock file, OR lock file exists with a dead PID (stale lock).
/// The caller must check `child.is_done` before surfacing.
pub fn children_of(gait_dir: &Path, parent_session_id: &str) -> Vec<ChildInfo> {
    let sessions_dir = gait_dir.join(".active");
    let Ok(entries) = std::fs::read_dir(&sessions_dir) else {
        return vec![];
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let session_dir = entry.path();
        let session_id = session_dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();
        if session_id.is_empty() {
            continue;
        }
        // Live-session log path is nested one level under the worktree mount;
        // re-derive via the helper rather than `session_dir.join("log.jsonl")`
        // (which would be missing the `sessions/<id>/` nesting).
        let log_path = crate::session::live::log_path(gait_dir, &session_id);
        let Ok(events) = crate::session::log::read_events(&log_path) else {
            continue;
        };
        let meta = crate::session::session_meta::from_events(&events, &session_id);
        if meta.assigned_by.as_deref() != Some(parent_session_id) {
            continue;
        }

        // "done" = lock absent or stale (dead PID). The lock lives at the
        // worktree-root mount, not nested under sessions/<id>/.
        let lock_path = session_dir.join("lock");
        let is_done = if !lock_path.exists() {
            true // no lock → not currently running
        } else {
            let pid_str = std::fs::read_to_string(&lock_path).unwrap_or_default();
            let pid: i32 = pid_str.trim().parse().unwrap_or(-1);
            if pid > 0 {
                // SAFETY: kill(pid, 0) sends no signal; only checks if the process exists.
                unsafe { libc::kill(pid, 0) != 0 }
            } else {
                true // malformed lock → treat as stale
            }
        };

        // Summary: last non-empty AssistantText.
        let summary = events.iter().rev().find_map(|ev| {
            if let Some(payload::Kind::AssistantText(a)) = crate::proto::event_kind(ev) {
                let t = a.content.trim();
                if !t.is_empty() {
                    return Some(t.to_string());
                }
            }
            None
        });

        let last_event_id = events.last().map(|e| e.id.clone()).unwrap_or_default();

        out.push(ChildInfo {
            id: session_id,
            is_done,
            summary,
            last_event_id,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::log::{init_session, InitSessionOpts};
    use tempfile::TempDir;

    fn make_session(gait_dir: &Path, id: &str, assigned_by: Option<&str>) {
        let log_path = crate::session::live::log_path(gait_dir, id);
        std::fs::create_dir_all(log_path.parent().unwrap()).unwrap();
        init_session(InitSessionOpts {
            log_path,
            id: id.to_string(),
            model: String::new(),
            codebase_hash: String::new(),
            codebase_dirty: false,
            assigned_by: assigned_by.map(|s| s.to_string()),
            codebase_path: None,
            notify_done: false,
        })
        .unwrap();
    }

    #[test]
    fn children_of_returns_assigned_sessions() {
        let dir = TempDir::new().unwrap();
        make_session(dir.path(), "schild01", Some("sparent"));
        make_session(dir.path(), "sother00", Some("sparent2")); // different parent
        make_session(dir.path(), "sunowned", None); // no parent

        let children = children_of(dir.path(), "sparent");
        let ids: Vec<&str> = children.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, vec!["schild01"], "only schild01 is a child of sparent");
        assert!(children[0].is_done, "no lock file → is_done = true");
    }

    #[test]
    fn children_of_empty_when_no_sessions() {
        let dir = TempDir::new().unwrap();
        // .active/ dir doesn't even exist yet
        let children = children_of(dir.path(), "sparent");
        assert!(children.is_empty());
    }

    #[test]
    fn child_with_stale_lock_is_done() {
        let dir = TempDir::new().unwrap();
        make_session(dir.path(), "schild02", Some("sparent"));
        // Write a lock file with a PID that is definitely dead (PID 0 is not a real process).
        let lock_path = dir.path().join(".active/schild02/lock");
        std::fs::write(&lock_path, "999999999").unwrap(); // absurdly high PID → dead
        let children = children_of(dir.path(), "sparent");
        assert_eq!(children.len(), 1);
        assert!(
            children[0].is_done,
            "stale lock (dead PID) → is_done = true"
        );
    }
}

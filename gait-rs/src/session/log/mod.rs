use std::path::{Path, PathBuf};

use super::SessionError;
use crate::proto::gait::log::v1 as pb;
use crate::proto::gait::log::v1::{payload, Event};
/// The session's append-only Log file. (The file is the Log, not the Transcript —
/// the Transcript is the ActiveMessages projection derived from it.)
pub fn log_path(gait_dir: &std::path::Path, session_id: &str) -> std::path::PathBuf {
    gait_dir.join("sessions").join(session_id).join("log.jsonl")
}

// ── JSONL helpers ─────────────────────────────────────────────────────────────

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub fn read_events(path: &Path) -> Result<Vec<Event>, SessionError> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let text = std::fs::read_to_string(path)?;
    let mut events = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        events.push(serde_json::from_str::<Event>(line)?);
    }
    Ok(events)
}

pub fn last_event(path: &Path) -> Result<Option<Event>, SessionError> {
    Ok(read_events(path)?.pop())
}

pub fn count_turn_complete(events: &[Event]) -> usize {
    events
        .iter()
        .filter(|e| {
            matches!(
                crate::proto::event_kind(e),
                Some(payload::Kind::TurnComplete(_))
            )
        })
        .count()
}

pub fn last_compaction_index(events: &[Event]) -> Option<usize> {
    events
        .iter()
        .rposition(|e| matches!(crate::proto::event_kind(e), Some(payload::Kind::Summary(_))))
}

pub struct InitSessionOpts {
    pub log_path: PathBuf,
    pub id: String,
    pub model: String,
    pub codebase_hash: String,
    pub codebase_dirty: bool,
    pub assigned_by: Option<String>,
    pub codebase_path: Option<String>,
    /// When true, the scheduler will send a `Notify` to `assigned_by` when the
    /// child session completes. (Task 6: peer agents notify; blocking subagents don't.)
    pub notify_done: bool,
}

// ── Session state introspection ─────────────────────────────────────────────

/// Snapshot of where a session is right now, derived from the transcript tail.
/// Used by external runners (CLI, REPL) to detect pending approval/clarify
/// requests so they can resume or reject after a restart.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SessionState {
    /// No pending request — runner should start a new turn.
    Idle,
    /// A tool permission request is awaiting a decision.
    AwaitingPermission {
        items: Vec<crate::proto::gait::log::v1::PermissionItem>,
    },
    /// A clarify request is awaiting user answers.
    AwaitingClarify {
        items: Vec<crate::proto::gait::log::v1::ClarifyItem>,
    },
    /// An inform request is awaiting user acknowledgement.
    AwaitingInform {
        items: Vec<crate::proto::gait::log::v1::InformItem>,
    },
    /// A tool access unlock request is awaiting a decision.
    AwaitingUnlock { items: Vec<pb::UnlockItem> },
}

/// Walk the transcript backwards to determine the current session state.
///
/// A reply event encountered before its corresponding request means the
/// request is already resolved (e.g. resumed after a previous run wrote
/// the reply). The first unresolved request encountered wins.
pub fn session_state(tx_path: &std::path::Path) -> Result<SessionState, SessionError> {
    let events = read_events(tx_path)?;
    for event in events.iter().rev() {
        match crate::proto::event_kind(event) {
            Some(payload::Kind::Permissions(p)) => {
                return Ok(SessionState::AwaitingPermission {
                    items: p.items.clone(),
                });
            }
            Some(payload::Kind::PermissionsReply(_)) => {
                return Ok(SessionState::Idle);
            }
            Some(payload::Kind::Clarify(c)) => {
                return Ok(SessionState::AwaitingClarify {
                    items: c.items.clone(),
                });
            }
            Some(payload::Kind::ClarifyReply(_)) => {
                return Ok(SessionState::Idle);
            }
            Some(payload::Kind::Inform(i)) => {
                return Ok(SessionState::AwaitingInform {
                    items: i.items.clone(),
                });
            }
            Some(payload::Kind::InformReply(_)) => {
                return Ok(SessionState::Idle);
            }
            Some(payload::Kind::Unlock(u)) => {
                return Ok(SessionState::AwaitingUnlock {
                    items: u.items.clone(),
                });
            }
            Some(payload::Kind::UnlockReply(_)) => {
                return Ok(SessionState::Idle);
            }
            Some(payload::Kind::TurnComplete(_)) => {
                return Ok(SessionState::Idle);
            }
            _ => continue,
        }
    }
    Ok(SessionState::Idle)
}

/// Append a `PermissionsReply` event to the transcript and stdout.
///
/// `from` identifies the source of the decisions (e.g. "user", "auto-deny").
/// Each `PermissionsReplyItem.id` corresponds to a `PermissionItem.id` in
/// the preceding `Permissions` request, and `decision` is "approve" | "deny" | "instruct".
pub fn write_permissions_reply(
    tx_path: &std::path::Path,
    from: &str,
    items: Vec<pb::PermissionsReplyItem>,
) -> Result<(), SessionError> {
    let body = crate::proto::events::permissions_reply(from.to_string(), &items);
    let prev = last_event(tx_path)?;
    let ev = crate::proto::envelope::mint(body, prev.as_ref(), 0, 0);
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(tx_path)?;
    let json = serde_json::to_string(&ev)?;
    use std::io::Write;
    writeln!(file, "{}", json)?;
    Ok(())
}

/// Append an `UnlockReply` event to the transcript.
///
/// `from` identifies the source of the decision (e.g. "user").
/// Each `UnlockReplyItem.id` corresponds to an `UnlockItem.id` in
/// the preceding `Unlock` request.
pub fn write_unlock_reply(
    tx_path: &std::path::Path,
    from: &str,
    items: Vec<pb::UnlockReplyItem>,
) -> Result<(), SessionError> {
    let body = crate::proto::events::unlock_reply(from.to_string(), &items);
    let prev = last_event(tx_path)?;
    let ev = crate::proto::envelope::mint(body, prev.as_ref(), 0, 0);
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(tx_path)?;
    let json = serde_json::to_string(&ev)?;
    use std::io::Write;
    writeln!(file, "{}", json)?;
    Ok(())
}

pub fn init_session(opts: InitSessionOpts) -> Result<(), SessionError> {
    // SessionStart is identity-only; model/codebase_hash/codebase_dirty on `opts`
    // remain unused here (pre-existing). codebase_path + assigned_by ARE persisted.
    let body = crate::proto::events::session_start(opts.id, opts.assigned_by, opts.codebase_path);
    let prev = last_event(&opts.log_path)?;
    let ev = crate::proto::envelope::mint(body, prev.as_ref(), 0, 0);
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&opts.log_path)?;
    let json = serde_json::to_string(&ev)?;
    use std::io::Write;
    writeln!(file, "{}", json)?;
    Ok(())
}

#[cfg(test)]
mod tests;

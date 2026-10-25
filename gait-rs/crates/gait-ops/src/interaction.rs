//! Interaction verbs: `reply` — external (outside a running turn) answer to a
//! pending `Clarify` or `Inform` request. Symmetric with `permission::allow`/
//! `deny`/`unlock`, which already cover the `Permissions` request kind; this
//! module covers the two request kinds `gait reply` was scaffolded for but
//! never implemented (`crates/gait-cli/src/cli/session.rs::cmd_reply` was a
//! stub printing "reply not yet implemented").
use crate::OpError;
use coded_error::ResultExt;
use std::path::Path;

/// Which pending-interaction kind `reply` answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplyKind {
    Clarify,
    Inform,
}

/// Outcome of `reply`: which kind was answered and the id of the item.
#[derive(Debug)]
pub struct ReplyOutcome {
    pub kind: ReplyKind,
    pub id: String,
}

/// Answer a session's single pending `Clarify` or `Inform` request with
/// `message`, appending the matching `ClarifyReply`/`InformReply` event.
///
/// v1 scope: requires the session have EXACTLY ONE pending item. A batch of
/// several items (e.g. an agent that asked two clarifying questions in one
/// `Clarify` request) is not addressable per-item through this single-message
/// CLI surface yet — `Err(OpError::Conflict(..))` lists the pending ids so the
/// caller knows why, rather than silently answering only one and leaving the
/// rest to be auto-denied at resume (mirrors the "must cover every id"
/// invariant `permission::reply` already enforces for the same reason).
///
/// Errors if the session has no pending Clarify/Inform (including if it's
/// pending a `Permission` instead — that's `gait allow`/`deny`/`unlock`'s
/// job, not this one).
pub fn reply(gait_dir: &Path, session_id: &str, message: String) -> anyhow::Result<ReplyOutcome> {
    use gait_state::session::log::{
        session_state, write_clarify_reply, write_inform_reply, SessionState,
    };

    let tx_path = gait_state::session::live::log_path(gait_dir, session_id);
    let state = session_state(&tx_path).coded()?;

    match state {
        SessionState::AwaitingClarify { items } => {
            if items.is_empty() {
                return Err(coded_error::into_anyhow(OpError::NotFound(format!(
                    "no pending clarify for session {session_id}"
                ))));
            }
            if items.len() > 1 {
                let ids: Vec<String> = items.iter().map(|it| it.id.clone()).collect();
                return Err(coded_error::into_anyhow(OpError::Conflict(format!(
                    "session {session_id} has {} pending clarify items ({}); `gait reply` only answers a single pending item",
                    items.len(),
                    ids.join(", ")
                ))));
            }
            let id = items[0].id.clone();
            write_clarify_reply(
                &tx_path,
                vec![gait_state::proto::gait::log::v1::ClarifyReplyItem {
                    id: id.clone(),
                    content: message,
                    reasoning: None,
                }],
            ).coded()?;
            Ok(ReplyOutcome {
                kind: ReplyKind::Clarify,
                id,
            })
        }
        SessionState::AwaitingInform { items } => {
            if items.is_empty() {
                return Err(coded_error::into_anyhow(OpError::NotFound(format!(
                    "no pending inform for session {session_id}"
                ))));
            }
            if items.len() > 1 {
                let ids: Vec<String> = items.iter().map(|it| it.id.clone()).collect();
                return Err(coded_error::into_anyhow(OpError::Conflict(format!(
                    "session {session_id} has {} pending inform items ({}); `gait reply` only answers a single pending item",
                    items.len(),
                    ids.join(", ")
                ))));
            }
            let id = items[0].id.clone();
            write_inform_reply(
                &tx_path,
                vec![gait_state::proto::gait::log::v1::InformReplyItem {
                    id: id.clone(),
                    content: message,
                }],
            )
            .coded()?;
            Ok(ReplyOutcome {
                kind: ReplyKind::Inform,
                id,
            })
        }
        SessionState::AwaitingPermission { .. } => Err(coded_error::into_anyhow(OpError::Conflict(format!(
            "session {session_id} is awaiting a permission decision, not clarify/inform — use `gait allow`/`gait deny`/`gait unlock` instead"
        )))),
        _ => Err(coded_error::into_anyhow(OpError::NotFound(format!(
            "no pending clarify/inform for session {session_id}"
        )))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_session(ws: &Path, id: &str) -> std::path::PathBuf {
        let tx_path = gait_state::session::live::log_path(ws, id);
        std::fs::create_dir_all(tx_path.parent().unwrap()).unwrap();
        tx_path
    }

    fn seed_clarify(tx_path: &Path, items: &[gait_state::proto::gait::log::v1::ClarifyItem]) {
        let body = gait_state::proto::events::clarify(items);
        gait_state::proto::wire::append(tx_path, body).unwrap();
    }

    fn seed_inform(tx_path: &Path, items: &[gait_state::proto::gait::log::v1::InformItem]) {
        let body = gait_state::proto::events::inform(items);
        gait_state::proto::wire::append(tx_path, body).unwrap();
    }

    #[test]
    fn reply_answers_pending_clarify() {
        let ws = TempDir::new().unwrap();
        let tx = make_session(ws.path(), "sreply01");
        seed_clarify(
            &tx,
            &[gait_state::proto::gait::log::v1::ClarifyItem {
                id: "c1".into(),
                content: "which db?".into(),
                why: "ambiguous config".into(),
                kind: gait_state::proto::gait::log::v1::ClarifyKind::Is as i32,
                options: vec![],
            }],
        );
        let out = reply(ws.path(), "sreply01", "postgres".into()).unwrap();
        assert_eq!(out.kind, ReplyKind::Clarify);
        assert_eq!(out.id, "c1");
        let state = gait_state::session::log::session_state(&tx).unwrap();
        assert!(matches!(
            state,
            gait_state::session::log::SessionState::Idle
        ));
    }

    #[test]
    fn reply_answers_pending_inform() {
        let ws = TempDir::new().unwrap();
        let tx = make_session(ws.path(), "sreply02");
        seed_inform(
            &tx,
            &[gait_state::proto::gait::log::v1::InformItem {
                id: "i1".into(),
                content: "deleted 3 stale branches".into(),
                context: None,
            }],
        );
        let out = reply(ws.path(), "sreply02", "ok".into()).unwrap();
        assert_eq!(out.kind, ReplyKind::Inform);
        assert_eq!(out.id, "i1");
    }

    #[test]
    fn reply_errors_when_nothing_pending() {
        let ws = TempDir::new().unwrap();
        let _tx = make_session(ws.path(), "sreply03");
        let err = reply(ws.path(), "sreply03", "hi".into()).unwrap_err();
        assert!(matches!(coded_error::downcast_ref::<OpError>(&err), Some(OpError::NotFound(_))));
    }

    #[test]
    fn reply_errors_on_multiple_pending_clarify_items() {
        let ws = TempDir::new().unwrap();
        let tx = make_session(ws.path(), "sreply04");
        seed_clarify(
            &tx,
            &[
                gait_state::proto::gait::log::v1::ClarifyItem {
                    id: "c1".into(),
                    content: "which db?".into(),
                    why: "ambiguous".into(),
                    kind: gait_state::proto::gait::log::v1::ClarifyKind::Is as i32,
                    options: vec![],
                },
                gait_state::proto::gait::log::v1::ClarifyItem {
                    id: "c2".into(),
                    content: "which region?".into(),
                    why: "ambiguous".into(),
                    kind: gait_state::proto::gait::log::v1::ClarifyKind::Is as i32,
                    options: vec![],
                },
            ],
        );
        let err = reply(ws.path(), "sreply04", "postgres".into()).unwrap_err();
        match coded_error::downcast_ref::<OpError>(&err) {
            Some(OpError::Conflict(msg)) => {
                assert!(msg.contains("c1"));
                assert!(msg.contains("c2"));
            }
            _ => panic!("expected Conflict, got {err:?}"),
        }
    }

    #[test]
    fn reply_errors_when_awaiting_permission_instead() {
        let ws = TempDir::new().unwrap();
        let tx = make_session(ws.path(), "sreply05");
        let body = gait_state::proto::events::permissions(&[
            gait_state::proto::gait::log::v1::PermissionItem {
                id: "p1".into(),
                tool_name: "shell".into(),
                cmd: "rm -rf /".into(),
                patterns: vec![],
                reason: String::new(),
                preview: None,
                tool_args: None,
            },
        ]);
        gait_state::proto::wire::append(&tx, body).unwrap();
        let err = reply(ws.path(), "sreply05", "hi".into()).unwrap_err();
        match coded_error::downcast_ref::<OpError>(&err) {
            Some(OpError::Conflict(msg)) => assert!(msg.contains("gait allow")),
            _ => panic!("expected Conflict, got {err:?}"),
        }
    }
}

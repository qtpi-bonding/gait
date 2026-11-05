//! `_meta` keys the session-notices contract adds to ACP messages: the lib
//! (agent-transport-core) tells us who cancelled a turn and why, and we tell
//! it that we understand (`SESSION_NOTICES_KEY` in the initialize response).

use gait_state::proto::events::CancelCause;
use gait_state::proto::gait::log::v1::CancelActorKind;
use std::sync::{Arc, Mutex};

pub const SESSION_NOTICES_KEY: &str = "qtpi.app/sessionNotices";
pub const ACTOR_KEY: &str = "qtpi.app/actor";
pub const NOTICE_KEY: &str = "qtpi.app/notice";

/// One turn's cancel cause. The first cause stored wins.
pub type CauseSlot = Arc<Mutex<Option<CancelCause>>>;

pub fn record_cause(slot: &CauseSlot, cause: Option<CancelCause>) {
    let Some(cause) = cause else { return };
    let mut guard = slot.lock().unwrap();
    if guard.is_none() {
        *guard = Some(cause);
    }
}

/// Reads who cancelled and why from a message's `_meta`. `None` when there is
/// no usable actor. A missing reason is filled in from `qtpi.app/notice` when
/// the lib classified the cancel as a notice.
pub fn cancel_cause(
    meta: Option<&serde_json::Map<String, serde_json::Value>>,
) -> Option<CancelCause> {
    let meta = meta?;
    let actor = meta.get(ACTOR_KEY)?.as_object()?;
    let kind = match actor.get("kind")?.as_str()? {
        "user" => CancelActorKind::User,
        "orchestrator" => CancelActorKind::Orchestrator,
        "policy" => CancelActorKind::Policy,
        "system" => CancelActorKind::System,
        _ => return None,
    };
    let id = actor.get("id").and_then(|v| v.as_str()).unwrap_or_default().to_string();
    let reason = actor
        .get("reason")
        .and_then(|v| v.as_str())
        .map(String::from)
        .or_else(|| notice_reason(meta.get(NOTICE_KEY)?.as_object()?));
    Some(CancelCause { kind, id, reason })
}

fn notice_reason(notice: &serde_json::Map<String, serde_json::Value>) -> Option<String> {
    let tool = notice.get("tool").and_then(|v| v.as_str()).unwrap_or("a tool");
    match notice.get("kind")?.as_str()? {
        "permission_denied_by_policy" => Some(format!("a rule refused permission for `{tool}`")),
        "permission_denied_unattended" => {
            Some(format!("nobody was available to approve `{tool}`"))
        }
        "elicitation_unanswered" => Some("nobody was available to answer the question".into()),
        _ => None,
    }
}

/// Appends the turn's `CancellationSignal` after an ACP cancel. The brain is
/// killed by the cascade before it can finalize, so nothing else records one.
/// Skipped when this turn's log already holds one, so there is never more than one.
pub fn record_cancellation(
    gait_dir: &std::path::Path,
    session_id: &str,
    cause: Option<&CancelCause>,
) -> anyhow::Result<()> {
    let path = gait_core::session::live::log_path(gait_dir, session_id);
    // Any signal since the last user trigger counts: finalize may have appended
    // bookkeeping after one the brain wrote before it died.
    let already = gait_core::session::log::read_events(&path)
        .ok()
        .is_some_and(|events| {
            use gait_core::proto::gait::log::v1::payload::Kind;
            for e in events.iter().rev() {
                match gait_core::proto::event_kind(e) {
                    Some(Kind::CancellationSignal(_)) => return true,
                    Some(Kind::UserText(_) | Kind::IncomingMessage(_)) => return false,
                    _ => {}
                }
            }
            false
        });
    if already {
        return Ok(());
    }
    gait_core::embed::record_session_update(
        &gait_core::git::Repos::production(),
        gait_dir,
        session_id,
        vec![gait_state::proto::events::cancellation_signal(cause)],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn meta(v: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn absent_key_or_meta_is_none() {
        assert_eq!(cancel_cause(None), None);
        assert_eq!(cancel_cause(Some(&meta(json!({"other": 1})))), None);
    }

    #[test]
    fn malformed_actor_is_none() {
        assert_eq!(cancel_cause(Some(&meta(json!({ACTOR_KEY: "orchestrator"})))), None);
        assert_eq!(cancel_cause(Some(&meta(json!({ACTOR_KEY: {"id": "x"}})))), None);
    }

    #[test]
    fn unknown_actor_kind_is_none() {
        let m = meta(json!({ACTOR_KEY: {"kind": "robot", "id": "x"}}));
        assert_eq!(cancel_cause(Some(&m)), None);
    }

    #[test]
    fn actor_with_reason_is_read() {
        let m = meta(json!({ACTOR_KEY: {"kind": "orchestrator", "id": "svc", "reason": "Action 7: x"}}));
        let c = cancel_cause(Some(&m)).unwrap();
        assert_eq!(c.kind, CancelActorKind::Orchestrator);
        assert_eq!(c.id, "svc");
        assert_eq!(c.reason.as_deref(), Some("Action 7: x"));
    }

    #[test]
    fn missing_reason_is_filled_from_the_notice() {
        let fill = |notice: serde_json::Value, kind: &str| {
            let m = meta(json!({ACTOR_KEY: {"kind": kind, "id": "lib"}, NOTICE_KEY: notice}));
            cancel_cause(Some(&m)).unwrap().reason
        };
        assert_eq!(
            fill(json!({"kind": "permission_denied_by_policy", "tool": "Bash"}), "policy").as_deref(),
            Some("a rule refused permission for `Bash`")
        );
        assert_eq!(
            fill(json!({"kind": "permission_denied_unattended", "tool": "Bash"}), "system").as_deref(),
            Some("nobody was available to approve `Bash`")
        );
        assert_eq!(
            fill(json!({"kind": "elicitation_unanswered"}), "system").as_deref(),
            Some("nobody was available to answer the question")
        );
        assert_eq!(fill(json!({"kind": "cancelled"}), "orchestrator"), None);
    }

    #[test]
    fn first_cause_wins() {
        let slot = CauseSlot::default();
        let mk = |id: &str| CancelCause { kind: CancelActorKind::System, id: id.into(), reason: None };
        record_cause(&slot, Some(mk("a")));
        record_cause(&slot, Some(mk("b")));
        assert_eq!(slot.lock().unwrap().as_ref().unwrap().id, "a");
    }
}

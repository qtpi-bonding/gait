//! Scheduler — the policy half of coordination. Given a turn that just ended,
//! it decides *what should happen next* and returns `Action` descriptions. The
//! `HarnessBackend` (mechanism) executes them. External orchestrators (grorg,
//! archebuild) supply their own `Scheduler`; `NativeScheduler` is the standalone
//! default (resume the assigner, if any).

use crate::turn::TurnResult;

pub struct TurnEnded<'a> {
    pub session_id: &'a str,
    pub assigned_by: Option<&'a str>,
    pub result: &'a TurnResult,
    /// When false, suppress `NotifyAndResume` — result delivered via Feedback instead.
    /// Set from `SessionMeta.notify_done` (which mirrors `SessionStart.notify_done`).
    pub notify_done: bool,
    /// The ending session's own log-tail event id, if any — sourced from the
    /// log mirror. Used as the stable component of the resulting
    /// `Action::NotifyAndResume`'s idempotency_key (see Completion,
    /// gait-state::session::completion).
    pub last_event_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Notify `target_session` of `from_session`'s result, then resume it.
    NotifyAndResume {
        target_session: String,
        from_session: String,
        status: String,
        summary: String,
        idempotency_key: String,
    },
}

pub trait Scheduler: Send + Sync {
    fn next(&self, ended: &TurnEnded) -> Vec<Action>;
}

/// Map a TurnResult to the (status, summary) reported up to an assigner.
fn summarize(result: &TurnResult) -> (String, String) {
    match result {
        TurnResult::Done { message, .. } => {
            ("done".to_string(), message.chars().take(500).collect())
        }
        TurnResult::Permissions => (
            "permissions".to_string(),
            "needs permissions approval".to_string(),
        ),
        TurnResult::Clarify { .. } => (
            "clarify".to_string(),
            "paused for clarification".to_string(),
        ),
        TurnResult::Inform { .. } => ("inform".to_string(), "paused to inform user".to_string()),
        TurnResult::SandboxUnlock { .. } => (
            "unlock".to_string(),
            "paused for sandbox unlock".to_string(),
        ),
        TurnResult::Cancelled => (
            "cancelled".to_string(),
            "turn cancelled by user".to_string(),
        ),
        TurnResult::Failed { ended, message } => (ended.clone(), message.clone()),
    }
}

pub struct NativeScheduler;

impl Scheduler for NativeScheduler {
    fn next(&self, ended: &TurnEnded) -> Vec<Action> {
        let Some(assigner) = ended.assigned_by else {
            return vec![];
        };
        if !ended.notify_done {
            return vec![];
        }
        let (status, summary) = summarize(ended.result);
        let idempotency_key = ended
            .last_event_id
            .clone()
            .unwrap_or_else(|| format!("{}#no-events", ended.session_id));
        vec![Action::NotifyAndResume {
            target_session: assigner.to_string(),
            from_session: ended.session_id.to_string(),
            status,
            summary,
            idempotency_key,
        }]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_assigner_yields_no_actions() {
        let result = TurnResult::Done {
            message: "ok".into(),
            result: None,
            changes: None,
            implicit: false,
        };
        let ended = TurnEnded {
            session_id: "s1",
            assigned_by: None,
            result: &result,
            notify_done: true,
            last_event_id: None,
        };
        assert!(NativeScheduler.next(&ended).is_empty());
    }

    #[test]
    fn assigner_yields_notify_and_resume_with_done_status() {
        let result = TurnResult::Done {
            message: "built the parser".into(),
            result: None,
            changes: None,
            implicit: false,
        };
        let ended = TurnEnded {
            session_id: "child",
            assigned_by: Some("parent"),
            result: &result,
            notify_done: true,
            last_event_id: Some("evt1".into()),
        };
        let actions = NativeScheduler.next(&ended);
        assert_eq!(
            actions,
            vec![Action::NotifyAndResume {
                target_session: "parent".into(),
                from_session: "child".into(),
                status: "done".into(),
                summary: "built the parser".into(),
                idempotency_key: "evt1".into(),
            }]
        );
    }

    #[test]
    fn pause_results_map_to_their_status() {
        let result = TurnResult::Permissions;
        let ended = TurnEnded {
            session_id: "child",
            assigned_by: Some("parent"),
            result: &result,
            notify_done: true,
            last_event_id: None,
        };
        let actions = NativeScheduler.next(&ended);
        match &actions[0] {
            Action::NotifyAndResume { status, .. } => assert_eq!(status, "permissions"),
        }
    }

    #[test]
    fn blocking_subagent_yields_no_action() {
        let result = TurnResult::Done {
            message: "ok".into(),
            result: None,
            changes: None,
            implicit: false,
        };
        let ended = TurnEnded {
            session_id: "child",
            assigned_by: Some("parent"),
            result: &result,
            notify_done: false,
            last_event_id: None,
        };
        assert!(NativeScheduler.next(&ended).is_empty());
    }

    #[test]
    fn peer_with_notify_done_true_yields_notify_and_resume() {
        let result = TurnResult::Done {
            message: "peer done".into(),
            result: None,
            changes: None,
            implicit: false,
        };
        let ended = TurnEnded {
            session_id: "peer",
            assigned_by: Some("assigner"),
            result: &result,
            notify_done: true,
            last_event_id: None,
        };
        let actions = NativeScheduler.next(&ended);
        assert_eq!(actions.len(), 1);
        match &actions[0] {
            Action::NotifyAndResume {
                target_session,
                from_session,
                status,
                ..
            } => {
                assert_eq!(target_session, "assigner");
                assert_eq!(from_session, "peer");
                assert_eq!(status, "done");
            }
        }
    }

    #[test]
    fn unlock_maps_to_unlock_status() {
        let result = TurnResult::SandboxUnlock { items: vec![] };
        let (status, summary) = summarize(&result);
        assert_eq!(status, "unlock");
        assert_eq!(summary, "paused for sandbox unlock");
    }
}

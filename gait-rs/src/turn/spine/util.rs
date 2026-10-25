//! Small pure helpers used by the relay loop and its handlers.

/// Remove `--task <value>` from an arg slice (brain runs Resume mode).
pub fn strip_task_from_args(args: &mut Vec<String>) {
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--task" {
            args.remove(i);
            if i < args.len() {
                args.remove(i);
            }
        } else {
            i += 1;
        }
    }
}

/// Convert the brain-internal outcome into the transient Nerve draft. Durable
/// ids, step membership, timing, checkpoints and summaries are spine-owned.
pub(crate) fn finalized_turn(
    result: &crate::turn::TurnResult,
) -> crate::proto::gait::sandbox::v1::FinalizedTurn {
    use crate::proto::gait::sandbox::v1::FinalizedTurnOutcome as Outcome;
    use crate::turn::TurnResult;

    let (outcome, message, value, failure_reason, error_code, error_class) = match result {
        TurnResult::Done {
            message,
            result,
            implicit,
            ..
        } => (
            if *implicit {
                Outcome::TextDone
            } else {
                Outcome::StructuredDone
            },
            message.clone(),
            crate::proto::events::to_value_opt(result),
            String::new(),
            None,
            None,
        ),
        TurnResult::Clarify { .. } => (Outcome::Clarify, String::new(), None, String::new(), None, None),
        TurnResult::Inform { .. } => (Outcome::Inform, String::new(), None, String::new(), None, None),
        TurnResult::Permissions => (Outcome::Permissions, String::new(), None, String::new(), None, None),
        TurnResult::SandboxUnlock { .. } => {
            (Outcome::SandboxUnlock, String::new(), None, String::new(), None, None)
        }
        TurnResult::Cancelled => (Outcome::Cancelled, String::new(), None, String::new(), None, None),
        TurnResult::Failed { ended, message, code, class } => {
            (Outcome::Failed, message.clone(), None, ended.clone(), code.clone(), class.clone())
        }
    };
    crate::proto::gait::sandbox::v1::FinalizedTurn {
        outcome: outcome as i32,
        message,
        result: value,
        failure_reason,
        error_code,
        error_class,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::gait::sandbox::v1::FinalizedTurnOutcome as Outcome;

    #[test]
    fn preserves_structured_done_across_brain_spine_boundary() {
        let draft = finalized_turn(&crate::turn::TurnResult::Done {
            message: "ok".into(),
            result: Some(serde_json::json!({"answer": 42})),
            changes: None,
            implicit: false,
        });
        assert_eq!(draft.outcome, Outcome::StructuredDone as i32);
        assert_eq!(draft.message, "ok");
        assert!(draft.result.is_some());
    }

    #[test]
    fn preserves_failure_reason_across_brain_spine_boundary() {
        let draft = finalized_turn(&crate::turn::TurnResult::Failed {
            ended: "max_steps".into(),
            message: "limit reached".into(),
            code: None,
            class: None,
        });
        assert_eq!(draft.outcome, Outcome::Failed as i32);
        assert_eq!(draft.failure_reason, "max_steps");
        assert_eq!(draft.message, "limit reached");
    }
}

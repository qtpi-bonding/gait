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

/// Reconstruct a `TurnResult` from the JSON that `BrainSink` encodes in
/// `Terminated.run_result_json` (a serialized `RunResult` proto).
///
/// Every oneof variant is faithfully reconstructed. `Permissions`/`Clarify`/
/// `Inform` previously fell through to a bare `Done` (review C1), flattening
/// those outcomes to `{"done":{}}` on the headless path — Task 3 rewrites
/// this to preserve the discriminant end-to-end. `implicit` is recovered
/// from the `ended` sibling string (set by `run_result_proto`); the
/// `Failed` arm is wired in Task 4 and is added here as a guarded fallback
/// against `ended` being non-empty with no oneof set.
pub(crate) fn turn_result_from_json(json: &str) -> crate::turn::TurnResult {
    use crate::proto::gait::log::v1::{run_result, RunResult};
    let rr: RunResult = serde_json::from_str(json).unwrap_or_default();
    match rr.result {
        Some(run_result::Result::Cancelled(_)) => crate::turn::TurnResult::Cancelled,
        Some(run_result::Result::Unlock(u)) => crate::turn::TurnResult::Unlock { items: u.items },
        Some(run_result::Result::Permissions(_)) => crate::turn::TurnResult::Permissions,
        Some(run_result::Result::Clarify(c)) => crate::turn::TurnResult::Clarify { items: c.items },
        Some(run_result::Result::Inform(i)) => crate::turn::TurnResult::Inform { items: i.items },
        Some(run_result::Result::Done(d)) => crate::turn::TurnResult::Done {
            message: d.message,
            result: d
                .result
                .as_ref()
                .and_then(|v| serde_json::to_value(v).ok())
                .filter(|v| !v.is_null()),
            changes: d.changes,
            implicit: rr.ended == "implicit_done",
        },
        // Failure: no oneof, only `ended` (wired in Task 4). `rr.result` is moved
        // by the match above, so key off the sibling field.
        None if !rr.ended.is_empty() => crate::turn::TurnResult::Failed {
            ended: rr.ended.clone(),
            message: String::new(), // detail already reached the user via the brain's stderr diagnostic
        },
        _ => crate::turn::TurnResult::Done {
            message: String::new(),
            result: None,
            changes: None,
            implicit: true,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::port::output::run_result_proto;
    use crate::proto::gait::log::v1::{ClarifyItem, InformItem};
    use crate::turn::TurnResult;

    /// Per-variant round-trip regression guard for review C1: every variant
    /// must survive `run_result_proto` -> `serde_json` -> `turn_result_from_json`
    /// with the same `ended` discriminant, and the success arms must NOT
    /// flatten to a bare `{"done":{}}`.
    #[test]
    fn round_trip_every_variant_preserves_ended() {
        let cases: Vec<(&'static str, TurnResult)> = vec![
            (
                "implicit_done",
                TurnResult::Done {
                    message: "".into(),
                    result: None,
                    changes: None,
                    implicit: true,
                },
            ),
            (
                "structured_done",
                TurnResult::Done {
                    message: "ok".into(),
                    result: Some(serde_json::json!({"k": "v"})),
                    changes: None,
                    implicit: false,
                },
            ),
            ("permissions", TurnResult::Permissions),
            (
                "clarify",
                TurnResult::Clarify {
                    items: vec![ClarifyItem {
                        id: "i1".into(),
                        content: "?".into(),
                        why: "w".into(),
                        kind: 0,
                        options: vec![],
                    }],
                },
            ),
            (
                "inform",
                TurnResult::Inform {
                    items: vec![InformItem {
                        id: "i1".into(),
                        content: "fyi".into(),
                        context: None,
                    }],
                },
            ),
            ("unlock", TurnResult::Unlock { items: vec![] }),
            ("cancelled", TurnResult::Cancelled),
        ];

        for (expected_ended, result) in cases {
            let proto = run_result_proto(&result);
            assert_eq!(
                proto.ended, expected_ended,
                "run_result_proto must set ended={expected_ended} for {result:?}"
            );
            let json = serde_json::to_string(&proto).unwrap();
            let back = turn_result_from_json(&json);
            let back_proto = run_result_proto(&back);
            assert_eq!(
                back_proto.ended, expected_ended,
                "round-trip must preserve ended={expected_ended} for {result:?}"
            );
        }
    }
}

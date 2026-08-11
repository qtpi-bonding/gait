use std::collections::{BTreeMap, HashMap};

use crate::proto::gait::log::v1 as pb;

/// Fold the mutation-effective tool activity inside one run window. Checkpoint
/// changes are supplied by the baseline-to-final diff owned by the caller.
pub(crate) fn build(
    events: &[pb::Event],
    run: u32,
    start_seq: u64,
    end_seq: u64,
    changes: Option<pb::ChangeSummary>,
) -> pb::RunSummary {
    use crate::session::retrace::{active_mutations, apply_cascade, effective_kinds};

    let mut mutations = active_mutations(events);
    apply_cascade(&mut mutations, events);
    let effective: Vec<_> = effective_kinds(events, &mutations)
        .filter(|(event, _)| {
            event.run == Some(run) && event.seq >= start_seq && event.seq <= end_seq
        })
        .collect();

    let mut counts = BTreeMap::<String, u32>::new();
    let mut shell = Vec::<pb::ShellInvocation>::new();
    let mut results = HashMap::new();
    for (_, kind) in &effective {
        if let pb::payload::Kind::ToolResult(result) = kind {
            results.insert(result.call_id.as_str(), result.clone());
        }
    }
    for (_, kind) in effective {
        let pb::payload::Kind::ToolCall(call) = kind else {
            continue;
        };
        *counts.entry(call.name.clone()).or_default() += 1;
        if call.name == "shell" {
            let command = call.input.as_ref().and_then(shell_command);
            let exit_code = results
                .get(call.call_id.as_str())
                .and_then(|result| result.process.as_ref())
                .map(|process| process.exit_code);
            shell.push(pb::ShellInvocation {
                call_id: call.call_id.clone(),
                command,
                exit_code,
            });
        }
    }

    pb::RunSummary {
        changes,
        tools: counts
            .into_iter()
            .map(|(name, count)| pb::ToolCount { name, count })
            .collect(),
        shell,
    }
}

fn shell_command(input: &pbjson_types::Struct) -> Option<String> {
    let value = serde_json::to_value(input).ok()?;
    value
        .get("cmd")
        .or_else(|| value.get("command"))
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::envelope::{mint, EventScope};

    fn push(events: &mut Vec<pb::Event>, body: pb::event::Body, scope: EventScope) -> String {
        let event = mint(body, events.last(), scope);
        let id = event.id.clone();
        events.push(event);
        id
    }

    #[test]
    fn aggregates_multiple_turns_and_keeps_missing_shell_edges() {
        let mut events = Vec::new();
        push(
            &mut events,
            crate::proto::events::run_start(pb::RunStart {
                run_id: "r0".into(),
                index: 0,
                ..Default::default()
            }),
            EventScope::Run { run: 0 },
        );
        push(
            &mut events,
            crate::proto::events::tool_call(
                "s1".into(),
                "shell".into(),
                &serde_json::json!({"cmd":"cargo test"}),
            ),
            EventScope::Step {
                run: 0,
                turn: 0,
                step: 0,
            },
        );
        push(
            &mut events,
            crate::proto::events::tool_result(
                "s1".into(),
                "ok".into(),
                None,
                Some(pb::ProcessResult { exit_code: 0 }),
            ),
            EventScope::Step {
                run: 0,
                turn: 0,
                step: 0,
            },
        );
        push(
            &mut events,
            crate::proto::events::tool_call(
                "s2".into(),
                "shell".into(),
                &serde_json::json!({"unexpected":true}),
            ),
            EventScope::Step {
                run: 0,
                turn: 1,
                step: 0,
            },
        );
        push(
            &mut events,
            crate::proto::events::tool_call("r1".into(), "read".into(), &serde_json::json!({})),
            EventScope::Step {
                run: 0,
                turn: 1,
                step: 0,
            },
        );
        let end = events.last().unwrap().seq;
        push(
            &mut events,
            crate::proto::events::tool_call(
                "outside".into(),
                "shell".into(),
                &serde_json::json!({"cmd":"do not count"}),
            ),
            EventScope::Step {
                run: 1,
                turn: 0,
                step: 0,
            },
        );

        let summary = build(&events, 0, 1, end, None);
        assert_eq!(
            summary.tools,
            vec![
                pb::ToolCount {
                    name: "read".into(),
                    count: 1
                },
                pb::ToolCount {
                    name: "shell".into(),
                    count: 2
                }
            ]
        );
        assert_eq!(summary.shell.len(), 2);
        assert_eq!(summary.shell[0].command.as_deref(), Some("cargo test"));
        assert_eq!(summary.shell[0].exit_code, Some(0));
        assert_eq!(
            summary.shell[1].command, None,
            "malformed command stays absent"
        );
        assert_eq!(
            summary.shell[1].exit_code, None,
            "missing result stays absent"
        );
    }

    #[test]
    fn mutations_cannot_resurrect_redacted_or_replaced_secrets() {
        let mut events = Vec::new();
        push(
            &mut events,
            crate::proto::events::run_start(pb::RunStart {
                run_id: "r0".into(),
                index: 0,
                ..Default::default()
            }),
            EventScope::Run { run: 0 },
        );
        let redacted = push(
            &mut events,
            crate::proto::events::tool_call(
                "secret-1".into(),
                "shell".into(),
                &serde_json::json!({"cmd":"echo TOP_SECRET"}),
            ),
            EventScope::Step {
                run: 0,
                turn: 0,
                step: 0,
            },
        );
        push(
            &mut events,
            crate::proto::events::redact(redacted, None),
            EventScope::Step {
                run: 0,
                turn: 0,
                step: 0,
            },
        );
        let replaced = push(
            &mut events,
            crate::proto::events::tool_call(
                "secret-2".into(),
                "shell".into(),
                &serde_json::json!({"cmd":"cat PRIVATE_TOKEN"}),
            ),
            EventScope::Step {
                run: 0,
                turn: 0,
                step: 0,
            },
        );
        let replacement = pb::Payload {
            kind: Some(pb::payload::Kind::ToolCall(pb::ToolCall {
                call_id: "secret-2".into(),
                name: "shell".into(),
                input: crate::proto::events::to_struct(&serde_json::json!({"cmd":"echo safe"})),
            })),
        };
        push(
            &mut events,
            crate::proto::events::replace(replaced, replacement, None),
            EventScope::Step {
                run: 0,
                turn: 0,
                step: 0,
            },
        );
        let summary = build(&events, 0, 1, events.last().unwrap().seq, None);
        assert_eq!(
            summary.tools,
            vec![pb::ToolCount {
                name: "shell".into(),
                count: 1
            }]
        );
        assert_eq!(summary.shell[0].command.as_deref(), Some("echo safe"));
        let json = serde_json::to_string(&summary).unwrap();
        assert!(!json.contains("TOP_SECRET"));
        assert!(!json.contains("PRIVATE_TOKEN"));
    }
}

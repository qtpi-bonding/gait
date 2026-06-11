//! # Retrace — the `Log → working-state` fold
//!
//! Rebuilds the model's working state from the immutable event `Log`. It produces
//! two projections:
//!
//! - [`messages`] — the **Transcript**: the `Vec<Message>` the model reasons over.
//!   Read-only today; its only mutation is prefix-`Compaction` (truncate-before-summary).
//! - [`enabled_loadables`] — the active loadable set, via last-write-wins toggle
//!   replay of `load(name, enabled=…)` events.
//!
//! General message-list mutation (`redact`/`replace` by event id) is a future spec;
//! see `docs/superpowers/specs/2026-06-10-agent-architecture-design.md`. The `Log`
//! JSONL wire format is frozen.

use std::path::Path;
use crate::types::{Message, MessageContent, AssistantContent, AssistantPart, ToolCall, ToolResult, ToolResultKind};
use crate::session::transcript::{read_events, last_compaction_index};
use crate::proto::gait::log::v1::{event, payload, ClarifyKind, Event};

/// The substantive payload kind for an event, if it carries one.
fn kind(e: &Event) -> Option<&payload::Kind> {
    match &e.body {
        Some(event::Body::Payload(p)) => p.kind.as_ref(),
        _ => None,
    }
}

/// Proto `Struct` tool input → `serde_json::Value` object (or Null).
fn struct_to_value(s: &Option<pbjson_types::Struct>) -> serde_json::Value {
    match s {
        Some(st) => serde_json::to_value(st).unwrap_or(serde_json::Value::Null),
        None => serde_json::Value::Null,
    }
}

fn clarify_kind_str(k: i32) -> &'static str {
    match ClarifyKind::try_from(k).unwrap_or(ClarifyKind::Unspecified) {
        ClarifyKind::Is => "is",
        ClarifyKind::Ought => "ought",
        ClarifyKind::Unspecified => "unspecified",
    }
}

pub fn messages(all_events: &[Event]) -> anyhow::Result<Vec<Message>> {
    if all_events.is_empty() { return Ok(vec![]); }

    // Start from last compaction breakpoint (or beginning)
    let start = last_compaction_index(all_events).unwrap_or(0);
    // If there IS a compaction at `start`, include it (it provides the summary message)
    let events = &all_events[start..];

    let mut messages: Vec<Message> = vec![];
    let mut i = 0;

    while i < events.len() {
        match kind(&events[i]) {
            Some(payload::Kind::Compaction(c)) => {
                messages.push(Message::User {
                    content: MessageContent::Text(
                        format!("[Session history summary: {}]", c.summary)
                    ),
                });
                i += 1;
            }
            Some(payload::Kind::SessionStart(_)) => { i += 1; }

            Some(payload::Kind::AgentUpdate(au)) => {
                let status = crate::proto::gait::log::v1::AgentStatus::try_from(au.status)
                    .map(|s| match s {
                        crate::proto::gait::log::v1::AgentStatus::Done => "done",
                        crate::proto::gait::log::v1::AgentStatus::Clarify => "clarify",
                        crate::proto::gait::log::v1::AgentStatus::Inform => "inform",
                        crate::proto::gait::log::v1::AgentStatus::Unspecified => "unspecified",
                    })
                    .unwrap_or("unspecified");
                let mut content = format!("[agent:{}] status: {}\nsummary: {}", au.session_id, status, au.summary);
                if let Some(items) = &au.items {
                    let v = serde_json::to_value(items).unwrap_or(serde_json::Value::Null);
                    content.push_str(&format!("\nitems: {}", v));
                }
                messages.push(Message::User { content: MessageContent::Text(content) });
                i += 1;
            }

            Some(payload::Kind::UserTurn(u)) => {
                messages.push(Message::User { content: MessageContent::Text(u.content.clone()) });
                i += 1;
            }

            Some(payload::Kind::AssistantText(a)) => {
                messages.push(Message::Assistant { content: AssistantContent::Text(a.content.clone()) });
                i += 1;
            }

            Some(payload::Kind::ToolCall(_)) => {
                // Gather all consecutive tool_call events
                let mut calls: Vec<(String, String, serde_json::Value)> = vec![];
                while i < events.len() {
                    if let Some(payload::Kind::ToolCall(tc)) = kind(&events[i]) {
                        calls.push((tc.call_id.clone(), tc.name.clone(), struct_to_value(&tc.input)));
                        i += 1;
                    } else { break; }
                }
                // Gather consecutive tool_result events
                let mut results: Vec<(String, String)> = vec![];
                while i < events.len() {
                    if let Some(payload::Kind::ToolResult(tr)) = kind(&events[i]) {
                        results.push((tr.call_id.clone(), tr.output.clone()));
                        i += 1;
                    } else { break; }
                }

                let parts: Vec<AssistantPart> = calls.iter().map(|(id, name, args)| {
                    AssistantPart::ToolCall(ToolCall {
                        tool_call_id: id.clone(),
                        tool_name: name.clone(),
                        args: args.clone(),
                    })
                }).collect();
                messages.push(Message::Assistant { content: AssistantContent::Parts(parts) });

                if !results.is_empty() {
                    let tool_content: Vec<ToolResult> = results.iter().map(|(id, output)| {
                        ToolResult {
                            kind: ToolResultKind::ToolResult,
                            tool_call_id: id.clone(),
                            result: serde_json::Value::String(output.clone()),
                        }
                    }).collect();
                    messages.push(Message::Tool { content: tool_content });
                }
            }

            Some(payload::Kind::ToolResult(_)) => { i += 1; } // orphan — skip

            Some(payload::Kind::Clarify(c)) => {
                let text = c.items.iter().map(|q| {
                    format!("clarify ({}): {}\nwhy: {}", clarify_kind_str(q.kind), q.content, q.why)
                }).collect::<Vec<_>>().join("\n\n");
                messages.push(Message::Assistant { content: AssistantContent::Text(text) });
                i += 1;
            }

            Some(payload::Kind::ClarifyReply(cr)) => {
                let text = cr.items.iter().map(|r| {
                    let base = format!("{}: {}", r.id, r.content);
                    if let Some(reason) = &r.reasoning { format!("{}\nreason: {}", base, reason) }
                    else { base }
                }).collect::<Vec<_>>().join("\n");
                messages.push(Message::User { content: MessageContent::Text(text) });
                i += 1;
            }

            Some(payload::Kind::ClarifyConvergence(cc)) => {
                for item in &cc.items {
                    messages.push(Message::Assistant { content: AssistantContent::Text(format!("[proposal] {}", item.proposal)) });
                    if let Some(resp) = &item.response {
                        messages.push(Message::User { content: MessageContent::Text(format!("[response] {}", resp)) });
                    }
                }
                i += 1;
            }

            Some(payload::Kind::Inform(inf)) => {
                let text = inf.items.iter().map(|it| {
                    let mut s = it.content.clone();
                    if let Some(ctx) = &it.context { s.push_str(&format!("\ncontext: {}", ctx)); }
                    s
                }).collect::<Vec<_>>().join("\n\n");
                messages.push(Message::Assistant { content: AssistantContent::Text(text) });
                i += 1;
            }

            Some(payload::Kind::InformReply(ir)) => {
                let text = ir.items.iter().map(|r| format!("{}: {}", r.id, r.content)).collect::<Vec<_>>().join("\n");
                messages.push(Message::User { content: MessageContent::Text(text) });
                i += 1;
            }

            Some(payload::Kind::Permissions(_)) => { i += 1; }    // audit event — excluded from model context
            Some(payload::Kind::PermissionsReply(_)) => { i += 1; } // audit event — excluded from model context

            Some(payload::Kind::Error(e)) => {
                let phase = crate::proto::gait::log::v1::ErrorPhase::try_from(e.phase)
                    .map(|p| match p {
                        crate::proto::gait::log::v1::ErrorPhase::Llm => "llm",
                        crate::proto::gait::log::v1::ErrorPhase::Tool => "tool",
                        crate::proto::gait::log::v1::ErrorPhase::Git => "git",
                        crate::proto::gait::log::v1::ErrorPhase::Init => "init",
                        crate::proto::gait::log::v1::ErrorPhase::Unspecified => "unspecified",
                    })
                    .unwrap_or("unspecified");
                messages.push(Message::User { content: MessageContent::Text(format!("error during {}: {}", phase, e.message)) });
                i += 1;
            }

            // Skip: TurnComplete, Raw (workflow/etc.), InformConvergence, Resolution,
            // mutations (Redact/Replace) — not needed for LLM context.
            _ => { i += 1; }
        }
    }

    Ok(messages)
}

pub fn messages_from_path(path: &Path) -> anyhow::Result<Vec<Message>> {
    let all_events = read_events(path)?;
    messages(&all_events)
}

/// Derive which loadable names are currently enabled, from the Log.
/// Seeds from config defaults (`config_enabled`), then replays the transcript's
/// `load(name, enabled=…)` tool calls as last-write-wins toggles per entity.
pub fn enabled_loadables(
    events: &[Event],
    config_enabled: &std::collections::HashSet<String>,
) -> std::collections::HashSet<String> {
    let mut enabled = config_enabled.clone();
    for event in events {
        if let Some(payload::Kind::ToolCall(tc)) = kind(event) {
            if tc.name == "load" {
                let input = struct_to_value(&tc.input);
                let item_name = input.get("name")
                    .and_then(|v| v.as_str()).unwrap_or("").to_string();
                let on = input.get("enabled")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                if on { enabled.insert(item_name); } else { enabled.remove(&item_name); }
            }
        }
    }
    enabled
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::transcript::*;
    use crate::proto::{events, wire};
    use tempfile::TempDir;

    fn write_events(dir: &std::path::Path, bodies: Vec<event::Body>) -> std::path::PathBuf {
        let path = dir.join("transcript.jsonl");
        for b in bodies { wire::append(&path, b).unwrap(); }
        path
    }

    /// Build in-memory proto events from bodies (seq is irrelevant for these
    /// assertions; each is minted as a root).
    fn mint_events(bodies: Vec<event::Body>) -> Vec<Event> {
        bodies.into_iter()
            .map(|b| crate::proto::envelope::mint(b, None))
            .collect()
    }

    #[test]
    fn empty_transcript_returns_empty() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("transcript.jsonl");
        let msgs = messages_from_path(&path).unwrap();
        assert!(msgs.is_empty());
    }

    #[test]
    fn user_and_assistant_text_reconstructed() {
        let dir = TempDir::new().unwrap();
        let path = write_events(dir.path(), vec![
            events::session_start("s1".into(), None),
            events::user_turn("hello".into(), None, None),
            events::assistant_text("hi there".into()),
        ]);
        let msgs = messages_from_path(&path).unwrap();
        assert_eq!(msgs.len(), 2); // session_start is skipped
        assert!(matches!(&msgs[0], Message::User { content: MessageContent::Text(t) } if t == "hello"));
        assert!(matches!(&msgs[1], Message::Assistant { content: AssistantContent::Text(t) } if t == "hi there"));
    }

    #[test]
    fn tool_call_and_result_paired() {
        let dir = TempDir::new().unwrap();
        let path = write_events(dir.path(), vec![
            events::tool_call("id1".into(), "read".into(), &serde_json::json!({"file_path":"a.rs"})),
            events::tool_result("id1".into(), "contents".into()),
        ]);
        let msgs = messages_from_path(&path).unwrap();
        assert_eq!(msgs.len(), 2);
        if let Message::Assistant { content: AssistantContent::Parts(parts) } = &msgs[0] {
            assert_eq!(parts.len(), 1);
            assert!(matches!(&parts[0], AssistantPart::ToolCall(tc) if tc.tool_call_id == "id1"));
        } else { panic!("expected assistant parts"); }
        assert!(matches!(&msgs[1], Message::Tool { .. }));
    }

    #[test]
    fn parallel_tool_calls_grouped() {
        let dir = TempDir::new().unwrap();
        let path = write_events(dir.path(), vec![
            events::tool_call("id1".into(), "read".into(), &serde_json::json!({})),
            events::tool_call("id2".into(), "ls".into(), &serde_json::json!({})),
            events::tool_result("id1".into(), "r1".into()),
            events::tool_result("id2".into(), "r2".into()),
        ]);
        let msgs = messages_from_path(&path).unwrap();
        assert_eq!(msgs.len(), 2);
        if let Message::Assistant { content: AssistantContent::Parts(parts) } = &msgs[0] {
            assert_eq!(parts.len(), 2);
        } else { panic!("expected 2 tool calls in one assistant message"); }
        if let Message::Tool { content } = &msgs[1] {
            assert_eq!(content.len(), 2);
        } else { panic!("expected tool message with 2 results"); }
    }

    #[test]
    fn permissions_excluded_from_context_reconstruction() {
        let bodies = vec![
            events::user_turn("do x".into(), None, None),
            events::permissions(&[PermissionItem {
                id: "tc1".into(), cmd: "shell".into(), reason: "test".into(),
                patterns: vec![], tool_name: "shell".into(),
                tool_args: serde_json::Value::Null,
            }]),
            events::permissions_reply("user".into(), &[PermissionsReplyItem {
                id: "tc1".into(), decision: "approve".into(), text: None, scope: None,
            }]),
        ];
        let messages = messages(&mint_events(bodies)).unwrap();
        assert_eq!(messages.len(), 1, "only UserTurn should appear; got: {} messages", messages.len());
        assert!(matches!(&messages[0], Message::User { .. }));
    }

    #[test]
    fn compaction_breakpoint_discards_earlier_events() {
        let dir = TempDir::new().unwrap();
        let usage = TurnUsage{input:1,output:1,cache_read:0,cache_write:0};
        let output = TurnOutput{message:"s".into(),result:None};
        let cu = CompactionUsage{input:10,output:5};
        let path = write_events(dir.path(), vec![
            events::user_turn("old".into(), None, None),
            events::assistant_text("old response".into()),
            events::turn_complete(&usage, "h".into(), false, None, vec![], &output),
            events::compaction("summary of old stuff".into(), 1, &cu),
            events::user_turn("new".into(), None, None),
        ]);
        let msgs = messages_from_path(&path).unwrap();
        // Should have: compaction summary + "new" user turn
        assert_eq!(msgs.len(), 2, "got: {:?}", msgs.len());
        assert!(matches!(&msgs[0], Message::User { content: MessageContent::Text(t) } if t.contains("summary of old stuff")));
        assert!(matches!(&msgs[1], Message::User { content: MessageContent::Text(t) } if t == "new"));
    }
    #[test]
    fn enabled_loadables_seeds_from_config_and_replays_toggles() {
        use std::collections::HashSet;
        let config_enabled: HashSet<String> = ["tdd".into()].into();
        let bodies = vec![
            events::tool_call("1".into(), "load".into(), &serde_json::json!({"name": "slack_send", "enabled": true})),
            events::tool_call("2".into(), "load".into(), &serde_json::json!({"name": "tdd", "enabled": false})),
        ];
        let enabled = enabled_loadables(&mint_events(bodies), &config_enabled);
        assert!(enabled.contains("slack_send"), "slack should be enabled after load");
        assert!(!enabled.contains("tdd"), "tdd should be disabled after unload (LWW per entity)");
    }

    #[test]
    fn both_projections_derive_from_one_event_log() {
        use std::collections::HashSet;
        let bodies = vec![
            events::user_turn("hi".into(), None, None),
            events::tool_call("1".into(), "load".into(), &serde_json::json!({"name": "slack_send", "enabled": true})),
        ];
        let proto_events = mint_events(bodies);
        let msgs = messages(&proto_events).unwrap();
        assert!(matches!(&msgs[0], Message::User { content: MessageContent::Text(t) } if t == "hi"));
        let enabled = enabled_loadables(&proto_events, &HashSet::new());
        assert!(enabled.contains("slack_send"));
    }
}

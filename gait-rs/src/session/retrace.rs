use std::path::Path;
use crate::types::{Message, MessageContent, AssistantContent, AssistantPart, ToolCall, ToolResult, ToolResultKind};
use crate::session::transcript::{AnyEvent, read_events, last_compaction_index};

pub fn messages(all_events: &[AnyEvent]) -> anyhow::Result<Vec<Message>> {
    if all_events.is_empty() { return Ok(vec![]); }

    // Start from last compaction breakpoint (or beginning)
    let start = last_compaction_index(all_events).unwrap_or(0);
    // If there IS a compaction at `start`, include it (it provides the summary message)
    let events = &all_events[start..];

    let mut messages: Vec<Message> = vec![];
    let mut i = 0;

    while i < events.len() {
        match &events[i] {
            AnyEvent::Compaction { summary, .. } => {
                messages.push(Message::User {
                    content: MessageContent::Text(
                        format!("[Session history summary: {}]", summary)
                    ),
                });
                i += 1;
            }
            AnyEvent::SessionStart { .. } => { i += 1; }

            AnyEvent::AgentUpdate { session_id, status, summary, items, .. } => {
                let mut content = format!("[agent:{}] status: {}\nsummary: {}", session_id, status, summary);
                if let Some(items) = items {
                    content.push_str(&format!("\nitems: {}", items));
                }
                messages.push(Message::User { content: MessageContent::Text(content) });
                i += 1;
            }

            AnyEvent::UserTurn { content, .. } => {
                messages.push(Message::User { content: MessageContent::Text(content.clone()) });
                i += 1;
            }

            AnyEvent::AssistantText { content, .. } => {
                messages.push(Message::Assistant { content: AssistantContent::Text(content.clone()) });
                i += 1;
            }

            AnyEvent::ToolCall { .. } => {
                // Gather all consecutive tool_call events
                let mut calls: Vec<(String, String, serde_json::Value)> = vec![];
                while i < events.len() {
                    if let AnyEvent::ToolCall { id, name, input, .. } = &events[i] {
                        calls.push((id.clone(), name.clone(), input.clone()));
                        i += 1;
                    } else { break; }
                }
                // Gather consecutive tool_result events
                let mut results: Vec<(String, String)> = vec![];
                while i < events.len() {
                    if let AnyEvent::ToolResult { id, output, .. } = &events[i] {
                        results.push((id.clone(), output.clone()));
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

            AnyEvent::ToolResult { .. } => { i += 1; } // orphan — skip

            AnyEvent::Clarify { items, .. } => {
                let text = items.iter().map(|q| {
                    format!("clarify ({}): {}\nwhy: {}", q.kind, q.content, q.why)
                }).collect::<Vec<_>>().join("\n\n");
                messages.push(Message::Assistant { content: AssistantContent::Text(text) });
                i += 1;
            }

            AnyEvent::ClarifyReply { items, .. } => {
                let text = items.iter().map(|r| {
                    let base = format!("{}: {}", r.id, r.content);
                    if let Some(reason) = &r.reasoning { format!("{}\nreason: {}", base, reason) }
                    else { base }
                }).collect::<Vec<_>>().join("\n");
                messages.push(Message::User { content: MessageContent::Text(text) });
                i += 1;
            }

            AnyEvent::ClarifyConvergence { items, .. } => {
                for item in items {
                    messages.push(Message::Assistant { content: AssistantContent::Text(format!("[proposal] {}", item.proposal)) });
                    if let Some(resp) = &item.response {
                        messages.push(Message::User { content: MessageContent::Text(format!("[response] {}", resp)) });
                    }
                }
                i += 1;
            }

            AnyEvent::Inform { items, .. } => {
                let text = items.iter().map(|it| {
                    let mut s = it.content.clone();
                    if let Some(ctx) = &it.context { s.push_str(&format!("\ncontext: {}", ctx)); }
                    s
                }).collect::<Vec<_>>().join("\n\n");
                messages.push(Message::Assistant { content: AssistantContent::Text(text) });
                i += 1;
            }

            AnyEvent::InformReply { items, .. } => {
                let text = items.iter().map(|r| format!("{}: {}", r.id, r.content)).collect::<Vec<_>>().join("\n");
                messages.push(Message::User { content: MessageContent::Text(text) });
                i += 1;
            }

            AnyEvent::Permissions { .. } => { i += 1; }    // audit event — excluded from model context
            AnyEvent::PermissionsReply { .. } => { i += 1; } // audit event — excluded from model context

            AnyEvent::Error { message, phase, .. } => {
                messages.push(Message::User { content: MessageContent::Text(format!("error during {}: {}", phase, message)) });
                i += 1;
            }

            // Skip: TurnComplete, CommandResult, InformConvergence (not needed for LLM context)
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
    events: &[AnyEvent],
    config_enabled: &std::collections::HashSet<String>,
) -> std::collections::HashSet<String> {
    let mut enabled = config_enabled.clone();
    for event in events {
        if let AnyEvent::ToolCall { name, input, .. } = event {
            if name == "load" {
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
    use tempfile::TempDir;

    fn write_events(dir: &std::path::Path, events: &[AnyEvent]) -> std::path::PathBuf {
        let path = dir.join("transcript.jsonl");
        for e in events { append_event(&path, e).unwrap(); }
        path
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
        let path = write_events(dir.path(), &[
            AnyEvent::SessionStart { schema_version: 2, id: "s1".into(), model: "m".into(), codebase_hash: "h".into(), codebase_dirty: false, ts: 0 },
            AnyEvent::UserTurn { content: "hello".into(), from: None, context: None, ts: 1 },
            AnyEvent::AssistantText { content: "hi there".into(), ts: 2 },
        ]);
        let msgs = messages_from_path(&path).unwrap();
        assert_eq!(msgs.len(), 2); // session_start is skipped
        assert!(matches!(&msgs[0], Message::User { content: MessageContent::Text(t) } if t == "hello"));
        assert!(matches!(&msgs[1], Message::Assistant { content: AssistantContent::Text(t) } if t == "hi there"));
    }

    #[test]
    fn tool_call_and_result_paired() {
        let dir = TempDir::new().unwrap();
        let path = write_events(dir.path(), &[
            AnyEvent::ToolCall { id: "id1".into(), name: "read".into(), input: serde_json::json!({"file_path":"a.rs"}), ts: 0 },
            AnyEvent::ToolResult { id: "id1".into(), output: "contents".into(), ts: 1 },
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
        let path = write_events(dir.path(), &[
            AnyEvent::ToolCall { id: "id1".into(), name: "read".into(), input: serde_json::json!({}), ts: 0 },
            AnyEvent::ToolCall { id: "id2".into(), name: "ls".into(), input: serde_json::json!({}), ts: 1 },
            AnyEvent::ToolResult { id: "id1".into(), output: "r1".into(), ts: 2 },
            AnyEvent::ToolResult { id: "id2".into(), output: "r2".into(), ts: 3 },
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
        let events = vec![
            AnyEvent::UserTurn { content: "do x".into(), from: None, context: None, ts: 1 },
            AnyEvent::Permissions {
                items: vec![PermissionItem {
                    id: "tc1".into(), cmd: "shell".into(), reason: "test".into(),
                    patterns: vec![], tool_name: "shell".into(),
                    tool_args: serde_json::Value::Null,
                }],
                ts: 2,
            },
            AnyEvent::PermissionsReply {
                from: "user".into(),
                items: vec![PermissionsReplyItem {
                    id: "tc1".into(), decision: "approve".into(), text: None, scope: None,
                }],
                ts: 3,
            },
        ];
        let messages = messages(&events).unwrap();
        assert_eq!(messages.len(), 1, "only UserTurn should appear; got: {} messages", messages.len());
        assert!(matches!(&messages[0], Message::User { .. }));
    }

    #[test]
    fn compaction_breakpoint_discards_earlier_events() {
        let dir = TempDir::new().unwrap();
        let path = write_events(dir.path(), &[
            AnyEvent::UserTurn { content: "old".into(), from: None, context: None, ts: 0 },
            AnyEvent::AssistantText { content: "old response".into(), ts: 1 },
            AnyEvent::TurnComplete { usage: TurnUsage{input:1,output:1,cache_read:0,cache_write:0}, codebase_hash:"h".into(), codebase_dirty:false, code_checkpoint: None, blocks: vec![], output: TurnOutput{message:"s".into(),result:None}, ts:2 },
            AnyEvent::Compaction { summary: "summary of old stuff".into(), turns_covered: 1, from_ts: 0, to_ts: 2, usage: CompactionUsage{input:10,output:5}, ts: 3 },
            AnyEvent::UserTurn { content: "new".into(), from: None, context: None, ts: 4 },
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
        let events = vec![
            AnyEvent::ToolCall { id: "1".into(), name: "load".into(),
                input: serde_json::json!({"name": "slack_send", "enabled": true}), ts: 1 },
            AnyEvent::ToolCall { id: "2".into(), name: "load".into(),
                input: serde_json::json!({"name": "tdd", "enabled": false}), ts: 2 },
        ];
        let enabled = enabled_loadables(&events, &config_enabled);
        assert!(enabled.contains("slack_send"), "slack should be enabled after load");
        assert!(!enabled.contains("tdd"), "tdd should be disabled after unload (LWW per entity)");
    }
}

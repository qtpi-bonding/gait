//! Pure translation between gait proto events and ACP schema types, plus the
//! TurnSink/TurnSource port impls that drive one ACP-initiated turn.
//! See docs/plans/2026-07-10-acp-adapter-phase2-design.md.

use agent_client_protocol::schema::v1::{
    ContentBlock, ContentChunk, SessionUpdate, TextContent, ToolCall as AcpToolCall, ToolCallId,
    ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
};
use gait_core::proto::gait::log::v1::{Event, event, payload};

/// Wrap ACP text into an `AgentMessageChunk` / `AgentThoughtChunk` body.
fn text_chunk(text: impl Into<String>) -> ContentChunk {
    ContentChunk::new(ContentBlock::Text(TextContent::new(text)))
}

/// Translate one *settled* gait event to an ACP `session/update`. Live text
/// deltas (`StreamItem::Delta`) are handled in the sink, not here. Returns
/// `None` for events with no client-facing rendering (control, session_start,
/// context, errors surfaced via the request result, …).
pub fn event_to_session_update(ev: &Event) -> Option<SessionUpdate> {
    let Some(event::Body::Payload(p)) = &ev.body else {
        return None;
    };
    match p.kind.as_ref()? {
        payload::Kind::AssistantText(t) => Some(SessionUpdate::AgentMessageChunk(text_chunk(
            t.content.clone(),
        ))),
        payload::Kind::ThinkingBlock(t) => Some(SessionUpdate::AgentThoughtChunk(text_chunk(
            t.content.clone(),
        ))),
        payload::Kind::ToolCall(tc) => {
            // proto field is `call_id` (not `id`); `input` is an Option<Struct>,
            // so convert with to_value, not from_str.
            let mut call = AcpToolCall::new(ToolCallId::new(tc.call_id.clone()), tc.name.clone());
            call.raw_input = tc.input.as_ref().and_then(|s| serde_json::to_value(s).ok());
            Some(SessionUpdate::ToolCall(call))
        }
        payload::Kind::ToolResult(tr) => {
            let ok = tr.process.as_ref().is_none_or(|pr| pr.exit_code == 0);
            // status lives on ToolCallUpdateFields, and ToolCallUpdate::new takes
            // (id, fields) — two args.
            let fields = ToolCallUpdateFields::new().status(if ok {
                ToolCallStatus::Completed
            } else {
                ToolCallStatus::Failed
            });
            let upd = ToolCallUpdate::new(ToolCallId::new(tr.call_id.clone()), fields);
            Some(SessionUpdate::ToolCallUpdate(upd))
        }
        _ => None,
    }
}

/// Extract the prompt text from ACP content blocks (Phase 2 handles `Text`
/// blocks; image/audio/resource blocks are a marked seam for a later phase).
pub fn content_blocks_to_text(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::{ContentBlock, SessionUpdate, TextContent};
    use gait_core::proto::events;
    use gait_core::proto::gait::log::v1::Event;

    fn event(body: gait_core::proto::gait::log::v1::event::Body) -> Event {
        Event {
            body: Some(body),
            ..Default::default()
        }
    }

    #[test]
    fn assistant_text_maps_to_agent_message_chunk() {
        let ev = event(events::assistant_text("hello".into()));
        match event_to_session_update(&ev) {
            Some(SessionUpdate::AgentMessageChunk(chunk)) => {
                assert!(matches!(&chunk.content, ContentBlock::Text(t) if t.text == "hello"));
            }
            other => panic!("expected AgentMessageChunk, got {other:?}"),
        }
    }

    #[test]
    fn thinking_maps_to_agent_thought_chunk() {
        let ev = event(events::thinking_block("pondering".into(), String::new()));
        assert!(matches!(
            event_to_session_update(&ev),
            Some(SessionUpdate::AgentThoughtChunk(_))
        ));
    }

    #[test]
    fn tool_call_maps_to_tool_call_update_kind() {
        let ev = event(events::tool_call(
            "c1".into(),
            "read".into(),
            &serde_json::json!({"file_path": "a.rs"}),
        ));
        assert!(matches!(
            event_to_session_update(&ev),
            Some(SessionUpdate::ToolCall(_))
        ));
    }

    #[test]
    fn tool_result_maps_to_tool_call_update() {
        let ev = event(events::tool_result("c1".into(), "ok".into(), None, None));
        assert!(matches!(
            event_to_session_update(&ev),
            Some(SessionUpdate::ToolCallUpdate(_))
        ));
    }

    #[test]
    fn non_client_facing_events_map_to_none() {
        // A session_start / context event has no client-facing rendering.
        let ev = event(events::cwd("/tmp"));
        assert!(event_to_session_update(&ev).is_none());
    }

    #[test]
    fn content_blocks_to_text_concatenates_text_blocks() {
        let blocks = vec![
            ContentBlock::Text(TextContent::new("say ")),
            ContentBlock::Text(TextContent::new("hi")),
        ];
        assert_eq!(content_blocks_to_text(&blocks), "say hi");
    }
}

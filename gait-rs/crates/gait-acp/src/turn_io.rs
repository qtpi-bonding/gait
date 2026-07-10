//! Pure translation between gait proto events and ACP schema types, plus the
//! TurnSink/TurnSource port impls that drive one ACP-initiated turn.
//! See docs/plans/2026-07-10-acp-adapter-phase2-design.md.

use std::path::PathBuf;

use agent_client_protocol::schema::v1::{
    ContentBlock, ContentChunk, SessionId, SessionNotification, SessionUpdate, StopReason,
    TextContent, ToolCall as AcpToolCall, ToolCallId, ToolCallStatus, ToolCallUpdate,
    ToolCallUpdateFields,
};
use agent_client_protocol::{Client, ConnectionTo};
use gait_core::embed::{Diag, TurnSink, TurnSource, TurnStart, user_turn_events};
use gait_core::git::Repos;
use gait_core::proto::gait::log::v1::{Event, StreamItem, event, payload, stream_item};
use gait_core::turn::TurnResult;

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

/// Map a finished turn to an ACP `StopReason`. `Err(msg)` means the prompt must
/// be resolved as a JSON-RPC error (an internal failure is not a `Refusal`, and
/// an interaction-required result is unsupported until Phase 3).
pub fn turn_result_to_stop_reason(r: &TurnResult) -> Result<StopReason, String> {
    match r {
        TurnResult::Done { .. } => Ok(StopReason::EndTurn),
        TurnResult::Cancelled => Ok(StopReason::Cancelled),
        TurnResult::Failed { ended, .. } if ended == "max_steps" => Ok(StopReason::MaxTurnRequests),
        TurnResult::Failed { message, .. } => Err(message.clone()),
        TurnResult::Permissions
        | TurnResult::Clarify { .. }
        | TurnResult::Inform { .. }
        | TurnResult::Unlock { .. } => Err("interactive turn not supported yet (Phase 3)".into()),
    }
}

/// Feeds one ACP prompt into the turn: first `next()` yields the user input,
/// then `None`. The Phase-3 permission loop (`AwaitingPermission -> reply ->
/// Resume`, mirroring `crates/gait-repl/src/repl/driver.rs`) slots in at the
/// marked seam below.
pub struct AcpTurnSource {
    first: Option<Vec<gait_core::proto::gait::log::v1::event::Body>>,
}

impl AcpTurnSource {
    pub fn new(text: String, repos: Repos, codebase: PathBuf) -> Self {
        Self {
            first: Some(user_turn_events(&repos, &codebase, text)),
        }
    }
}

#[async_trait::async_trait(?Send)]
impl TurnSource for AcpTurnSource {
    async fn next(&mut self) -> Option<TurnStart> {
        // PHASE-3 SEAM: on the second call, instead of `None`, read
        // SessionState::AwaitingPermission from the log, emit a
        // session/request_permission, append the reply, and return
        // TurnStart::Resume. Phase 2 handles the no-approval happy path only.
        self.first.take().map(TurnStart::Input)
    }
}

/// Streams settled turn events out as ACP `session/update` notifications and
/// captures the final `TurnResult`. `cx.send_notification` is synchronous, so
/// `item()` sends inline — no bridge task, and FIFO ordering is preserved.
pub struct AcpTurnSink {
    cx: ConnectionTo<Client>,
    session_id: SessionId,
    result: Option<TurnResult>,
}

impl AcpTurnSink {
    pub fn new(cx: ConnectionTo<Client>, session_id: SessionId) -> Self {
        Self {
            cx,
            session_id,
            result: None,
        }
    }
    pub fn take_result(&mut self) -> Option<TurnResult> {
        self.result.take()
    }
}

impl TurnSink for AcpTurnSink {
    fn item(&mut self, item: &StreamItem) {
        // Live text deltas are a no-op stub this phase (the settled AssistantText
        // event carries the full text — see design §6).
        let Some(stream_item::Item::Event(ev)) = &item.item else {
            return;
        };
        if let Some(update) = event_to_session_update(ev) {
            let _ = self
                .cx
                .send_notification(SessionNotification::new(self.session_id.clone(), update));
        }
    }
    fn result(&mut self, r: &TurnResult) {
        self.result = Some(r.clone());
    }
    fn session_id(&mut self, _id: &str) {}
    fn diagnostic(&mut self, _level: Diag, _msg: &str) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::{ContentBlock, SessionUpdate, StopReason, TextContent};
    use gait_core::proto::events;
    use gait_core::proto::gait::log::v1::Event;
    use gait_core::turn::TurnResult;

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

    #[test]
    fn done_maps_to_end_turn() {
        let r = TurnResult::Done {
            message: "ok".into(),
            result: None,
            changes: None,
            implicit: true,
        };
        assert_eq!(turn_result_to_stop_reason(&r), Ok(StopReason::EndTurn));
    }

    #[test]
    fn cancelled_maps_to_cancelled() {
        assert_eq!(
            turn_result_to_stop_reason(&TurnResult::Cancelled),
            Ok(StopReason::Cancelled)
        );
    }

    #[test]
    fn max_steps_maps_to_max_turn_requests() {
        let r = TurnResult::Failed {
            ended: "max_steps".into(),
            message: "hit cap".into(),
        };
        assert_eq!(
            turn_result_to_stop_reason(&r),
            Ok(StopReason::MaxTurnRequests)
        );
    }

    #[test]
    fn error_maps_to_jsonrpc_error() {
        let r = TurnResult::Failed {
            ended: "error".into(),
            message: "boom".into(),
        };
        assert_eq!(turn_result_to_stop_reason(&r), Err("boom".into()));
    }

    #[test]
    fn interaction_required_maps_to_jsonrpc_error() {
        // Permissions/Clarify/Inform/Unlock are the Phase-3 seam: no interactive
        // support yet, so surface a loud error rather than a fake completion.
        assert!(turn_result_to_stop_reason(&TurnResult::Permissions).is_err());
        assert!(turn_result_to_stop_reason(&TurnResult::Clarify { items: vec![] }).is_err());
        assert!(turn_result_to_stop_reason(&TurnResult::Inform { items: vec![] }).is_err());
        assert!(turn_result_to_stop_reason(&TurnResult::Unlock { items: vec![] }).is_err());
    }

    #[tokio::test]
    async fn source_yields_input_once_then_none() {
        use gait_core::embed::{TurnSource, TurnStart};
        let repos = gait_core::git::Repos::production();
        let mut src = AcpTurnSource::new("say hi".into(), repos, std::env::temp_dir());
        assert!(matches!(src.next().await, Some(TurnStart::Input(_))));
        assert!(
            src.next().await.is_none(),
            "second next() ends the turn (Phase-3 seam)"
        );
    }
}

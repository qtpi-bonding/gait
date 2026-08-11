//! Pure translation between gait proto events and ACP schema types, plus the
//! TurnSink/TurnSource port impls that drive one ACP-initiated turn.
//! See docs/plans/2026-07-10-acp-adapter-phase2-design.md.
use std::collections::HashMap;
use std::path::PathBuf;

use agent_client_protocol::schema::v1::{
    ContentBlock, ContentChunk, PermissionOption, PermissionOptionId, PermissionOptionKind,
    RequestPermissionOutcome, RequestPermissionRequest, SelectedPermissionOutcome, SessionId,
    SessionNotification, SessionUpdate, StopReason, TextContent, ToolCall as AcpToolCall,
    ToolCallId, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
};
use agent_client_protocol::{Client, ConnectionTo};
use gait_core::embed::{Diag, TurnSink, TurnSource, TurnStart, user_turn_events};
use gait_core::git::Repos;
use gait_core::proto::gait::log::v1::{
    Decision, Event, PermissionItem, RunResult, StreamItem, event, payload, stream_item,
};
use gait_core::session::log::{SessionState, session_state};
use gait_ops::permission::{GrantScope, ItemChoice};

/// Wrap ACP text into an `AgentMessageChunk` / `AgentThoughtChunk` body.
fn text_chunk(text: impl Into<String>) -> ContentChunk {
    ContentChunk::new(ContentBlock::Text(TextContent::new(text)))
}

/// Translate a client-renderable payload to an ACP `session/update`.
///
/// Shared by live settled-event delivery and durable-history replay. Returns
/// `None` for control/session metadata and errors surfaced through requests.
pub fn kind_to_session_update(kind: &payload::Kind) -> Option<SessionUpdate> {
    match kind {
        payload::Kind::UserText(t) => Some(SessionUpdate::UserMessageChunk(text_chunk(
            t.content.clone(),
        ))),
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

/// Translate one *settled* gait event to an ACP `session/update`. Live text
/// deltas (`StreamItem::Delta`) are handled in the sink, not here.
pub fn event_to_session_update(ev: &Event) -> Option<SessionUpdate> {
    let Some(event::Body::Payload(p)) = &ev.body else {
        return None;
    };
    kind_to_session_update(p.kind.as_ref()?)
}

/// Render the mutation-applied durable history in source order.
///
/// `apply_cascade` is load-bearing: a redaction of either side of a tool
/// call/result pair hides its partner as well, matching the core transcript
/// fold. Replay intentionally does not run a brain or mutate the log.
pub fn replay_updates(events: &[Event]) -> Vec<SessionUpdate> {
    let mut mutations = gait_state::session::retrace::active_mutations(events);
    gait_state::session::retrace::apply_cascade(&mut mutations, events);
    gait_state::session::retrace::effective_kinds(events, &mutations)
        .filter_map(|(_, kind)| kind_to_session_update(kind))
        .collect()
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
pub fn turn_result_to_stop_reason(r: &RunResult) -> Result<StopReason, String> {
    use gait_core::proto::gait::log::v1::turn_result::Outcome;
    match r.turns.last().and_then(|turn| turn.outcome.as_ref()) {
        Some(Outcome::Done(_)) => Ok(StopReason::EndTurn),
        Some(Outcome::Cancelled(_)) => Ok(StopReason::Cancelled),
        Some(Outcome::Failed(failed)) if failed.reason == "max_steps" => {
            Ok(StopReason::MaxTurnRequests)
        }
        Some(Outcome::Failed(failed)) => Err(failed.message.clone()),
        Some(Outcome::Pause(_)) => {
            // A turn that ends genuinely awaiting interaction with no cancellation is
            // still unsupported. The cancel path never reaches here: the completion
            // block short-circuits on was_cancelled() before mapping.
            Err("turn ended awaiting interaction, which is unsupported here".into())
        }
        None => Err("run produced no final turn outcome".into()),
    }
}

/// Build the singular `tool_call` field an ACP `session/request_permission`
/// carries. Title is the item's display command; id is the gait tool_call_id.
pub fn tool_call_update(item: &PermissionItem) -> ToolCallUpdate {
    ToolCallUpdate::new(
        ToolCallId::new(item.id.clone()),
        ToolCallUpdateFields::new().title(item.cmd.clone()),
    )
}

/// Bridge one tool call's segments to a flat ACP option list plus an in-scope
/// decode map (design §4.2/§4.3). `segs` all share one `tool_call_id`.
///
/// Options (per design):
///   - Allow once      -> each seg { Allow, Once, patterns[0] }        (kind AllowOnce)
///   - Allow session   -> each seg { Allow, Session, patterns.last() } (kind AllowAlways)
///   - Reject          -> each seg { Deny }                            (kind RejectOnce)
///   - (single-segment only) one Allow-session per *middle* rung        (kind AllowAlways)
///
/// The `option_id` is an opaque ordinal; the map records exactly which
/// `ItemChoice`s selecting it yields. No semantics are encoded in the string.
pub fn build_permission_options(
    segs: &[PermissionItem],
) -> (
    Vec<PermissionOption>,
    HashMap<PermissionOptionId, Vec<ItemChoice>>,
) {
    let mut options = Vec::new();
    let mut map: HashMap<PermissionOptionId, Vec<ItemChoice>> = HashMap::new();
    let mut next = 0u32;

    let mut push = |options: &mut Vec<PermissionOption>,
                    map: &mut HashMap<PermissionOptionId, Vec<ItemChoice>>,
                    name: String,
                    kind: PermissionOptionKind,
                    choices: Vec<ItemChoice>| {
        let id = PermissionOptionId::new(next.to_string());
        next += 1;
        options.push(PermissionOption::new(id.clone(), name, kind));
        map.insert(id, choices);
    };

    let allow = |id: &str, scope: GrantScope, pat: &str| ItemChoice {
        id: id.to_string(),
        decision: Decision::Allow,
        scope,
        pattern: Some(pat.to_string()),
        agent: None,
    };
    let deny = |id: &str| ItemChoice {
        id: id.to_string(),
        decision: Decision::Deny,
        scope: GrantScope::Once, // ignored for Deny
        pattern: None,
        agent: None,
    };

    // A segment always has at least one rung; guard defensively.
    let narrow = |s: &PermissionItem| s.patterns.first().cloned().unwrap_or_else(|| s.cmd.clone());
    let broad = |s: &PermissionItem| s.patterns.last().cloned().unwrap_or_else(|| s.cmd.clone());

    // Canonical: allow once (narrowest per seg).
    push(
        &mut options,
        &mut map,
        "Allow once".to_string(),
        PermissionOptionKind::AllowOnce,
        segs.iter()
            .map(|s| allow(&s.id, GrantScope::Once, &narrow(s)))
            .collect(),
    );
    // Canonical: allow this session (broadest per seg).
    push(
        &mut options,
        &mut map,
        "Allow this session".to_string(),
        PermissionOptionKind::AllowAlways,
        segs.iter()
            .map(|s| allow(&s.id, GrantScope::Session, &broad(s)))
            .collect(),
    );
    // Canonical: reject.
    push(
        &mut options,
        &mut map,
        "Reject".to_string(),
        PermissionOptionKind::RejectOnce,
        segs.iter().map(|s| deny(&s.id)).collect(),
    );

    // Session-granular: single-segment only (per-rung is ambiguous for compounds).
    if let [seg] = segs {
        let n = seg.patterns.len();
        // Middle rungs: indices 1..n-1 (narrowest is the Once option, broadest the
        // Session option already offered above).
        for rung in seg.patterns.iter().take(n.saturating_sub(1)).skip(1) {
            push(
                &mut options,
                &mut map,
                format!("Allow `{rung}` this session"),
                PermissionOptionKind::AllowAlways,
                vec![allow(&seg.id, GrantScope::Session, rung)],
            );
        }
    }

    (options, map)
}

/// Drives one ACP-initiated turn, including the Phase-3 permission loop.
pub struct AcpTurnSource {
    first: Option<Vec<gait_core::proto::gait::log::v1::event::Body>>,
    cx: ConnectionTo<Client>,
    session_id: SessionId,
    sid: String,
    gait_dir: PathBuf,
    /// Shared per-turn cancel signal (a clone of the SessionEntry token). Tripped
    /// by the cancel handler and, on the compliant Cancelled outcome, by us.
    cancel: tokio_util::sync::CancellationToken,
}

impl AcpTurnSource {
    pub fn new(
        text: String,
        repos: Repos,
        codebase: PathBuf,
        cx: ConnectionTo<Client>,
        session_id: SessionId,
        gait_dir: PathBuf,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Self {
        let sid = session_id.0.to_string();
        Self {
            first: Some(user_turn_events(&repos, &codebase, text)),
            cx,
            session_id,
            sid,
            gait_dir,
            cancel,
        }
    }
}

#[async_trait::async_trait(?Send)]
impl TurnSource for AcpTurnSource {
    async fn next(&mut self) -> Option<TurnStart> {
        // First call: feed the user input.
        if let Some(bodies) = self.first.take() {
            return Some(TurnStart::Input(bodies));
        }

        // Post-turn: is the turn waiting for permission?
        let log_path = gait_core::session::live::log_path(&self.gait_dir, &self.sid);
        let items = match session_state(&log_path) {
            Ok(SessionState::AwaitingPermission { items }) => items,
            _ => return None, // terminal / idle -> the run loop finalizes.
        };

        // Ask round: one request per distinct tool_call_id; accumulate choices.
        let mut choices: Vec<ItemChoice> = Vec::new();
        // Preserve first-seen order of ids (grouping is by shared tool_call_id).
        let mut seen: Vec<String> = Vec::new();
        for it in &items {
            if !seen.iter().any(|s| s == &it.id) {
                seen.push(it.id.clone());
            }
        }
        for id in &seen {
            let segs: Vec<_> = items.iter().filter(|it| &it.id == id).cloned().collect();
            let (options, map) = build_permission_options(&segs);
            let tool_call = tool_call_update(&segs[0]);
            let req = RequestPermissionRequest::new(self.session_id.clone(), tool_call, options);
            let resp = tokio::select! {
                biased;
                // Our own session/cancel tripped the token: abandon. The completion
                // path sees was_cancelled() and resolves StopReason::Cancelled.
                _ = self.cancel.cancelled() => return None,
                r = self.cx.send_request(req).block_task() => match r {
                    Ok(r) => r,
                    // Transport/connection error: abandon the turn (drain reaps it).
                    Err(_) => return None,
                },
            };
            match resp.outcome {
                RequestPermissionOutcome::Selected(SelectedPermissionOutcome {
                    option_id, ..
                }) => match map.get(&option_id) {
                    Some(cs) => choices.extend(cs.iter().cloned()),
                    // Client echoed an option we never offered -> protocol violation.
                    // Bail; the run loop finalizes and the prompt resolves as an error
                    // (no reply written). NOT a cancellation — leave the token alone.
                    None => return None,
                },
                // The client cancelled while we awaited: trip the shared token so the
                // completion path reports StopReason::Cancelled, then finalize (no reply).
                RequestPermissionOutcome::Cancelled => {
                    self.cancel.cancel();
                    return None;
                }
                // Future-proof: any new outcome is treated like cancelled.
                _ => {
                    self.cancel.cancel();
                    return None;
                }
            }
        }

        // Write the reply (a log append — never a turn) and resume. Completeness is
        // guaranteed by construction (choices cover every pending id). A reply error
        // is an internal bug; abandon rather than half-answer.
        match gait_ops::permission::reply(&self.gait_dir, &self.sid, choices) {
            Ok(_) => Some(TurnStart::Resume),
            Err(_) => None,
        }
    }
}

/// Streams settled turn events out as ACP `session/update` notifications and
/// captures the final `TurnResult`. `cx.send_notification` is synchronous, so
/// `item()` sends inline — no bridge task, and FIFO ordering is preserved.
pub struct AcpTurnSink {
    cx: ConnectionTo<Client>,
    session_id: SessionId,
    result: Option<RunResult>,
}

impl AcpTurnSink {
    pub fn new(cx: ConnectionTo<Client>, session_id: SessionId) -> Self {
        Self {
            cx,
            session_id,
            result: None,
        }
    }
    pub fn take_result(&mut self) -> Option<RunResult> {
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
    fn result(&mut self, r: &RunResult) {
        self.result = Some(r.clone());
    }
    fn session_id(&mut self, _id: &str) {}
    fn diagnostic(&mut self, _level: Diag, _msg: &str) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::{
        ContentBlock, PermissionOptionKind, SessionUpdate, StopReason, TextContent, ToolCallId,
    };
    use gait_core::proto::events;
    use gait_core::proto::gait::log::v1::{Event, PauseReason};
    use gait_ops::permission::GrantScope;

    fn run_with(outcome: gait_core::proto::gait::log::v1::turn_result::Outcome) -> RunResult {
        RunResult {
            run_id: "r1".into(),
            turns: vec![gait_core::proto::gait::log::v1::TurnResult {
                turn_id: "t1".into(),
                outcome: Some(outcome),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

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
    fn user_text_maps_to_user_message_chunk() {
        let ev = event(events::user_text("hello".into()));
        match event_to_session_update(&ev) {
            Some(SessionUpdate::UserMessageChunk(chunk)) => {
                assert!(matches!(&chunk.content, ContentBlock::Text(t) if t.text == "hello"));
            }
            other => panic!("expected UserMessageChunk, got {other:?}"),
        }
    }

    #[test]
    fn replay_updates_preserve_renderable_event_order() {
        let events = vec![
            event(events::user_text("question".into())),
            event(events::assistant_text("answer".into())),
        ];
        let updates = replay_updates(&events);
        assert!(matches!(
            &updates[..],
            [
                SessionUpdate::UserMessageChunk(_),
                SessionUpdate::AgentMessageChunk(_)
            ]
        ));
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
        let r = run_with(gait_core::proto::gait::log::v1::turn_result::Outcome::Done(
            Default::default(),
        ));
        assert_eq!(turn_result_to_stop_reason(&r), Ok(StopReason::EndTurn));
    }

    #[test]
    fn cancelled_maps_to_cancelled() {
        assert_eq!(
            turn_result_to_stop_reason(&run_with(
                gait_core::proto::gait::log::v1::turn_result::Outcome::Cancelled(Default::default())
            )),
            Ok(StopReason::Cancelled)
        );
    }

    #[test]
    fn max_steps_maps_to_max_turn_requests() {
        let r = run_with(
            gait_core::proto::gait::log::v1::turn_result::Outcome::Failed(
                gait_core::proto::gait::log::v1::TurnFailed {
                    reason: "max_steps".into(),
                    message: "hit cap".into(),
                },
            ),
        );
        assert_eq!(
            turn_result_to_stop_reason(&r),
            Ok(StopReason::MaxTurnRequests)
        );
    }

    #[test]
    fn error_maps_to_jsonrpc_error() {
        let r = run_with(
            gait_core::proto::gait::log::v1::turn_result::Outcome::Failed(
                gait_core::proto::gait::log::v1::TurnFailed {
                    reason: "error".into(),
                    message: "boom".into(),
                },
            ),
        );
        assert_eq!(turn_result_to_stop_reason(&r), Err("boom".into()));
    }

    #[test]
    fn interaction_required_maps_to_jsonrpc_error() {
        // Permissions/Clarify/Inform/Unlock are the Phase-3 seam: no interactive
        // support yet, so surface a loud error rather than a fake completion.
        for reason in [
            PauseReason::Permissions,
            PauseReason::Clarify,
            PauseReason::Inform,
            PauseReason::SandboxUnlock,
        ] {
            let run = run_with(
                gait_core::proto::gait::log::v1::turn_result::Outcome::Pause(
                    gait_core::proto::gait::log::v1::TurnPause {
                        reason: reason as i32,
                        ..Default::default()
                    },
                ),
            );
            assert!(turn_result_to_stop_reason(&run).is_err());
        }
    }

    fn item(id: &str, cmd: &str, patterns: &[&str]) -> PermissionItem {
        PermissionItem {
            id: id.to_string(),
            cmd: cmd.to_string(),
            reason: String::new(),
            patterns: patterns.iter().map(|s| s.to_string()).collect(),
            tool_name: "shell".to_string(),
            tool_args: None,
            preview: None,
        }
    }

    #[test]
    fn single_segment_request_offers_canonical_plus_ladder() {
        // A 3-rung ladder: narrowest "cargo build", mid "cargo *", broadest "*".
        let segs = vec![item("tc1", "cargo build", &["cargo build", "cargo *", "*"])];
        let (options, map) = build_permission_options(&segs);

        // Canonical three + one middle-rung session option = 4.
        assert_eq!(options.len(), 4, "3 canonical + 1 middle rung");

        // Every option decodes.
        for opt in &options {
            assert!(
                map.contains_key(&opt.option_id),
                "option {:?} must decode",
                opt.option_id
            );
        }

        // Allow-once (kind AllowOnce) -> Once scope, narrowest pattern.
        let once = options
            .iter()
            .find(|o| o.kind == PermissionOptionKind::AllowOnce)
            .unwrap();
        let once_choices = &map[&once.option_id];
        assert_eq!(once_choices.len(), 1);
        assert_eq!(once_choices[0].decision, Decision::Allow);
        assert_eq!(once_choices[0].scope, GrantScope::Once);
        assert_eq!(once_choices[0].pattern.as_deref(), Some("cargo build"));

        // Reject (kind RejectOnce) -> Deny.
        let reject = options
            .iter()
            .find(|o| o.kind == PermissionOptionKind::RejectOnce)
            .unwrap();
        assert_eq!(map[&reject.option_id][0].decision, Decision::Deny);

        // The Session options (kind AllowAlways) carry Session scope; the broadest
        // one grants "*", and the ladder as a whole covers the mid rung "cargo *".
        let session_patterns: std::collections::HashSet<String> = options
            .iter()
            .filter(|o| o.kind == PermissionOptionKind::AllowAlways)
            .flat_map(|o| map[&o.option_id].iter())
            .inspect(|c| assert_eq!(c.scope, GrantScope::Session))
            .filter_map(|c| c.pattern.clone())
            .collect();
        assert!(
            session_patterns.contains("*"),
            "broadest session grant present"
        );
        assert!(
            session_patterns.contains("cargo *"),
            "mid-rung session grant present"
        );
    }

    #[test]
    fn compound_segments_offer_canonical_only() {
        // Two segments sharing one tool_call_id -> compound command, all-or-nothing.
        let segs = vec![
            item("tc9", "cargo build", &["cargo build", "cargo *"]),
            item("tc9", "git push", &["git push", "git *"]),
        ];
        let (options, map) = build_permission_options(&segs);

        // Exactly the canonical three — no per-rung explosion for multi-segment.
        assert_eq!(options.len(), 3, "compound requests get canonical-only");

        // "Allow this session" expands to ONE choice per segment (both broadest),
        // proving all-or-nothing coverage of the shared id.
        let session = options
            .iter()
            .find(|o| o.kind == PermissionOptionKind::AllowAlways)
            .unwrap();
        let choices = &map[&session.option_id];
        assert_eq!(choices.len(), 2, "one choice per segment");
        assert!(
            choices
                .iter()
                .all(|c| c.id == "tc9" && c.scope == GrantScope::Session)
        );
        let pats: std::collections::HashSet<&str> = choices
            .iter()
            .filter_map(|c| c.pattern.as_deref())
            .collect();
        assert!(
            pats.contains("cargo *") && pats.contains("git *"),
            "each seg broadest"
        );
    }

    #[test]
    fn reject_decodes_to_deny_for_all_segments() {
        let segs = vec![
            item("tc9", "cargo build", &["cargo build"]),
            item("tc9", "git push", &["git push"]),
        ];
        let (options, map) = build_permission_options(&segs);
        let reject = options
            .iter()
            .find(|o| o.kind == PermissionOptionKind::RejectOnce)
            .unwrap();
        let choices = &map[&reject.option_id];
        assert_eq!(choices.len(), 2);
        assert!(choices.iter().all(|c| c.decision == Decision::Deny));
    }

    #[test]
    fn tool_call_update_carries_id_and_title() {
        let it = item("tc1", "cargo build", &["cargo build"]);
        let upd = tool_call_update(&it);
        assert_eq!(upd.tool_call_id, ToolCallId::new("tc1"));
        assert_eq!(upd.fields.title.as_deref(), Some("cargo build"));
    }
}

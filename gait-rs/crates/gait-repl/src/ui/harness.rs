//! #[cfg(test)] rendering harness: feed a scripted brain turn through the real
//! ReplSink/ReplBuilder path and collect the structured ReplViewItems rendered.
use crate::repl::ScriptedReader;
use crate::theme::Theme;
use crate::ui::builder::ReplBuilder;
use crate::ui::view::ReplViewItem;
use gait_core::proto::gait::log::v1 as pb;
use gait_core::proto::{envelope, events};

fn builder() -> ReplBuilder {
    ReplBuilder::new(Box::new(ScriptedReader::new([])), Theme::dark())
}

fn ev(body: pb::event::Body) -> pb::StreamItem {
    pb::StreamItem {
        item: Some(pb::stream_item::Item::Event(envelope::mint(
            body, None, 0, 0,
        ))),
    }
}

fn delta(s: &str) -> pb::StreamItem {
    pb::StreamItem {
        item: Some(pb::stream_item::Item::Delta(pb::TextDelta {
            content: s.into(),
        })),
    }
}

fn drive(b: &mut ReplBuilder, items: &[pb::StreamItem]) -> Vec<ReplViewItem> {
    b.begin_turn();
    let mut painted = Vec::new();
    for it in items {
        painted.extend(b.handle_stream_item(it.clone()).painted);
    }
    painted.extend(b.flush());
    painted
}

#[test]
fn plain_reply_renders_a_visible_block() {
    let mut b = builder();
    let painted = drive(&mut b, &[delta("Hello! How can I help?\n"), delta("\n")]);
    assert!(
        painted
            .iter()
            .any(|v| matches!(v, ReplViewItem::Block { content, .. }
            if content.contains("Hello! How can I help?"))),
        "a plain streamed reply must render a Block: {painted:?}"
    );
}

#[test]
fn assistant_text_event_alone_renders_nothing() {
    // Documents the delta-reliance contract: an AssistantText event with no
    // deltas produces no Block. This is WHY stream.rs must publish deltas.
    let mut b = builder();
    let painted = drive(&mut b, &[ev(events::assistant_text("silent".into()))]);
    assert!(
        !painted
            .iter()
            .any(|v| matches!(v, ReplViewItem::Block { .. })),
        "AssistantText alone must not render a Block (delta-reliance contract): {painted:?}"
    );
}

#[test]
fn tool_call_then_result_renders_tool_result() {
    let mut b = builder();
    let call = ev(events::tool_call(
        "c1".into(),
        "read".into(),
        &serde_json::json!({"file_path":"a.rs"}),
    ));
    let result = ev(events::tool_result("c1".into(), "ok".into(), None, None));
    let painted = drive(&mut b, &[call, result]);
    assert!(
        painted.iter().any(|v| matches!(v, ReplViewItem::ToolResult { ok: true, name, .. } if name.contains("read"))),
        "tool call+result must render a ToolResult: {painted:?}"
    );
}

#[test]
fn permission_event_sets_interaction_pending() {
    use crate::ui::state::Pending;
    let mut b = builder();
    let item = pb::PermissionItem {
        id: "p1".into(),
        cmd: "glob **/*.rs".into(),
        reason: String::new(),
        patterns: vec!["glob".into()],
        tool_name: "glob".into(),
        tool_args: None,
        preview: None,
    };
    drive(&mut b, &[ev(events::permissions(&[item]))]);
    assert!(
        matches!(b.state().peek_pending(), Pending::Permission(items) if items.len() == 1),
        "permission event must queue a Permission interaction"
    );
}

#[test]
fn clarify_event_sets_interaction_pending() {
    use crate::ui::state::Pending;
    let mut b = builder();
    let item = pb::ClarifyItem {
        id: "q1".into(),
        content: "which?".into(),
        why: "ambiguous".into(),
        kind: pb::ClarifyKind::Is as i32,
        options: vec![],
    };
    drive(&mut b, &[ev(events::clarify(&[item]))]);
    assert!(matches!(b.state().peek_pending(), Pending::Clarify(i) if i.len() == 1));
}

#[test]
fn error_event_renders_error_item() {
    let mut b = builder();
    let painted = drive(&mut b, &[ev(events::error("boom".into(), "tool"))]);
    assert!(
        painted
            .iter()
            .any(|v| matches!(v, ReplViewItem::Error { message } if message.contains("boom"))),
        "error event must render an Error item: {painted:?}"
    );
}

#[test]
fn turn_complete_renders_turn_summary() {
    use gait_core::proto::gait::log::v1::TurnTokenUsage;
    let mut b = builder();
    let tc = events::turn_complete(events::TurnCompleteParams {
        usage: TurnTokenUsage {
            input: 0,
            output: 42,
            cache_read: 0,
            cache_write: 0,
        },
        ..Default::default()
    });
    let painted = drive(&mut b, &[ev(tc)]);
    assert!(
        painted.iter().any(
            |v| matches!(v, ReplViewItem::TurnSummary { output_tokens, .. } if *output_tokens == 42)
        ),
        "turn_complete must render a TurnSummary: {painted:?}"
    );
}

// Matrix row 7 (structured_done → DoneMessage/StructuredResult) is intentionally
// not duplicated here per the plan — it is covered by the existing state.rs tests
// `structured_done_message_emitted_as_done_message` and
// `structured_done_result_emitted_as_json_only_box`.

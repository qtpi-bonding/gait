use super::*;
use crate::proto::gait::log::v1::event;
use tempfile::TempDir;

/// The substantive payload kind for a proto event, for test assertions.
fn k(e: &Event) -> &payload::Kind {
    match &e.body {
        Some(event::Body::Payload(p)) => p.kind.as_ref().expect("kind set"),
        other => panic!("expected payload body, got {other:?}"),
    }
}

use crate::proto::events;
use crate::proto::wire;

#[test]
fn append_and_read_roundtrip() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("log.jsonl");
    wire::append(&path, events::user_text("hello".into(), None)).unwrap();
    wire::append(&path, events::assistant_text("world".into())).unwrap();
    let events = read_events(&path).unwrap();
    assert_eq!(events.len(), 2);
    assert!(matches!(k(&events[0]), payload::Kind::UserText(u) if u.content == "hello"));
    assert!(matches!(k(&events[1]), payload::Kind::AssistantText(a) if a.content == "world"));
}

#[test]
fn init_session_writes_session_start() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("log.jsonl");
    init_session(InitSessionOpts {
        log_path: path.clone(),
        id: "s1234".into(),
        model: "anthropic/claude-sonnet-4-6".into(),
        codebase_hash: "abc123".into(),
        codebase_dirty: false,
    })
    .unwrap();
    let events = read_events(&path).unwrap();
    assert_eq!(events.len(), 1);
    assert!(matches!(k(&events[0]), payload::Kind::SessionStart(s) if s.session_id == "s1234"));
}

fn mint(body: event::Body) -> Event {
    crate::proto::envelope::mint(body, None, 0, 0)
}

#[test]
fn count_turn_complete_counts_correctly() {
    let usage = pb::TurnTokenUsage {
        input: 1,
        output: 1,
        cache_read: 0,
        cache_write: 0,
    };
    let events = vec![
        mint(events::user_text("x".into(), None)),
        mint(events::turn_complete(events::TurnCompleteParams {
            usage,
            ..Default::default()
        })),
        mint(events::user_text("y".into(), None)),
        mint(events::turn_complete(events::TurnCompleteParams {
            usage,
            ..Default::default()
        })),
    ];
    assert_eq!(count_turn_complete(&events), 2);
}

#[test]
fn last_compaction_index_finds_last() {
    let cu = pb::CompactionTokenUsage {
        input: 10,
        output: 5,
    };
    let events = vec![
        mint(events::user_text("x".into(), None)),
        mint(events::summary("s1".into(), 1, cu)),
        mint(events::user_text("y".into(), None)),
    ];
    assert_eq!(last_compaction_index(&events), Some(1));
}

#[test]
fn user_turn_from_field_roundtrips() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("log.jsonl");
    wire::append(
        &path,
        events::user_text("do the thing".into(), Some("agent:s-parent123".into())),
    )
    .unwrap();
    let events = read_events(&path).unwrap();
    assert_eq!(events.len(), 1);
    if let payload::Kind::UserText(u) = k(&events[0]) {
        assert_eq!(u.content, "do the thing");
        assert_eq!(u.from.as_deref(), Some("agent:s-parent123"));
    } else {
        panic!("expected UserTurn");
    }
}

#[test]
fn workflow_events_roundtrip() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("t.jsonl");
    wire::append(
        &path,
        wire::raw(
            "workflow_start",
            serde_json::json!({
                "workflow_id": "wf1", "name": "test",
                "yaml_path": "workflows/test.yaml", "args": [],
            }),
        ),
    )
    .unwrap();
    wire::append(
        &path,
        wire::raw(
            "node_start",
            serde_json::json!({
                "workflow_id": "wf1", "node_id": "n1", "node_type": "shell", "attempt": 1,
            }),
        ),
    )
    .unwrap();
    wire::append(
        &path,
        wire::raw(
            "node_complete",
            serde_json::json!({
                "workflow_id": "wf1", "node_id": "n1", "output": "ok",
                "exit_code": 0, "json_output": null, "routed_to": null,
            }),
        ),
    )
    .unwrap();
    let events = read_events(&path).unwrap();
    assert_eq!(events.len(), 3);
    // Workflow events store the original tagged JSON verbatim in a RawEvent.
    match k(&events[0]) {
        payload::Kind::Raw(r) => {
            assert_eq!(r.r#type, "workflow_start");
            let v = serde_json::to_value(r.payload.as_ref().unwrap()).unwrap();
            assert_eq!(v["workflow_id"], "wf1");
        }
        other => panic!("expected Raw, got {other:?}"),
    }
    match k(&events[2]) {
        payload::Kind::Raw(r) => {
            assert_eq!(r.r#type, "node_complete");
            let v = serde_json::to_value(r.payload.as_ref().unwrap()).unwrap();
            // pbjson Struct stores numbers as f64 → 0 round-trips as 0.0.
            assert_eq!(v["exit_code"].as_f64(), Some(0.0));
        }
        other => panic!("expected Raw, got {other:?}"),
    }
}

#[test]
fn all_event_bodies_serialize_roundtrip() {
    let usage = pb::TurnTokenUsage {
        input: 1,
        output: 1,
        cache_read: 0,
        cache_write: 0,
    };
    let bodies: Vec<event::Body> = vec![
        events::session_start("s1".into(), None),
        events::user_text("t".into(), None),
        events::assistant_text("a".into()),
        events::tool_call("i".into(), "read".into(), &serde_json::json!({})),
        events::tool_result("i".into(), "r".into(), None, None),
        events::error("e".into(), "llm"),
        events::permissions(&[]),
        events::permissions_reply("user".into(), &[]),
        events::turn_complete(events::TurnCompleteParams {
            usage,
            message: "s".into(),
            ..Default::default()
        }),
    ];
    for body in bodies {
        let ev = mint(body);
        let json = serde_json::to_string(&ev).unwrap();
        let back: Event = serde_json::from_str(&json).unwrap();
        assert!(back.body.is_some(), "missing body in: {json}");
        assert!(
            crate::proto::envelope::verify_hash(&back),
            "hash must verify: {json}"
        );
    }
}

#[test]
fn emit_writes_to_transcript() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("log.jsonl");
    wire::append(&path, events::assistant_text("hello emit".into())).unwrap();
    let events = read_events(&path).unwrap();
    assert_eq!(events.len(), 1);
    assert!(matches!(k(&events[0]), payload::Kind::AssistantText(a) if a.content == "hello emit"));
}
#[test]
fn log_path_is_log_jsonl() {
    let dir = tempfile::TempDir::new().unwrap();
    assert_eq!(
        log_path(dir.path(), "s1"),
        dir.path().join("sessions").join("s1").join("log.jsonl"),
    );
}

#[test]
fn permission_preview_survives_proto_round_trip() {
    use crate::proto::gait::log::v1::{self as pb, payload};
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("log.jsonl");

    let item = pb::PermissionItem {
        id: "tc-rt".into(),
        cmd: "edit src/lib.rs".into(),
        preview: Some("-old\n+new".into()),
        ..Default::default()
    };

    wire::append(&path, events::permissions(&[item])).unwrap();

    let events = read_events(&path).unwrap();
    assert_eq!(events.len(), 1);
    match k(&events[0]) {
        payload::Kind::Permissions(p) => {
            assert_eq!(p.items[0].preview.as_deref(), Some("-old\n+new"));
        }
        other => panic!("expected Permissions kind, got {other:?}"),
    }
}

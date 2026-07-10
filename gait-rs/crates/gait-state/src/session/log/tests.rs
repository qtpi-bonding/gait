use super::*;
use crate::proto::gait::log::v1::event;
use tempfile::TempDir;

// Re-export for integration assertions
use crate::session::retrace::active_unlock_state;

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
        assigned_by: None,
        codebase_path: None,
        notify_done: false,
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
        events::session_start("s1".into(), None, None, false),
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

#[test]
fn session_state_awaiting_unlock() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("log.jsonl");

    // Create a log ending with an unlock event
    let items = vec![pb::UnlockItem {
        id: "lock-1".into(),
        tool_name: "edit".into(),
        resource: "/path/to/file".into(),
        dimension: "write".into(),
        preview: None,
    }];
    wire::append(&path, events::unlock(&items)).unwrap();

    let state = session_state(&path).unwrap();
    match state {
        SessionState::AwaitingUnlock {
            items: result_items,
        } => {
            assert_eq!(result_items.len(), 1);
            assert_eq!(result_items[0].id, "lock-1");
            assert_eq!(result_items[0].tool_name, "edit");
        }
        other => panic!("expected AwaitingUnlock state, got {other:?}"),
    }
}

#[test]
fn session_state_unlock_reply_returns_idle() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("log.jsonl");

    // Create a log with unlock then unlock_reply
    let items = vec![pb::UnlockItem {
        id: "lock-1".into(),
        tool_name: "edit".into(),
        resource: "/path/to/file".into(),
        dimension: "write".into(),
        preview: None,
    }];
    wire::append(&path, events::unlock(&items)).unwrap();

    let reply_items = vec![pb::UnlockReplyItem {
        id: "lock-1".into(),
        decision: pb::Decision::Allow as i32,
        scope: pb::Scope::Once as i32,
    }];
    wire::append(&path, events::unlock_reply("user".into(), &reply_items)).unwrap();

    let state = session_state(&path).unwrap();
    assert!(
        matches!(state, SessionState::Idle),
        "expected Idle after unlock_reply, got {state:?}"
    );
}

#[test]
fn unlock_once_grant_round_trip_no_config_change() {
    use std::path::PathBuf;

    let dir = TempDir::new().unwrap();
    let gait_dir = dir.path();
    let session_id = "test-unlock-rt".to_string();
    let tx_path = crate::session::live::log_path(gait_dir, &session_id);
    std::fs::create_dir_all(tx_path.parent().unwrap()).unwrap();

    // Step 1: Write an Unlock event (simulating pause_for_unlock)
    let resource_path = "/some/external/path.txt";
    let unlock_item = pb::UnlockItem {
        id: "unlock-rt-1".to_string(),
        tool_name: "write".to_string(),
        resource: resource_path.to_string(),
        dimension: "write".to_string(),
        preview: None,
    };
    wire::append(&tx_path, events::unlock(&[unlock_item])).unwrap();

    // Step 2: Verify session is AwaitingUnlock
    assert!(matches!(
        session_state(&tx_path).unwrap(),
        SessionState::AwaitingUnlock { .. }
    ));

    // Step 3: Write the UnlockReply with Once scope (simulating cmd_unlock once)
    let reply_item = pb::UnlockReplyItem {
        id: "unlock-rt-1".to_string(),
        decision: pb::Decision::Allow as i32,
        scope: pb::Scope::Once as i32,
    };
    write_unlock_reply(&tx_path, "user", vec![reply_item]).unwrap();

    // Step 4: Assert gait.jsonc was NOT created (once = log only)
    assert!(
        !gait_dir.join("gait.jsonc").exists(),
        "once grant must not write gait.jsonc"
    );

    // Step 5: Assert session state is now Idle
    assert!(matches!(
        session_state(&tx_path).unwrap(),
        SessionState::Idle
    ));

    // Step 6: Assert ActiveUnlock projection contains the granted path
    let evs = read_events(&tx_path).unwrap();
    let unlock_state = active_unlock_state(&evs);
    assert!(
        unlock_state.once.contains(&PathBuf::from(resource_path)),
        "once grant should be in active_unlock.once; got: {:?}",
        unlock_state.once
    );
    assert!(unlock_state.session.is_empty());

    // Step 7: Assert that after a TurnComplete, the once grant is cleared
    let usage = pb::TurnTokenUsage {
        input: 1,
        output: 1,
        cache_read: 0,
        cache_write: 0,
    };
    wire::append(
        &tx_path,
        events::turn_complete(events::TurnCompleteParams {
            usage,
            ..Default::default()
        }),
    )
    .unwrap();
    let evs_after = read_events(&tx_path).unwrap();
    let unlock_state_after = active_unlock_state(&evs_after);
    assert!(
        unlock_state_after.once.is_empty(),
        "once grant should clear after TurnComplete"
    );
}

#[test]
fn write_unlock_reply_appends_and_state_becomes_idle() {
    let dir = TempDir::new().unwrap();
    let tx_path = dir.path().join("log.jsonl");

    // Write an Unlock event first (so session_state returns AwaitingUnlock)
    let unlock_item = pb::UnlockItem {
        id: "item-1".to_string(),
        tool_name: "write".to_string(),
        resource: "/outside/path/foo.txt".to_string(),
        dimension: "write".to_string(),
        preview: None,
    };
    wire::append(&tx_path, events::unlock(&[unlock_item])).unwrap();
    // Verify session is awaiting unlock
    assert!(matches!(
        session_state(&tx_path).unwrap(),
        SessionState::AwaitingUnlock { .. }
    ));

    // Write the reply
    let reply_item = pb::UnlockReplyItem {
        id: "item-1".to_string(),
        decision: pb::Decision::Allow as i32,
        scope: pb::Scope::Once as i32,
    };
    write_unlock_reply(&tx_path, "user", vec![reply_item]).unwrap();

    // State should now be Idle
    assert!(matches!(
        session_state(&tx_path).unwrap(),
        SessionState::Idle
    ));
    // Log should have 2 events
    let events = read_events(&tx_path).unwrap();
    assert_eq!(events.len(), 2);
    assert!(matches!(
        crate::proto::event_kind(&events[1]),
        Some(crate::proto::gait::log::v1::payload::Kind::UnlockReply(_))
    ));
}

#[test]
fn write_registry_appends_when_session_quiescent() {
    let dir = TempDir::new().unwrap();
    let tx_path = dir.path().join("log.jsonl");
    // Quiescent: no lock file at all in the session dir.
    write_registry(&tx_path, dir.path(), "skill", "tdd", true).unwrap();

    let events = read_events(&tx_path).unwrap();
    assert_eq!(events.len(), 1);
    match crate::proto::event_kind(&events[0]) {
        Some(payload::Kind::Registry(r)) => {
            assert_eq!(r.name, "tdd");
            assert!(r.listed);
        }
        other => panic!("expected Registry event, got {other:?}"),
    }
}

#[test]
fn write_toggle_appends_when_session_quiescent() {
    let dir = TempDir::new().unwrap();
    let tx_path = dir.path().join("log.jsonl");
    // Quiescent: no lock file at all in the session dir.
    write_toggle(&tx_path, dir.path(), "skill", "tdd", true).unwrap();

    let events = read_events(&tx_path).unwrap();
    assert_eq!(events.len(), 1);
    match crate::proto::event_kind(&events[0]) {
        Some(payload::Kind::Toggle(t)) => {
            assert_eq!(t.name, "tdd");
            assert!(t.enabled);
        }
        other => panic!("expected Toggle event, got {other:?}"),
    }
}

#[test]
fn write_toggle_rejected_when_session_live() {
    let dir = TempDir::new().unwrap();
    let tx_path = dir.path().join("log.jsonl");
    // Fabricate a "live" lock: current process PID (guaranteed alive during the test).
    std::fs::write(dir.path().join("lock"), std::process::id().to_string()).unwrap();

    let result = write_toggle(&tx_path, dir.path(), "skill", "tdd", true);
    assert!(
        matches!(result, Err(SessionError::SessionRunning)),
        "expected SessionRunning error, got {result:?}"
    );
    // Nothing should have been appended.
    assert!(
        !tx_path.exists() || read_events(&tx_path).unwrap().is_empty(),
        "no event should be appended when session is live"
    );
}

#[test]
fn write_registry_rejected_when_session_live() {
    let dir = TempDir::new().unwrap();
    let tx_path = dir.path().join("log.jsonl");
    // Fabricate a "live" lock: current process PID (guaranteed alive during the test).
    std::fs::write(dir.path().join("lock"), std::process::id().to_string()).unwrap();

    let result = write_registry(&tx_path, dir.path(), "skill", "tdd", true);
    assert!(
        matches!(result, Err(SessionError::SessionRunning)),
        "expected SessionRunning error, got {result:?}"
    );
    // Nothing should have been appended.
    assert!(
        !tx_path.exists() || read_events(&tx_path).unwrap().is_empty(),
        "no event should be appended when session is live"
    );
}

#[test]
fn init_session_writes_codebase_path_into_session_start() {
    use crate::proto::gait::log::v1::{event, payload};
    let dir = TempDir::new().unwrap();
    let log_path = dir.path().join("log.jsonl");
    super::init_session(super::InitSessionOpts {
        log_path: log_path.clone(),
        id: "s1234567".to_string(),
        model: String::new(),
        codebase_hash: String::new(),
        codebase_dirty: false,
        assigned_by: Some("sparent0".to_string()),
        codebase_path: Some("/code".to_string()),
        notify_done: false,
    })
    .unwrap();
    let events = super::read_events(&log_path).unwrap();
    let s = events.iter().find_map(|e| match &e.body {
        Some(event::Body::Payload(p)) => match &p.kind {
            Some(payload::Kind::SessionStart(s)) => Some(s.clone()),
            _ => None,
        },
        _ => None,
    });
    let s = s.expect("SessionStart present");
    assert_eq!(s.codebase_path.as_deref(), Some("/code"));
    assert_eq!(s.assigned_by.as_deref(), Some("sparent0"));
}

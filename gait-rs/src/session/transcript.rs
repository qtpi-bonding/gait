use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::proto::gait::log::v1::{event, payload, Event};

// ── Supporting types ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserTurnContext {
    pub codebase_hash: Option<String>,
    pub codebase_dirty: Option<bool>,
    pub dirty_files: Option<Vec<String>>,
    pub injected_files: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnUsage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnOutput {
    pub message: String,
    pub result: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionUsage {
    pub input: u64,
    pub output: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClarifyItem {
    pub id: String,
    pub content: String,
    pub why: String,
    pub kind: String, // "is" | "ought"
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClarifyReplyItem {
    pub id: String,
    pub content: String,
    pub reasoning: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClarifyConvergenceItem {
    pub id: String,
    pub proposal: String,
    pub response: Option<String>,
    pub consistent: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InformItem {
    pub id: String,
    pub content: String,
    pub context: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InformReplyItem {
    pub id: String,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InformConvergenceItem {
    pub id: String,
    pub proposal: String,
    pub response: Option<String>,
    pub consistent: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionItem {
    pub id: String,
    pub cmd: String,
    pub reason: String,
    #[serde(default)]
    pub patterns: Vec<String>,
    #[serde(default)]
    pub tool_name: String,
    #[serde(default)]
    pub tool_args: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionsReplyItem {
    pub id: String,
    pub decision: String, // "approve" | "deny" | "instruct"
    pub text: Option<String>,
    #[serde(default)]
    pub scope: Option<String>, // "once" | "agent" | "global" — None = legacy
}

// ── JSONL helpers ─────────────────────────────────────────────────────────────

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub fn read_events(path: &Path) -> anyhow::Result<Vec<Event>> {
    crate::proto::wire::read_events(path)
}

pub fn last_event(path: &Path) -> anyhow::Result<Option<Event>> {
    crate::proto::wire::last_event(path)
}

/// The substantive payload kind for an event, if it carries one.
fn kind(e: &Event) -> Option<&payload::Kind> {
    match &e.body {
        Some(event::Body::Payload(p)) => p.kind.as_ref(),
        _ => None,
    }
}

pub fn count_turn_complete(events: &[Event]) -> usize {
    events.iter().filter(|e| matches!(kind(e), Some(payload::Kind::TurnComplete(_)))).count()
}

pub fn last_compaction_index(events: &[Event]) -> Option<usize> {
    events.iter().rposition(|e| matches!(kind(e), Some(payload::Kind::Compaction(_))))
}

pub struct InitSessionOpts {
    pub transcript_path: PathBuf,
    pub id: String,
    pub model: String,
    pub codebase_hash: String,
    pub codebase_dirty: bool,
}

// ── Session state introspection ─────────────────────────────────────────────

/// Snapshot of where a session is right now, derived from the transcript tail.
/// Used by external runners (CLI, REPL) to detect pending approval/clarify
/// requests so they can resume or reject after a restart.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SessionState {
    /// No pending request — runner should start a new turn.
    Idle,
    /// A tool permission request is awaiting a decision.
    AwaitingPermission {
        items: Vec<crate::proto::gait::log::v1::PermissionItem>,
    },
    /// A clarify request is awaiting user answers.
    AwaitingClarify {
        items: Vec<crate::proto::gait::log::v1::ClarifyItem>,
    },
    /// An inform request is awaiting user acknowledgement.
    AwaitingInform {
        items: Vec<crate::proto::gait::log::v1::InformItem>,
    },
}

/// Walk the transcript backwards to determine the current session state.
///
/// A reply event encountered before its corresponding request means the
/// request is already resolved (e.g. resumed after a previous run wrote
/// the reply). The first unresolved request encountered wins.
pub fn session_state(tx_path: &std::path::Path) -> anyhow::Result<SessionState> {
    let events = read_events(tx_path)?;
    for event in events.iter().rev() {
        match kind(event) {
            Some(payload::Kind::Permissions(p)) => {
                return Ok(SessionState::AwaitingPermission { items: p.items.clone() });
            }
            Some(payload::Kind::PermissionsReply(_)) => {
                return Ok(SessionState::Idle);
            }
            Some(payload::Kind::Clarify(c)) => {
                return Ok(SessionState::AwaitingClarify { items: c.items.clone() });
            }
            Some(payload::Kind::ClarifyReply(_)) => {
                return Ok(SessionState::Idle);
            }
            Some(payload::Kind::Inform(i)) => {
                return Ok(SessionState::AwaitingInform { items: i.items.clone() });
            }
            Some(payload::Kind::InformReply(_)) => {
                return Ok(SessionState::Idle);
            }
            Some(payload::Kind::TurnComplete(_)) => {
                return Ok(SessionState::Idle);
            }
            _ => continue,
        }
    }
    Ok(SessionState::Idle)
}

/// Append a `PermissionsReply` event to the transcript and stdout.
///
/// `from` identifies the source of the decisions (e.g. "user", "auto-deny").
/// Each `PermissionsReplyItem.id` corresponds to a `PermissionItem.id` in
/// the preceding `Permissions` request, and `decision` is "approve" | "deny" | "instruct".
pub fn write_permissions_reply(
    tx_path: &std::path::Path,
    from: &str,
    items: Vec<PermissionsReplyItem>,
) -> anyhow::Result<()> {
    crate::proto::wire::emit(
        tx_path,
        crate::proto::events::permissions_reply(from.to_string(), &items),
    )?;
    Ok(())
}

pub fn init_session(opts: InitSessionOpts) -> anyhow::Result<()> {
    // SessionStart is identity-only; model/codebase fields on `opts` are unused here.
    crate::proto::wire::emit(
        &opts.transcript_path,
        crate::proto::events::session_start(opts.id, None),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
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
        let path = dir.path().join("transcript.jsonl");
        wire::append(&path, events::user_turn("hello".into(), None, None)).unwrap();
        wire::append(&path, events::assistant_text("world".into())).unwrap();
        let events = read_events(&path).unwrap();
        assert_eq!(events.len(), 2);
        assert!(matches!(k(&events[0]), payload::Kind::UserTurn(u) if u.content == "hello"));
        assert!(matches!(k(&events[1]), payload::Kind::AssistantText(a) if a.content == "world"));
    }

    #[test]
    fn init_session_writes_session_start() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("transcript.jsonl");
        init_session(InitSessionOpts {
            transcript_path: path.clone(),
            id: "s1234".into(),
            model: "anthropic/claude-sonnet-4-6".into(),
            codebase_hash: "abc123".into(),
            codebase_dirty: false,
        }).unwrap();
        let events = read_events(&path).unwrap();
        assert_eq!(events.len(), 1);
        assert!(matches!(k(&events[0]), payload::Kind::SessionStart(s) if s.session_id == "s1234"));
    }

    fn mint(body: event::Body) -> Event {
        crate::proto::envelope::mint(body, None)
    }

    #[test]
    fn count_turn_complete_counts_correctly() {
        let usage = TurnUsage { input: 1, output: 1, cache_read: 0, cache_write: 0 };
        let output = TurnOutput { message: "done".into(), result: None };
        let events = vec![
            mint(events::user_turn("x".into(), None, None)),
            mint(events::turn_complete(&usage, "a".into(), false, None, vec![], &output)),
            mint(events::user_turn("y".into(), None, None)),
            mint(events::turn_complete(&usage, "a".into(), false, None, vec![], &output)),
        ];
        assert_eq!(count_turn_complete(&events), 2);
    }

    #[test]
    fn last_compaction_index_finds_last() {
        let cu = CompactionUsage { input: 10, output: 5 };
        let events = vec![
            mint(events::user_turn("x".into(), None, None)),
            mint(events::compaction("s1".into(), 1, &cu)),
            mint(events::user_turn("y".into(), None, None)),
        ];
        assert_eq!(last_compaction_index(&events), Some(1));
    }

    #[test]
    fn user_turn_from_field_roundtrips() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("transcript.jsonl");
        wire::append(
            &path,
            events::user_turn("do the thing".into(), Some("agent:s-parent123".into()), None),
        )
        .unwrap();
        let events = read_events(&path).unwrap();
        assert_eq!(events.len(), 1);
        if let payload::Kind::UserTurn(u) = k(&events[0]) {
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
            wire::raw("workflow_start", serde_json::json!({
                "workflow_id": "wf1", "name": "test",
                "yaml_path": "workflows/test.yaml", "args": [],
            })),
        )
        .unwrap();
        wire::append(
            &path,
            wire::raw("node_start", serde_json::json!({
                "workflow_id": "wf1", "node_id": "n1", "node_type": "shell", "attempt": 1,
            })),
        )
        .unwrap();
        wire::append(
            &path,
            wire::raw("node_complete", serde_json::json!({
                "workflow_id": "wf1", "node_id": "n1", "output": "ok",
                "exit_code": 0, "json_output": null, "routed_to": null,
            })),
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
        let usage = TurnUsage { input: 1, output: 1, cache_read: 0, cache_write: 0 };
        let output = TurnOutput { message: "s".into(), result: None };
        let bodies: Vec<event::Body> = vec![
            events::session_start("s1".into(), None),
            events::user_turn("t".into(), None, None),
            events::assistant_text("a".into()),
            events::tool_call("i".into(), "read".into(), &serde_json::json!({})),
            events::tool_result("i".into(), "r".into(), None, None),
            events::error("e".into(), "llm"),
            events::permissions(&[]),
            events::permissions_reply("user".into(), &[]),
            events::turn_complete(&usage, "h".into(), false, None, vec![], &output),
        ];
        for body in bodies {
            let ev = mint(body);
            let json = serde_json::to_string(&ev).unwrap();
            let back: Event = serde_json::from_str(&json).unwrap();
            assert!(back.body.is_some(), "missing body in: {json}");
            assert!(crate::proto::envelope::verify_hash(&back), "hash must verify: {json}");
        }
    }

    #[test]
    fn emit_writes_to_transcript() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("transcript.jsonl");
        wire::emit(&path, events::assistant_text("hello emit".into())).unwrap();
        let events = read_events(&path).unwrap();
        assert_eq!(events.len(), 1);
        assert!(matches!(k(&events[0]), payload::Kind::AssistantText(a) if a.content == "hello emit"));
    }

    #[test]
    fn permission_item_roundtrips_patterns() {
        let item = PermissionItem {
            id: "tc-1".into(),
            cmd: "cargo test".into(),
            reason: "needs permission".into(),
            patterns: vec!["cargo *".into(), "cargo test".into()],
            tool_name: String::new(),
            tool_args: serde_json::Value::Null,
        };
        let json = serde_json::to_string(&item).unwrap();
        let back: PermissionItem = serde_json::from_str(&json).unwrap();
        assert_eq!(back.patterns, vec!["cargo *", "cargo test"]);
    }

    #[test]
    fn permission_item_carries_tool_call_for_reexecution() {
        let item = PermissionItem {
            id: "tc-1".into(),
            cmd: "cargo test".into(),
            reason: "shell requires permission".into(),
            patterns: vec!["cargo *".into(), "cargo test".into()],
            tool_name: "shell".into(),
            tool_args: serde_json::json!({"cmd": "cargo test --verbose", "reason": "run tests"}),
        };
        let json = serde_json::to_string(&item).unwrap();
        let back: PermissionItem = serde_json::from_str(&json).unwrap();
        assert_eq!(back.tool_name, "shell");
        assert_eq!(back.tool_args["cmd"], "cargo test --verbose");
    }
}

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
    pub links: Option<Vec<LinkRef>>,
    pub to: Option<String>,
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
    pub links: Option<Vec<LinkRef>>,
    pub to: Option<String>,
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
pub struct LinkRef {
    pub target: String,
    pub relation: String, // "depends_on" | "related_to" | "part_of"
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

// ── Main event enum ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AnyEvent {
    SessionStart {
        schema_version: u32,
        id: String,
        model: String,
        codebase_hash: String,
        codebase_dirty: bool,
        ts: u64,
    },
    UserTurn {
        content: String,
        from: Option<String>,
        context: Option<UserTurnContext>,
        ts: u64,
    },
    AssistantText {
        content: String,
        ts: u64,
    },
    ToolCall {
        id: String,
        name: String,
        input: serde_json::Value,
        ts: u64,
    },
    ToolResult {
        id: String,
        output: String,
        ts: u64,
    },
    CommandResult {
        call_id: String,
        cmd: String,
        output: String,
        exit_code: i32,
        duration_ms: u64,
        timed_out: bool,
        ts: u64,
    },
    Error {
        message: String,
        phase: String, // "llm" | "tool" | "git" | "init"
        ts: u64,
    },
    Clarify {
        items: Vec<ClarifyItem>,
        ts: u64,
    },
    ClarifyReply {
        items: Vec<ClarifyReplyItem>,
        ts: u64,
    },
    ClarifyConvergence {
        items: Vec<ClarifyConvergenceItem>,
        ts: u64,
    },
    Inform {
        items: Vec<InformItem>,
        ts: u64,
    },
    InformReply {
        items: Vec<InformReplyItem>,
        ts: u64,
    },
    InformConvergence {
        items: Vec<InformConvergenceItem>,
        ts: u64,
    },
    AgentUpdate {
        session_id: String,
        status: String, // "done" | "clarify" | "inform"
        summary: String,
        items: Option<serde_json::Value>,
        ts: u64,
    },
    Permissions {
        items: Vec<PermissionItem>,
        ts: u64,
    },
    PermissionsReply {
        from: String,
        items: Vec<PermissionsReplyItem>,
        ts: u64,
    },
    TurnComplete {
        usage: TurnUsage,
        codebase_hash: String,
        codebase_dirty: bool,
        /// Shadow-git checkpoint SHA for this turn (internal; powers `gait undo`).
        /// None for legacy transcripts and non-git codebases.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        code_checkpoint: Option<String>,
        /// Rendered text split into numbered blocks (paragraphs / fenced code chunks).
        /// Powers `>(N)` quote-reply in the REPL and any other UI built on this stream.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        blocks: Vec<String>,
        output: TurnOutput,
        ts: u64,
    },
    Compaction {
        summary: String,
        turns_covered: u32,
        from_ts: u64,
        to_ts: u64,
        usage: CompactionUsage,
        ts: u64,
    },
    WorkflowStart {
        workflow_id: String,
        name: String,
        yaml_path: String,
        args: Vec<String>,
        ts: u64,
    },
    NodeStart {
        workflow_id: String,
        node_id: String,
        node_type: String,
        attempt: u32,
        ts: u64,
    },
    NodeComplete {
        workflow_id: String,
        node_id: String,
        output: String,
        exit_code: Option<i32>,
        json_output: Option<serde_json::Value>,
        routed_to: Option<String>,
        ts: u64,
    },
    NodeSkip {
        workflow_id: String,
        node_id: String,
        reason: String,
        ts: u64,
    },
    NodeError {
        workflow_id: String,
        node_id: String,
        error: String,
        attempt: u32,
        ts: u64,
    },
    HookRun {
        workflow_id: String,
        node_id: Option<String>,
        hook_type: String,
        cmd: String,
        exit_code: i32,
        ts: u64,
    },
    WorkflowComplete {
        workflow_id: String,
        ts: u64,
    },
    WorkflowError {
        workflow_id: String,
        failed_node: String,
        error: String,
        ts: u64,
    },
    WorkflowGate {
        workflow_id: String,
        node_id: String,
        message: String,
        ts: u64,
    },
}

// ── JSONL helpers ─────────────────────────────────────────────────────────────

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub fn append_event(path: &Path, event: &AnyEvent) -> anyhow::Result<()> {
    crate::proto::wire::append(path, crate::proto::convert::any_to_body(event))?;
    Ok(())
}

/// Writes event to transcript file AND stdout as NDJSON.
/// Use for events in the current session. Cross-session writes use `append_event`.
pub fn emit_event(path: &Path, event: &AnyEvent) -> anyhow::Result<()> {
    crate::proto::wire::emit(path, crate::proto::convert::any_to_body(event))?;
    Ok(())
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
    emit_event(tx_path, &AnyEvent::PermissionsReply {
        from: from.to_string(),
        items,
        ts: now_ms(),
    })
}

pub fn init_session(opts: InitSessionOpts) -> anyhow::Result<()> {
    emit_event(&opts.transcript_path, &AnyEvent::SessionStart {
        schema_version: 2,
        id: opts.id,
        model: opts.model,
        codebase_hash: opts.codebase_hash,
        codebase_dirty: opts.codebase_dirty,
        ts: now_ms(),
    })
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

    #[test]
    fn append_and_read_roundtrip() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("transcript.jsonl");
        append_event(&path, &AnyEvent::UserTurn {
            content: "hello".into(),
            from: None,
            context: None,
            ts: 1000,
        }).unwrap();
        append_event(&path, &AnyEvent::AssistantText {
            content: "world".into(),
            ts: 2000,
        }).unwrap();
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

    fn mint_events(aes: &[AnyEvent]) -> Vec<Event> {
        aes.iter()
            .map(|ae| crate::proto::envelope::mint(crate::proto::convert::any_to_body(ae), None))
            .collect()
    }

    #[test]
    fn count_turn_complete_counts_correctly() {
        let events = mint_events(&[
            AnyEvent::UserTurn { content: "x".into(), from: None, context: None, ts: 0 },
            AnyEvent::TurnComplete {
                usage: TurnUsage { input: 1, output: 1, cache_read: 0, cache_write: 0 },
                codebase_hash: "a".into(),
                codebase_dirty: false,
                code_checkpoint: None,
                blocks: vec![],
                output: TurnOutput { message: "done".into(), result: None },
                ts: 1,
            },
            AnyEvent::UserTurn { content: "y".into(), from: None, context: None, ts: 2 },
            AnyEvent::TurnComplete {
                usage: TurnUsage { input: 1, output: 1, cache_read: 0, cache_write: 0 },
                codebase_hash: "a".into(),
                codebase_dirty: false,
                code_checkpoint: None,
                blocks: vec![],
                output: TurnOutput { message: "done".into(), result: None },
                ts: 3,
            },
        ]);
        assert_eq!(count_turn_complete(&events), 2);
    }

    #[test]
    fn last_compaction_index_finds_last() {
        let events = mint_events(&[
            AnyEvent::UserTurn { content: "x".into(), from: None, context: None, ts: 0 },
            AnyEvent::Compaction {
                summary: "s1".into(), turns_covered: 1,
                from_ts: 0, to_ts: 1,
                usage: CompactionUsage { input: 10, output: 5 }, ts: 1,
            },
            AnyEvent::UserTurn { content: "y".into(), from: None, context: None, ts: 2 },
        ]);
        assert_eq!(last_compaction_index(&events), Some(1));
    }

    #[test]
    fn user_turn_from_field_roundtrips() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("transcript.jsonl");
        append_event(&path, &AnyEvent::UserTurn {
            content: "do the thing".into(),
            from: Some("agent:s-parent123".into()),
            context: None,
            ts: 42,
        }).unwrap();
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
    fn user_turn_from_defaults_to_none_when_missing() {
        // Legacy AnyEvent JSON without `from` must still deserialize (writers
        // build AnyEvent before conversion).
        let json = r#"{"type":"user_turn","content":"hello","ts":1}"#;
        let event: AnyEvent = serde_json::from_str(json).unwrap();
        if let AnyEvent::UserTurn { from, .. } = event {
            assert!(from.is_none(), "expected None when field absent");
        } else {
            panic!("expected UserTurn");
        }
    }

    #[test]
    fn workflow_events_roundtrip() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("t.jsonl");
        append_event(&path, &AnyEvent::WorkflowStart {
            workflow_id: "wf1".into(), name: "test".into(),
            yaml_path: "workflows/test.yaml".into(), args: vec![], ts: 1,
        }).unwrap();
        append_event(&path, &AnyEvent::NodeStart {
            workflow_id: "wf1".into(), node_id: "n1".into(),
            node_type: "shell".into(), attempt: 1, ts: 2,
        }).unwrap();
        append_event(&path, &AnyEvent::NodeComplete {
            workflow_id: "wf1".into(), node_id: "n1".into(),
            output: "ok".into(), exit_code: Some(0),
            json_output: None, routed_to: None, ts: 3,
        }).unwrap();
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
    fn all_event_variants_serialize_roundtrip() {
        let events: Vec<AnyEvent> = vec![
            AnyEvent::SessionStart { schema_version: 2, id: "s1".into(), model: "m".into(), codebase_hash: "h".into(), codebase_dirty: false, ts: 0 },
            AnyEvent::UserTurn { content: "t".into(), from: None, context: None, ts: 0 },
            AnyEvent::AssistantText { content: "a".into(), ts: 0 },
            AnyEvent::ToolCall { id: "i".into(), name: "read".into(), input: serde_json::json!({}), ts: 0 },
            AnyEvent::ToolResult { id: "i".into(), output: "r".into(), ts: 0 },
            AnyEvent::Error { message: "e".into(), phase: "llm".into(), ts: 0 },
            AnyEvent::Permissions { items: vec![], ts: 0 },
            AnyEvent::PermissionsReply { from: "user".into(), items: vec![], ts: 0 },
            AnyEvent::TurnComplete { usage: TurnUsage { input:1,output:1,cache_read:0,cache_write:0 }, codebase_hash:"h".into(), codebase_dirty:false, code_checkpoint: None, blocks: vec![], output: TurnOutput{message:"s".into(),result:None}, ts:0 },
        ];
        // Writers still build AnyEvent; the on-disk form is now a proto Event line.
        for ae in &events {
            let ev = crate::proto::envelope::mint(crate::proto::convert::any_to_body(ae), None);
            let json = serde_json::to_string(&ev).unwrap();
            let back: Event = serde_json::from_str(&json).unwrap();
            assert!(back.body.is_some(), "missing body in: {json}");
            assert!(crate::proto::envelope::verify_hash(&back), "hash must verify: {json}");
        }
    }

    #[test]
    fn emit_event_writes_to_transcript() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("transcript.jsonl");
        emit_event(&path, &AnyEvent::AssistantText {
            content: "hello emit".into(),
            ts: 1,
        }).unwrap();
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

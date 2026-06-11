//! Ergonomic constructors that build proto [`event::Body`] values directly.
//!
//! These replace the legacy `AnyEvent` → `any_to_body` converter: each write
//! call site now mints a proto body straight from native args. The mapping
//! helpers (`to_struct`, enum-string → enum, item mappers) live here as private
//! fns — this is the single place that encodes the schema restructuring the
//! proto format introduces (id → call_id, `Value` → `Struct`, enum strings →
//! proto enums, context → `CodebaseState` + injected files, etc.).

use crate::proto::gait::log::v1 as pb;
use crate::proto::wire;
use crate::session::transcript as tx;

/// `SessionStart` — identity only (model / codebase history dropped per design).
pub fn session_start(session_id: String, assigned_by: Option<String>) -> pb::event::Body {
    wire::body(pb::payload::Kind::SessionStart(pb::SessionStart {
        session_id,
        assigned_by,
    }))
}

/// `UserTurn` — maps context → `CodebaseState` + `injected_files`.
pub fn user_turn(
    content: String,
    from: Option<String>,
    context: Option<&tx::UserTurnContext>,
) -> pb::event::Body {
    let (codebase, injected_files) = match context {
        Some(ctx) => (
            Some(pb::CodebaseState {
                hash: ctx.codebase_hash.clone().unwrap_or_default(),
                dirty: ctx.codebase_dirty.unwrap_or(false),
                dirty_files: ctx.dirty_files.clone().unwrap_or_default(),
            }),
            ctx.injected_files.clone().unwrap_or_default(),
        ),
        None => (None, Vec::new()),
    };
    wire::body(pb::payload::Kind::UserTurn(pb::UserTurn {
        content,
        from,
        codebase,
        injected_files,
    }))
}

/// `AssistantText`.
pub fn assistant_text(content: String) -> pb::event::Body {
    wire::body(pb::payload::Kind::AssistantText(pb::AssistantText { content }))
}

/// `ToolCall` — id → call_id, `Value` input → `Struct`.
pub fn tool_call(call_id: String, name: String, input: &serde_json::Value) -> pb::event::Body {
    wire::body(pb::payload::Kind::ToolCall(pb::ToolCall {
        call_id,
        name,
        input: to_struct(input),
    }))
}

/// `ToolResult` — generic `duration` (every tool) + `ProcessResult` (shell/process tools).
/// Both are optional; the synthetic sites (denied/error/approved-fallback/recorded) pass `None, None`.
pub fn tool_result(
    call_id: String,
    output: String,
    duration: Option<pbjson_types::Duration>,
    process: Option<pb::ProcessResult>,
) -> pb::event::Body {
    wire::body(pb::payload::Kind::ToolResult(pb::ToolResult {
        call_id,
        output,
        duration,
        process,
    }))
}

/// `Error` — phase string → proto enum.
pub fn error(message: String, phase: &str) -> pb::event::Body {
    wire::body(pb::payload::Kind::Error(pb::Error {
        message,
        phase: error_phase(phase),
    }))
}

/// `Clarify`.
pub fn clarify(items: &[tx::ClarifyItem]) -> pb::event::Body {
    wire::body(pb::payload::Kind::Clarify(pb::Clarify {
        items: items
            .iter()
            .map(|c| pb::ClarifyItem {
                id: c.id.clone(),
                content: c.content.clone(),
                why: c.why.clone(),
                kind: clarify_kind(&c.kind),
            })
            .collect(),
    }))
}

/// `ClarifyReply`.
pub fn clarify_reply(items: &[tx::ClarifyReplyItem]) -> pb::event::Body {
    wire::body(pb::payload::Kind::ClarifyReply(pb::ClarifyReply {
        items: items
            .iter()
            .map(|r| pb::ClarifyReplyItem {
                id: r.id.clone(),
                content: r.content.clone(),
                reasoning: r.reasoning.clone(),
            })
            .collect(),
    }))
}

/// `ClarifyConvergence`.
pub fn clarify_convergence(items: &[tx::ClarifyConvergenceItem]) -> pb::event::Body {
    wire::body(pb::payload::Kind::ClarifyConvergence(pb::ClarifyConvergence {
        items: items
            .iter()
            .map(|c| pb::ConvergenceItem {
                id: c.id.clone(),
                proposal: c.proposal.clone(),
                response: c.response.clone(),
                consistent: c.consistent,
            })
            .collect(),
    }))
}

/// `Inform`.
pub fn inform(items: &[tx::InformItem]) -> pb::event::Body {
    wire::body(pb::payload::Kind::Inform(pb::Inform {
        items: items
            .iter()
            .map(|it| pb::InformItem {
                id: it.id.clone(),
                content: it.content.clone(),
                context: it.context.clone(),
            })
            .collect(),
    }))
}

/// `InformReply`.
pub fn inform_reply(items: &[tx::InformReplyItem]) -> pb::event::Body {
    wire::body(pb::payload::Kind::InformReply(pb::InformReply {
        items: items
            .iter()
            .map(|r| pb::InformReplyItem {
                id: r.id.clone(),
                content: r.content.clone(),
            })
            .collect(),
    }))
}

/// `InformConvergence`.
pub fn inform_convergence(items: &[tx::InformConvergenceItem]) -> pb::event::Body {
    wire::body(pb::payload::Kind::InformConvergence(pb::InformConvergence {
        items: items
            .iter()
            .map(|c| pb::ConvergenceItem {
                id: c.id.clone(),
                proposal: c.proposal.clone(),
                response: c.response.clone(),
                consistent: c.consistent,
            })
            .collect(),
    }))
}

/// `AgentUpdate` — status string → proto enum, items → `Value`.
pub fn agent_update(
    session_id: String,
    status: &str,
    summary: String,
    items: Option<serde_json::Value>,
) -> pb::event::Body {
    wire::body(pb::payload::Kind::AgentUpdate(pb::AgentUpdate {
        session_id,
        status: agent_status(status),
        summary,
        items: to_value_opt(&items),
    }))
}

/// `Permissions`.
pub fn permissions(items: &[tx::PermissionItem]) -> pb::event::Body {
    wire::body(pb::payload::Kind::Permissions(pb::Permissions {
        items: items.iter().map(permission_item).collect(),
    }))
}

/// `PermissionsReply` — decision / scope strings → proto enums.
pub fn permissions_reply(from: String, items: &[tx::PermissionsReplyItem]) -> pb::event::Body {
    wire::body(pb::payload::Kind::PermissionsReply(pb::PermissionsReply {
        from,
        items: items
            .iter()
            .map(|r| pb::PermissionsReplyItem {
                id: r.id.clone(),
                decision: decision(&r.decision),
                text: r.text.clone(),
                scope: scope(&r.scope),
            })
            .collect(),
    }))
}

/// `TurnComplete`.
#[allow(clippy::too_many_arguments)]
pub fn turn_complete(
    usage: &tx::TurnUsage,
    codebase_hash: String,
    codebase_dirty: bool,
    checkpoint: Option<String>,
    blocks: Vec<String>,
    output: &tx::TurnOutput,
) -> pb::event::Body {
    wire::body(pb::payload::Kind::TurnComplete(pb::TurnComplete {
        usage: Some(pb::TurnUsage {
            input: usage.input,
            output: usage.output,
            cache_read: usage.cache_read,
            cache_write: usage.cache_write,
        }),
        codebase: Some(pb::CodebaseState {
            hash: codebase_hash,
            dirty: codebase_dirty,
            dirty_files: Vec::new(),
        }),
        checkpoint,
        blocks,
        output: Some(pb::TurnOutput {
            message: output.message.clone(),
            result: to_value_opt(&output.result),
        }),
    }))
}

/// Build the proto `Resolution` message from the live resolved profile.
/// Mapped field-by-field from the in-memory `ResolvedProfile`. The `mcp_servers`
/// list is sorted because the source `mcp_clients` is a `HashMap` whose
/// iteration order is nondeterministic; sorting makes the emitted event stable
/// so the drift check in `run_turn` doesn't thrash on otherwise-identical
/// resolutions.
pub fn resolution_msg(resolved: &crate::profile::ResolvedProfile) -> pb::Resolution {
    let mut mcp_servers: Vec<String> = resolved.mcp_clients.keys().cloned().collect();
    mcp_servers.sort();
    pb::Resolution {
        model: resolved.model.clone(),
        tools: resolved.tools.iter().map(|t| t.name().to_string()).collect(),
        mcp_servers,
        skills: resolved.skill_registry.iter().map(|s| s.name.clone()).collect(),
        permissions: Some(pb::PermissionPolicy {
            allow: resolved.allow.clone(),
            deny: resolved.deny.clone(),
        }),
        instructions: resolved.instructions.clone(),
        output_schema_retries: Some(resolved.output_schema_retries as u32),
    }
}

/// The `Resolution` event body.
pub fn resolution(resolved: &crate::profile::ResolvedProfile) -> pb::event::Body {
    wire::body(pb::payload::Kind::Resolution(resolution_msg(resolved)))
}

/// The most recent `Resolution` payload in `events`, if any. Used by `run_turn`
/// as the drift-guard baseline: skip emitting a new `Resolution` if the live
/// profile already matches the most recent one.
pub fn last_resolution(events: &[crate::proto::gait::log::v1::Event]) -> Option<pb::Resolution> {
    for e in events.iter().rev() {
        if let Some(crate::proto::gait::log::v1::event::Body::Payload(p)) = &e.body {
            if let Some(pb::payload::Kind::Resolution(r)) = &p.kind {
                return Some(r.clone());
            }
        }
    }
    None
}

/// `Compaction` — through_seq unset (no seq in legacy data).
pub fn compaction(
    summary: String,
    turns_covered: u32,
    usage: &tx::CompactionUsage,
) -> pb::event::Body {
    wire::body(pb::payload::Kind::Compaction(pb::Compaction {
        summary,
        turns_covered,
        usage: Some(pb::CompactionUsage {
            input: usage.input,
            output: usage.output,
        }),
        through_seq: None,
    }))
}

// ── helpers ──────────────────────────────────────────────────────────────────

fn permission_item(p: &tx::PermissionItem) -> pb::PermissionItem {
    pb::PermissionItem {
        id: p.id.clone(),
        cmd: p.cmd.clone(),
        reason: p.reason.clone(),
        patterns: p.patterns.clone(),
        tool_name: p.tool_name.clone(),
        tool_args: to_struct(&p.tool_args),
    }
}

/// A JSON object → proto `Struct`. Non-objects (e.g. `Value::Null`) → `None`.
fn to_struct(v: &serde_json::Value) -> Option<pbjson_types::Struct> {
    serde_json::from_value(v.clone()).ok()
}

/// An optional JSON value → proto `Value` (any JSON shape).
fn to_value_opt(v: &Option<serde_json::Value>) -> Option<pbjson_types::Value> {
    v.as_ref().and_then(|x| serde_json::from_value(x.clone()).ok())
}

fn error_phase(s: &str) -> i32 {
    let e = match s {
        "llm" => pb::ErrorPhase::Llm,
        "tool" => pb::ErrorPhase::Tool,
        "git" => pb::ErrorPhase::Git,
        "init" => pb::ErrorPhase::Init,
        _ => pb::ErrorPhase::Unspecified,
    };
    e as i32
}

fn clarify_kind(s: &str) -> i32 {
    let k = match s {
        "is" => pb::ClarifyKind::Is,
        "ought" => pb::ClarifyKind::Ought,
        _ => pb::ClarifyKind::Unspecified,
    };
    k as i32
}

fn agent_status(s: &str) -> i32 {
    let a = match s {
        "done" => pb::AgentStatus::Done,
        "clarify" => pb::AgentStatus::Clarify,
        "inform" => pb::AgentStatus::Inform,
        _ => pb::AgentStatus::Unspecified,
    };
    a as i32
}

fn decision(s: &str) -> i32 {
    let d = match s {
        "approve" => pb::Decision::Approve,
        "deny" => pb::Decision::Deny,
        _ => pb::Decision::Unspecified,
    };
    d as i32
}

fn scope(s: &Option<String>) -> i32 {
    let sc = match s.as_deref() {
        Some("once") => pb::Scope::Once,
        Some("agent") => pb::Scope::Agent,
        Some("global") => pb::Scope::Global,
        _ => pb::Scope::Unspecified,
    };
    sc as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kind_of(body: &pb::event::Body) -> &pb::payload::Kind {
        match body {
            pb::event::Body::Payload(p) => p.kind.as_ref().expect("kind set"),
            other => panic!("expected Payload body, got {other:?}"),
        }
    }

    #[test]
    fn tool_call_sets_call_id_and_struct_input() {
        let body = tool_call("call-1".into(), "read".into(), &serde_json::json!({"file_path": "a.rs"}));
        match kind_of(&body) {
            pb::payload::Kind::ToolCall(tc) => {
                assert_eq!(tc.call_id, "call-1");
                assert_eq!(tc.name, "read");
                let s = tc.input.as_ref().expect("input struct");
                assert!(s.fields.contains_key("file_path"));
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
    }

    #[test]
    fn error_git_maps_to_enum() {
        let body = error("boom".into(), "git");
        match kind_of(&body) {
            pb::payload::Kind::Error(e) => assert_eq!(e.phase, pb::ErrorPhase::Git as i32),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn user_turn_context_becomes_codebase_and_injected() {
        let ctx = tx::UserTurnContext {
            codebase_hash: Some("abc".into()),
            codebase_dirty: Some(true),
            dirty_files: Some(vec!["x.rs".into()]),
            injected_files: Some(vec!["README.md".into()]),
        };
        let body = user_turn("hi".into(), Some("user".into()), Some(&ctx));
        match kind_of(&body) {
            pb::payload::Kind::UserTurn(u) => {
                let cb = u.codebase.as_ref().expect("codebase");
                assert_eq!(cb.hash, "abc");
                assert!(cb.dirty);
                assert_eq!(u.injected_files, vec!["README.md".to_string()]);
            }
            other => panic!("expected UserTurn, got {other:?}"),
        }
    }

    #[test]
    fn resolution_maps_fields_from_resolved_profile() {
        use crate::profile::ResolvedProfile;
        use crate::tools::OutputTool;
        use std::sync::Arc;

        // Build a ResolvedProfile with everything populated, via for_test
        // (which gives a stub provider + empty allow/deny/skills) and then
        // override the fields the mapping reads. We pass a real Tool (Output)
        // so the Vec<Box<dyn Tool>> is constructable in for_test.
        let mut profile = ResolvedProfile::for_test(
            Arc::new(DummyProvider) as Arc<dyn crate::provider::Provider>,
            vec![Box::new(OutputTool::new())],
            vec!["read".into()],
            vec!["shell:rm *".into()],
        );
        profile.model = "anthropic/x".into();
        profile.instructions = "be terse".into();
        profile.output_schema_retries = 7;

        let body = resolution(&profile);
        let r = match kind_of(&body) {
            pb::payload::Kind::Resolution(r) => r.clone(),
            other => panic!("expected Resolution, got {other:?}"),
        };
        assert_eq!(r.model, "anthropic/x");
        let perms = r.permissions.as_ref().expect("permissions");
        assert_eq!(perms.allow, vec!["read".to_string()]);
        assert_eq!(perms.deny, vec!["shell:rm *".to_string()]);
        assert_eq!(r.output_schema_retries, Some(7));
        // mcp_servers is empty in for_test (no MCP clients); trivially sorted.
        assert!(r.mcp_servers.is_empty());
        // skills is empty in for_test default.
        assert!(r.skills.is_empty());
    }

    #[test]
    fn resolution_equality_is_mcp_server_order_sensitive() {
        // The drift-guard relies on equality of `pb::Resolution` being
        // order-sensitive for mcp_servers, so a `HashMap` iteration-order
        // change does not falsely appear as a content change after we sort
        // the vec on emit.
        let r1 = pb::Resolution {
            model: "m".into(),
            tools: vec![],
            mcp_servers: vec!["zeta".into(), "alpha".into()],
            skills: vec![],
            permissions: Some(pb::PermissionPolicy { allow: vec![], deny: vec![] }),
            instructions: String::new(),
            output_schema_retries: None,
        };
        let r2 = pb::Resolution {
            mcp_servers: vec!["alpha".into(), "zeta".into()],
            ..r1.clone()
        };
        assert_ne!(r1, r2, "resolution equality is sensitive to mcp_servers order");
    }

    #[test]
    fn last_resolution_returns_most_recent_resolution() {
        use crate::proto::wire;
        let dir = tempfile::TempDir::new().expect("tempdir");
        let path = dir.path().join("tx.jsonl");
        wire::append(&path, resolution_body("model-a")).unwrap();
        wire::append(&path, resolution_body("model-b")).unwrap();
        let events = wire::read_events(&path).unwrap();
        let r = last_resolution(&events).expect("a resolution");
        assert_eq!(r.model, "model-b");
    }

    fn resolution_body(model: &str) -> pb::event::Body {
        wire::body(pb::payload::Kind::Resolution(pb::Resolution {
            model: model.into(),
            tools: vec![],
            mcp_servers: vec![],
            skills: vec![],
            permissions: Some(pb::PermissionPolicy { allow: vec![], deny: vec![] }),
            instructions: String::new(),
            output_schema_retries: None,
        }))
    }

    // ── Dummy provider for the mapping test (we never call into it) ──────────
    use async_trait::async_trait;
    use crate::provider::{Provider, ProviderError, StreamChunk};
    use crate::types::{Message, Tool};

    pub struct DummyProvider;
    #[async_trait]
    impl Provider for DummyProvider {
        fn model_id(&self) -> &str { "dummy" }
        async fn stream(
            &self,
            _system: &str,
            _messages: &[Message],
            _tools: &[Box<dyn Tool>],
        ) -> Result<std::pin::Pin<Box<dyn futures_util::Stream<Item = Result<StreamChunk, ProviderError>> + Send>>, ProviderError> {
            Err(ProviderError::Other("dummy".into()))
        }
    }
}

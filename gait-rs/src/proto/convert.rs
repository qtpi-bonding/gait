//! Migration converter: legacy [`AnyEvent`] → proto [`event::Body`].
//!
//! This is the **single place** that encodes the schema restructuring the proto
//! format introduces over the old hand-written `AnyEvent`:
//!   * `SessionStart` sheds `model` / `codebase_*` → identity only (model history
//!     becomes the `Resolution` event; codebase moves onto turns).
//!   * `ToolResult` gains `process` / `duration` (unset here; populated at the
//!     shell call site in a later unit).
//!   * `Compaction`'s `from_ts` / `to_ts` → `through_seq` (no `seq` in legacy data → None).
//!   * enum strings (`phase`, `kind`, `status`, `decision`, `scope`) → proto enums.
//!   * `Value` tool I/O → `Struct` / `Value` well-known types.
//!   * workflow / `Node*` / `HookRun` (deferred) + dead `CommandResult` → opaque `RawEvent`.
//!
//! Scaffolding: it exists so writers can keep constructing `AnyEvent` while the
//! read side moves to proto (kept green). It is deleted once the write call sites
//! build `Payload` directly. Dropped fields (`links` / `to` on clarify/inform
//! items; `schema_version`) are dead per the design review.
//!
//! [`AnyEvent`]: crate::session::transcript::AnyEvent
//! [`event::Body`]: crate::proto::gait::log::v1::event::Body

use crate::proto::gait::log::v1 as pb;
use crate::proto::wire;
use crate::session::transcript::AnyEvent;

/// Convert a legacy event into the proto event body it maps to.
pub fn any_to_body(event: &AnyEvent) -> pb::event::Body {
    match event {
        AnyEvent::SessionStart { id, .. } => {
            wire::body(pb::payload::Kind::SessionStart(pb::SessionStart {
                session_id: id.clone(),
                assigned_by: None,
            }))
        }

        AnyEvent::UserTurn { content, from, context, .. } => {
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
                content: content.clone(),
                from: from.clone(),
                codebase,
                injected_files,
            }))
        }

        AnyEvent::AssistantText { content, .. } => {
            wire::body(pb::payload::Kind::AssistantText(pb::AssistantText {
                content: content.clone(),
            }))
        }

        AnyEvent::ToolCall { id, name, input, .. } => {
            wire::body(pb::payload::Kind::ToolCall(pb::ToolCall {
                call_id: id.clone(),
                name: name.clone(),
                input: to_struct(input),
            }))
        }

        AnyEvent::ToolResult { id, output, .. } => {
            wire::body(pb::payload::Kind::ToolResult(pb::ToolResult {
                call_id: id.clone(),
                output: output.clone(),
                duration: None,
                process: None,
            }))
        }

        AnyEvent::Error { message, phase, .. } => {
            wire::body(pb::payload::Kind::Error(pb::Error {
                message: message.clone(),
                phase: error_phase(phase),
            }))
        }

        AnyEvent::Clarify { items, .. } => {
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

        AnyEvent::ClarifyReply { items, .. } => {
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

        AnyEvent::ClarifyConvergence { items, .. } => {
            wire::body(pb::payload::Kind::ClarifyConvergence(pb::ClarifyConvergence {
                items: items.iter().map(convergence_item).collect(),
            }))
        }

        AnyEvent::Inform { items, .. } => {
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

        AnyEvent::InformReply { items, .. } => {
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

        AnyEvent::InformConvergence { items, .. } => {
            wire::body(pb::payload::Kind::InformConvergence(pb::InformConvergence {
                items: items.iter().map(convergence_item_inform).collect(),
            }))
        }

        AnyEvent::AgentUpdate { session_id, status, summary, items, .. } => {
            wire::body(pb::payload::Kind::AgentUpdate(pb::AgentUpdate {
                session_id: session_id.clone(),
                status: agent_status(status),
                summary: summary.clone(),
                items: to_value_opt(items),
            }))
        }

        AnyEvent::Permissions { items, .. } => {
            wire::body(pb::payload::Kind::Permissions(pb::Permissions {
                items: items.iter().map(permission_item).collect(),
            }))
        }

        AnyEvent::PermissionsReply { from, items, .. } => {
            wire::body(pb::payload::Kind::PermissionsReply(pb::PermissionsReply {
                from: from.clone(),
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

        AnyEvent::TurnComplete {
            usage, codebase_hash, codebase_dirty, code_checkpoint, blocks, output, ..
        } => {
            wire::body(pb::payload::Kind::TurnComplete(pb::TurnComplete {
                usage: Some(pb::TurnUsage {
                    input: usage.input,
                    output: usage.output,
                    cache_read: usage.cache_read,
                    cache_write: usage.cache_write,
                }),
                codebase: Some(pb::CodebaseState {
                    hash: codebase_hash.clone(),
                    dirty: *codebase_dirty,
                    dirty_files: Vec::new(),
                }),
                checkpoint: code_checkpoint.clone(),
                blocks: blocks.clone(),
                output: Some(pb::TurnOutput {
                    message: output.message.clone(),
                    result: to_value_opt(&output.result),
                }),
            }))
        }

        AnyEvent::Compaction { summary, turns_covered, usage, .. } => {
            wire::body(pb::payload::Kind::Compaction(pb::Compaction {
                summary: summary.clone(),
                turns_covered: *turns_covered,
                usage: Some(pb::CompactionUsage {
                    input: usage.input,
                    output: usage.output,
                }),
                through_seq: None,
            }))
        }

        // Deferred (workflow family) + dead (`CommandResult`) → opaque RawEvent,
        // storing the original tagged JSON verbatim so the workflow subsystem can
        // still read it back until its redesign.
        other => {
            let v = serde_json::to_value(other).unwrap_or_else(|_| serde_json::json!({}));
            let tag = v.get("type").and_then(|t| t.as_str()).unwrap_or("unknown").to_string();
            wire::raw(&tag, v)
        }
    }
}

// ── helpers ──────────────────────────────────────────────────────────────────

fn convergence_item(c: &crate::session::transcript::ClarifyConvergenceItem) -> pb::ConvergenceItem {
    pb::ConvergenceItem {
        id: c.id.clone(),
        proposal: c.proposal.clone(),
        response: c.response.clone(),
        consistent: c.consistent,
    }
}

fn convergence_item_inform(c: &crate::session::transcript::InformConvergenceItem) -> pb::ConvergenceItem {
    pb::ConvergenceItem {
        id: c.id.clone(),
        proposal: c.proposal.clone(),
        response: c.response.clone(),
        consistent: c.consistent,
    }
}

fn permission_item(p: &crate::session::transcript::PermissionItem) -> pb::PermissionItem {
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
    use crate::session::transcript::{ClarifyItem, UserTurnContext};

    fn kind_of(body: &pb::event::Body) -> &pb::payload::Kind {
        match body {
            pb::event::Body::Payload(p) => p.kind.as_ref().expect("kind set"),
            other => panic!("expected Payload body, got {other:?}"),
        }
    }

    #[test]
    fn session_start_sheds_to_identity() {
        let body = any_to_body(&AnyEvent::SessionStart {
            schema_version: 2,
            id: "s1".into(),
            model: "anthropic/claude".into(),
            codebase_hash: "h".into(),
            codebase_dirty: true,
            ts: 5,
        });
        match kind_of(&body) {
            pb::payload::Kind::SessionStart(s) => {
                assert_eq!(s.session_id, "s1");
                assert_eq!(s.assigned_by, None); // model + codebase dropped
            }
            other => panic!("expected SessionStart, got {other:?}"),
        }
    }

    #[test]
    fn tool_call_remaps_id_and_input() {
        let body = any_to_body(&AnyEvent::ToolCall {
            id: "call-1".into(),
            name: "read".into(),
            input: serde_json::json!({"file_path": "a.rs"}),
            ts: 0,
        });
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
    fn error_phase_string_maps_to_enum() {
        let body = any_to_body(&AnyEvent::Error {
            message: "boom".into(),
            phase: "git".into(),
            ts: 0,
        });
        match kind_of(&body) {
            pb::payload::Kind::Error(e) => {
                assert_eq!(e.phase, pb::ErrorPhase::Git as i32);
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn clarify_drops_links_keeps_kind() {
        let body = any_to_body(&AnyEvent::Clarify {
            items: vec![ClarifyItem {
                id: "q1".into(),
                content: "which?".into(),
                why: "matters".into(),
                kind: "ought".into(),
                links: Some(vec![]),
                to: Some("someone".into()),
            }],
            ts: 0,
        });
        match kind_of(&body) {
            pb::payload::Kind::Clarify(c) => {
                assert_eq!(c.items.len(), 1);
                assert_eq!(c.items[0].id, "q1");
                assert_eq!(c.items[0].kind, pb::ClarifyKind::Ought as i32);
            }
            other => panic!("expected Clarify, got {other:?}"),
        }
    }

    #[test]
    fn user_turn_context_becomes_codebase_and_injected() {
        let body = any_to_body(&AnyEvent::UserTurn {
            content: "hi".into(),
            from: Some("user".into()),
            context: Some(UserTurnContext {
                codebase_hash: Some("abc".into()),
                codebase_dirty: Some(true),
                dirty_files: Some(vec!["x.rs".into()]),
                injected_files: Some(vec!["README.md".into()]),
            }),
            ts: 0,
        });
        match kind_of(&body) {
            pb::payload::Kind::UserTurn(u) => {
                let cb = u.codebase.as_ref().expect("codebase");
                assert_eq!(cb.hash, "abc");
                assert!(cb.dirty);
                assert_eq!(cb.dirty_files, vec!["x.rs".to_string()]);
                assert_eq!(u.injected_files, vec!["README.md".to_string()]);
            }
            other => panic!("expected UserTurn, got {other:?}"),
        }
    }

    #[test]
    fn workflow_event_becomes_raw_verbatim() {
        let body = any_to_body(&AnyEvent::NodeStart {
            workflow_id: "wf1".into(),
            node_id: "n1".into(),
            node_type: "shell".into(),
            attempt: 1,
            ts: 0,
        });
        match kind_of(&body) {
            pb::payload::Kind::Raw(r) => {
                assert_eq!(r.r#type, "node_start");
                let s = r.payload.as_ref().expect("struct");
                assert!(s.fields.contains_key("workflow_id"));
                assert!(s.fields.contains_key("node_id"));
            }
            other => panic!("expected Raw, got {other:?}"),
        }
    }

    #[test]
    fn permission_null_tool_args_is_none() {
        let body = any_to_body(&AnyEvent::Permissions {
            items: vec![crate::session::transcript::PermissionItem {
                id: "p1".into(),
                cmd: "shell".into(),
                reason: "r".into(),
                patterns: vec![],
                tool_name: "shell".into(),
                tool_args: serde_json::Value::Null,
            }],
            ts: 0,
        });
        match kind_of(&body) {
            pb::payload::Kind::Permissions(p) => {
                assert!(p.items[0].tool_args.is_none(), "Null tool_args → None");
            }
            other => panic!("expected Permissions, got {other:?}"),
        }
    }
}

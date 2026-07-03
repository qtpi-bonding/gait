//! Permission-classification layer for the streaming turn loop. Pure: no I/O,
//! no side effects. Given one `ToolCall` plus the current effective allow
//! list and the deny list, decide whether the call should be allowed, denied,
//! or surfaced to the user for a decision. Preview streaming and the
//! emit-blocked-results loop stay in `mod.rs` — only the classification
//! step itself is here.

use crate::permissions::{find_matched_allow_pattern, is_request_approved, is_request_denied};
use crate::proto::gait::log::v1 as pb;
use crate::types::{Tool, ToolCall, ToolPermission};
/// The classification of a single tool call against the current permission
/// policy. Each variant tells the caller what to do next — the `Deny` variant
/// is a unit (no fields) because the actual deny pattern is recomputed at
/// the emit site where `find_matched_deny_pattern` is already called.
pub(super) enum GateDecision {
    /// Tool is allowed. `approval_prefix` is the text to prepend to the tool
    /// result (e.g. `"[Auto mode.]"` or `"[Auto-approved: shell:*]"`). `None`
    /// means no prefix — the tool runs as-is.
    Allow { approval_prefix: Option<String> },
    /// Tool requires user approval. `items` lists the `PermissionItem`s the
    /// Permissions UI should show for this call.
    Gate { items: Vec<pb::PermissionItem> },
    /// Tool is blocked by the deny list. Hard floor, applies even in auto mode;
    /// the caller emits a `[Blocked by config: ...] permission denied`
    /// `ToolResult` so the model sees why.
    Deny,
}

/// Classify one tool call against the current permission policy.
///
/// Pure: no I/O, no side effects. Preview streaming happens at the call site
/// (the `Allow` arm in `mod.rs`).
///
/// `tool` is `Option<&Box<dyn Tool>>` because a call may name a tool the loop
/// doesn't have in scope (unknown tool → treated as a "requires approval"
/// call with the tool name as its own pattern, same as the original
/// `unwrap_or_else(...)` fallback).
pub(super) fn classify(
    tc: &ToolCall,
    tool: Option<&dyn Tool>,
    effective_allow: &[String],
    deny: &[String],
    auto: bool,
) -> GateDecision {
    let perm = tool.map(|t| t.permission(&tc.args)).unwrap_or_else(|| {
        ToolPermission::RequiresApproval(vec![crate::types::PermissionRequest {
            display: tc.tool_name.clone(),
            patterns: vec![tc.tool_name.clone()],
        }])
    });

    // Tier 1: deny list — hard floor, checked before allow, applies even in auto mode.
    let is_denied = match &perm {
        ToolPermission::Passthrough => false,
        ToolPermission::RequiresApproval(requests) => requests
            .iter()
            .any(|req| is_request_denied(&tc.tool_name, req, deny)),
    };
    if is_denied {
        return GateDecision::Deny;
    }

    // Tier 2: allow list (auto-mode skips the check).
    let is_allowed = auto
        || match &perm {
            ToolPermission::Passthrough => true,
            ToolPermission::RequiresApproval(requests) => requests
                .iter()
                .all(|req| is_request_approved(&tc.tool_name, req, effective_allow)),
        };
    if is_allowed {
        // Pick the prefix to stamp on the tool result. In auto mode we
        // always stamp "[Auto mode.]"; in manual mode we only stamp when
        // the tool was auto-approved via a specific allow pattern, so the
        // model can see which pattern let it through.
        let approval_prefix: Option<String> = if auto {
            Some("[Auto mode.]".to_string())
        } else {
            match &perm {
                ToolPermission::Passthrough => None,
                ToolPermission::RequiresApproval(requests) => requests
                    .iter()
                    .find_map(|req| find_matched_allow_pattern(&tc.tool_name, req, effective_allow))
                    .map(|p| format!("[Auto-approved: {}]", p)),
            }
        };
        return GateDecision::Allow { approval_prefix };
    }

    // Tier 3: ask — emit a PermissionItem for each request segment that
    // wasn't already approved.
    let mut items: Vec<pb::PermissionItem> = vec![];
    if let ToolPermission::RequiresApproval(requests) = &perm {
        for req in requests
            .iter()
            .filter(|r| !is_request_approved(&tc.tool_name, r, effective_allow))
        {
            // The preview-policy check stays in mod.rs (the classification
            // layer is pure; only the loop knows about PreviewPolicy). Set
            // `preview: None` here and let the loop's `Allow` arm stream
            // the preview before this call is dispatched. For `Gate`
            // items the UI surfaces the preview directly.
            items.push(pb::PermissionItem {
                id: tc.tool_call_id.clone(),
                cmd: req.display.clone(),
                reason: format!("{} requires permission", tc.tool_name),
                patterns: req.patterns.clone(),
                tool_name: tc.tool_name.clone(),
                tool_args: crate::proto::events::to_struct(&tc.args),
                preview: None, // spine fills this from the codebase (sealed brain reads nothing)
            });
        }
    }
    GateDecision::Gate { items }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{PermissionRequest, ToolPermission};

    /// A minimal Tool stub whose `preview()` always returns a non-None value.
    /// Used to prove that `classify` does NOT call `preview()` — gate-layer
    /// items must always carry `preview: None` (the spine fills it).
    struct PreviewTool;

    #[async_trait::async_trait]
    impl crate::types::Tool for PreviewTool {
        fn name(&self) -> &str {
            "edit"
        }
        fn description(&self) -> &str {
            "test edit tool"
        }
        fn parameters(&self) -> &serde_json::Value {
            &serde_json::Value::Null
        }
        async fn execute(
            &self,
            _args: serde_json::Value,
            _rec: &crate::Recorder,
        ) -> Result<serde_json::Value, crate::tools::ToolError> {
            Ok(serde_json::Value::Null)
        }
        fn permission(&self, _args: &serde_json::Value) -> ToolPermission {
            ToolPermission::RequiresApproval(vec![PermissionRequest {
                display: "edit /foo".into(),
                patterns: vec!["edit:*".into()],
            }])
        }
        fn preview(&self, _args: &serde_json::Value) -> Option<String> {
            Some("--- a/foo\n+++ b/foo\n@@ -1 +1 @@\n-old\n+new".into())
        }
    }

    #[test]
    fn classify_gate_item_preview_is_none() {
        // Brain must never compute previews; the spine fills them after relay.
        let tc = crate::types::ToolCall {
            tool_call_id: "tc1".into(),
            tool_name: "edit".into(),
            args: serde_json::json!({"path": "/foo", "content": "new"}),
        };
        let tool: &dyn crate::types::Tool = &PreviewTool;
        let result = classify(&tc, Some(tool), &[], &[], false);
        match result {
            GateDecision::Gate { items } => {
                assert_eq!(items.len(), 1);
                assert!(
                    items[0].preview.is_none(),
                    "gate must emit preview: None — spine fills it; got {:?}",
                    items[0].preview
                );
            }
            _other => panic!("expected GateDecision::Gate, got something else"),
        }
    }
}

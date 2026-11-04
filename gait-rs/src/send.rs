//! The `send_message` reply tool. The model proposes targets and a body; the
//! spine signs with the agent's device session and runs the configured send
//! command (`message-attestation` does the parsing, signing and exec). The
//! model never sees keys, the audience, the message id or a timestamp.
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

use identity_crypto::PrincipalId;
use serde_json::Value;

use crate::types::{PermissionRequest, Tool, ToolPermission};

/// Process-level send configuration (spine side): the command argv and the
/// deliverer principal every send is addressed to.
#[derive(Clone, Debug)]
pub struct SendConfig {
    pub argv: Vec<String>,
    pub audience: PrincipalId,
}

static CONFIG: OnceLock<SendConfig> = OnceLock::new();
static OFFERED: AtomicBool = AtomicBool::new(false);

/// Installs the send configuration (spine side). The first call wins.
pub fn install_send_config(config: SendConfig) {
    let _ = CONFIG.set(config);
}

pub(crate) fn config() -> Option<&'static SendConfig> {
    CONFIG.get()
}

/// Marks the tool as offered in a process that has no `SendConfig` (the sealed
/// brain, told by the spine's argv).
pub fn install_offered() {
    OFFERED.store(true, Ordering::SeqCst);
}

/// Whether the model is offered `send_message` in this process.
pub fn offered() -> bool {
    OFFERED.load(Ordering::SeqCst) || CONFIG.get().is_some()
}

/// Brain-side half: the library's definition. Execution happens in the spine
/// (`handlers::send_message`), like `spawn_agent`.
pub struct SendMessageTool {
    parameters: Value,
}

impl SendMessageTool {
    pub fn new() -> Self {
        Self { parameters: message_attestation::send_message_tool().input_schema }
    }
}

impl Default for SendMessageTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Tool for SendMessageTool {
    fn name(&self) -> &str {
        message_attestation::SEND_MESSAGE_TOOL
    }
    fn description(&self) -> &str {
        message_attestation::send_message_tool().description
    }
    fn parameters(&self) -> &Value {
        &self.parameters
    }
    async fn execute(
        &self,
        _args: Value,
        _rec: &dyn crate::types::Rec,
    ) -> Result<Value, crate::tools::ToolError> {
        Err(crate::tools::ToolError::Execute(
            "send_message runs in the spine, not in the brain".to_string(),
        ))
    }
    /// Approval shows the targets and a body preview; the pattern stays the
    /// tool name (the default `RequiresApproval`).
    fn permission(&self, args: &Value) -> ToolPermission {
        let targets = args
            .get("to")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", "))
            .unwrap_or_default();
        let body = args.get("body").and_then(Value::as_str).unwrap_or_default();
        let preview: String = body.chars().take(200).collect();
        ToolPermission::RequiresApproval(vec![PermissionRequest {
            display: format!("send_message to [{targets}]: {preview}"),
            patterns: vec![self.name().to_string()],
        }])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_schema_comes_from_the_library_and_has_no_audience_or_id() {
        let tool = SendMessageTool::new();
        assert_eq!(tool.name(), "send_message");
        let props = tool.parameters()["properties"].as_object().unwrap();
        for forbidden in ["audience", "message_id", "authored_at", "issued_at"] {
            assert!(!props.contains_key(forbidden), "{forbidden}");
        }
        assert!(props.contains_key("to") && props.contains_key("body"));
    }

    #[test]
    fn tool_is_permission_gated_with_a_meaningful_display() {
        let tool = SendMessageTool::new();
        let p = tool.permission(&serde_json::json!({"to": ["room:r1"], "body": "hello"}));
        let ToolPermission::RequiresApproval(reqs) = p else { panic!("not gated") };
        assert_eq!(reqs[0].patterns, vec!["send_message".to_string()]);
        assert!(reqs[0].display.contains("room:r1") && reqs[0].display.contains("hello"));
    }
}

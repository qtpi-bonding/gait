use crate::types::{Tool, ToolPermission, ExecutionSignal};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize, JsonSchema)]
struct ClarifyItemParam {
    /// Unique ID for this question (use a short slug, e.g. "auth-model")
    id: String,
    /// The question to ask the user
    content: String,
    /// What changes in your approach based on the answer
    why: String,
    /// "is" for factual questions, "ought" for judgment/preference questions
    kind: String,
}

#[derive(Deserialize, JsonSchema)]
struct ClarifyParams {
    items: Vec<ClarifyItemParam>,
}

pub struct ClarifyTool {
    parameters: Value,
}

impl ClarifyTool {
    pub fn new() -> Self {
        let parameters = crate::tools::make_schema::<ClarifyParams>();
        Self { parameters }
    }
}

#[async_trait::async_trait]
impl Tool for ClarifyTool {
    fn name(&self) -> &str { "clarify" }
    fn description(&self) -> &str {
        "Ask the user one or more questions and pause until they answer. Use when you need information or judgment from the user before you can proceed correctly. Each item needs an 'id', 'content' (the question), 'why' (what changes based on the answer), and 'kind' ('is' for facts, 'ought' for judgment)."
    }
    fn parameters(&self) -> &Value { &self.parameters }

    fn permission(&self, _args: &serde_json::Value) -> ToolPermission {
        ToolPermission::Passthrough
    }
    fn execution_signal(&self) -> ExecutionSignal {
        ExecutionSignal::Clarify
    }

    async fn execute(&self, _args: Value) -> anyhow::Result<Value> {
        // turn.rs intercepts this call before execution and handles the signal.
        // This path only runs in tests or if called outside turn.rs.
        Ok(Value::String("clarify_signal".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_is_clarify() {
        assert_eq!(ClarifyTool::new().name(), "clarify");
    }

    #[test]
    fn parameters_schema_has_items() {
        let schema = ClarifyTool::new().parameters().clone();
        assert!(schema["properties"]["items"].is_object());
    }

    #[tokio::test]
    async fn execute_returns_signal() {
        let result = ClarifyTool::new()
            .execute(serde_json::json!({"items": []}))
            .await
            .unwrap();
        assert_eq!(result, Value::String("clarify_signal".into()));
    }

    #[test]
    fn clarify_permission_is_passthrough() {
        let tool = ClarifyTool::new();
        assert!(matches!(tool.permission(&serde_json::json!({})), crate::types::ToolPermission::Passthrough));
    }

    #[test]
    fn clarify_execution_signal_is_clarify() {
        let tool = ClarifyTool::new();
        assert_eq!(tool.execution_signal(), crate::types::ExecutionSignal::Clarify);
    }
}

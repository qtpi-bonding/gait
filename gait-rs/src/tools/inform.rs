use crate::types::{Tool, ToolPermission, PermissionRequest, ExecutionSignal};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize, JsonSchema)]
struct InformItemParam {
    /// Unique ID for this explanation (use a short slug, e.g. "rust-ownership")
    id: String,
    /// The topic or concept being explained
    content: String,
    /// What prompted this explanation (optional)
    context: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct InformParams {
    items: Vec<InformItemParam>,
}

pub struct InformTool {
    parameters: Value,
}

impl InformTool {
    pub fn new() -> Self {
        let parameters = crate::tools::make_schema::<InformParams>();
        Self { parameters }
    }
}

#[async_trait::async_trait]
impl Tool for InformTool {
    fn name(&self) -> &str { "inform" }
    fn description(&self) -> &str {
        "Convey an explanation or understanding to the user and pause until they confirm they understood. Use when you want to check that the user correctly understood something important you explained. Each item needs an 'id', 'content' (what to explain), and optionally 'context' (what prompted this)."
    }
    fn parameters(&self) -> &Value { &self.parameters }

    fn permission(&self, _args: &serde_json::Value) -> ToolPermission {
        ToolPermission::RequiresApproval(vec![PermissionRequest {
            display: "inform".to_string(),
            patterns: vec!["inform".to_string()],
        }])
    }
    fn execution_signal(&self) -> ExecutionSignal {
        ExecutionSignal::Inform
    }

    async fn execute(&self, _args: Value) -> anyhow::Result<Value> {
        // turn.rs intercepts this call before execution and handles the signal.
        Ok(Value::String("inform_signal".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_is_inform() {
        assert_eq!(InformTool::new().name(), "inform");
    }

    #[test]
    fn parameters_schema_has_items() {
        let schema = InformTool::new().parameters().clone();
        assert!(schema["properties"]["items"].is_object());
    }

    #[tokio::test]
    async fn execute_returns_signal() {
        let result = InformTool::new()
            .execute(serde_json::json!({"items": []}))
            .await
            .unwrap();
        assert_eq!(result, Value::String("inform_signal".into()));
    }

    #[test]
    fn inform_permission_requires_approval() {
        let tool = InformTool::new();
        match tool.permission(&serde_json::json!({})) {
            crate::types::ToolPermission::RequiresApproval(reqs) => {
                assert_eq!(reqs[0].patterns, vec!["inform"]);
            }
            other => panic!("expected RequiresApproval, got {:?}", other),
        }
    }

    #[test]
    fn inform_execution_signal_is_inform() {
        let tool = InformTool::new();
        assert_eq!(tool.execution_signal(), crate::types::ExecutionSignal::Inform);
    }
}

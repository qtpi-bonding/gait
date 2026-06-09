use crate::types::{Tool, ToolPermission, ExecutionSignal};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize, JsonSchema)]
struct OutputParams {
    /// What the user reads. Markdown OK, multi-paragraph OK, no length cap. Required, non-empty.
    message: String,
    /// Structured JSON result — readable by workflow nodes ($node.output.field) or parent agents
    result: Option<Value>,
}

pub struct OutputTool {
    parameters: Value,
}

impl OutputTool {
    pub fn new() -> Self {
        let parameters = crate::tools::make_schema::<OutputParams>();
        Self { parameters }
    }
}

#[async_trait::async_trait]
impl Tool for OutputTool {
    fn name(&self) -> &str { "output" }
    fn description(&self) -> &str {
        "Return structured output from this turn. Use when the caller needs to read specific fields — in a workflow via $node.output.field, or from a parent agent via spawn_agent. For plain-text completion, just respond — the turn ends automatically."
    }
    fn parameters(&self) -> &Value { &self.parameters }

    fn permission(&self, _args: &serde_json::Value) -> ToolPermission {
        ToolPermission::Passthrough
    }
    fn execution_signal(&self) -> ExecutionSignal {
        ExecutionSignal::TurnComplete
    }

    async fn execute(&self, _args: Value) -> anyhow::Result<Value> {
        Ok(Value::String("recorded".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn execute_returns_recorded() {
        let tool = OutputTool::new();
        let result = tool.execute(serde_json::json!({
            "message": "added health endpoint"
        })).await.unwrap();
        assert_eq!(result, Value::String("recorded".into()));
    }

    #[tokio::test]
    async fn execute_with_result_still_returns_recorded() {
        let tool = OutputTool::new();
        let result = tool.execute(serde_json::json!({
            "message": "done",
            "result": { "files_changed": ["a.rs"] }
        })).await.unwrap();
        assert_eq!(result, Value::String("recorded".into()));
    }

    #[test]
    fn name_is_output() {
        let tool = OutputTool::new();
        assert_eq!(tool.name(), "output");
    }

    #[test]
    fn parameters_schema_has_summary_field() {
        let tool = OutputTool::new();
        let schema = tool.parameters();
        let props = schema["properties"].as_object().unwrap();
        assert!(props.contains_key("message"), "schema missing summary field");
    }

    #[test]
    fn output_permission_is_passthrough() {
        let tool = OutputTool::new();
        assert_eq!(tool.permission(&serde_json::json!({})), crate::types::ToolPermission::Passthrough);
    }

    #[test]
    fn output_execution_signal_is_turn_complete() {
        let tool = OutputTool::new();
        assert_eq!(tool.execution_signal(), crate::types::ExecutionSignal::TurnComplete);
    }
}

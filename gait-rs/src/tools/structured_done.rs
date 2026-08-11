use crate::types::{ExecutionSignal, Tool, ToolPermission};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

/// The canonical name of the turn-completion tool. Referenced by the harness
/// prompt template so the two never drift.
pub const DONE_TOOL_NAME: &str = "structured_done";

#[allow(dead_code)]
#[derive(Deserialize, JsonSchema)]
struct OutputParams {
    /// What the user reads. Markdown OK, multi-paragraph OK. Required, non-empty.
    #[schemars(length(min = 1))]
    message: String,
    /// Structured JSON result — readable by workflow nodes ($node.output.field) or parent agents
    result: Option<Value>,
}

#[allow(dead_code)]
#[derive(Deserialize, JsonSchema)]
struct OutputParamsWithResult {
    /// What the user reads. Markdown OK, multi-paragraph OK. Required, non-empty.
    #[schemars(length(min = 1))]
    message: String,
    /// Structured JSON result that MUST conform to the caller-declared schema.
    result: Value,
}

pub struct StructuredDoneTool {
    parameters: Value,
    result_schema: Option<Value>,
}

impl Default for StructuredDoneTool {
    fn default() -> Self {
        Self::new()
    }
}

impl StructuredDoneTool {
    pub fn new() -> Self {
        let parameters = crate::tools::make_schema::<OutputParams>();
        Self {
            parameters,
            result_schema: None,
        }
    }

    pub fn with_schema(schema: Value) -> Self {
        let parameters = crate::tools::make_schema::<OutputParamsWithResult>();
        Self {
            parameters,
            result_schema: Some(schema),
        }
    }
}

#[async_trait::async_trait]
impl Tool for StructuredDoneTool {
    fn name(&self) -> &str {
        "structured_done"
    }
    fn description(&self) -> &str {
        if self.result_schema.is_some() {
            "Return structured output. The result must match the caller's <result-schema>; invalid results are returned for retry."
        } else {
            "Return structured output when a workflow or parent agent needs specific fields. For plain-text completion, respond normally."
        }
    }
    fn parameters(&self) -> &Value {
        &self.parameters
    }

    fn permission(&self, _args: &serde_json::Value) -> ToolPermission {
        ToolPermission::Passthrough
    }
    fn execution_signal(&self) -> ExecutionSignal {
        ExecutionSignal::FinishTurn
    }

    async fn execute(
        &self,
        _args: Value,
        _rec: &dyn crate::types::Rec,
    ) -> Result<Value, crate::tools::ToolError> {
        Ok(Value::String("recorded".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn execute_returns_recorded() {
        let tool = StructuredDoneTool::new();
        let result = tool
            .execute(
                serde_json::json!({
                    "message": "added health endpoint"
                }),
                &crate::Recorder::detached(),
            )
            .await
            .unwrap();
        assert_eq!(result, Value::String("recorded".into()));
    }

    #[tokio::test]
    async fn execute_with_result_still_returns_recorded() {
        let tool = StructuredDoneTool::new();
        let result = tool
            .execute(
                serde_json::json!({
                    "message": "done",
                    "result": { "files_changed": ["a.rs"] }
                }),
                &crate::Recorder::detached(),
            )
            .await
            .unwrap();
        assert_eq!(result, Value::String("recorded".into()));
    }

    #[test]
    fn name_is_structured_done() {
        let tool = StructuredDoneTool::new();
        assert_eq!(tool.name(), "structured_done");
    }

    #[test]
    fn parameters_schema_has_summary_field() {
        let tool = StructuredDoneTool::new();
        let schema = tool.parameters();
        let props = schema["properties"].as_object().unwrap();
        assert!(
            props.contains_key("message"),
            "schema missing summary field"
        );
    }

    #[test]
    fn output_permission_is_passthrough() {
        let tool = StructuredDoneTool::new();
        assert_eq!(
            tool.permission(&serde_json::json!({})),
            crate::types::ToolPermission::Passthrough
        );
    }

    #[test]
    fn output_execution_signal_finishes_turn() {
        let tool = StructuredDoneTool::new();
        assert_eq!(
            tool.execution_signal(),
            crate::types::ExecutionSignal::FinishTurn
        );
    }

    #[test]
    fn message_schema_requires_non_empty() {
        let tool = StructuredDoneTool::new();
        let schema = tool.parameters();
        let msg = &schema["properties"]["message"];
        assert_eq!(
            msg["minLength"].as_u64(),
            Some(1),
            "message field must have minLength:1 in schema, got: {schema}"
        );
    }
}

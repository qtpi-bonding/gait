use crate::types::{ExecutionSignal, Tool, ToolPermission};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

#[allow(dead_code)]
#[derive(Deserialize, JsonSchema)]
struct InformItemParam {
    /// Unique ID for this explanation (use a short slug, e.g. "rust-ownership")
    id: String,
    /// The topic or concept being explained
    content: String,
    /// What prompted this explanation (optional)
    context: Option<String>,
}

#[allow(dead_code)]
#[derive(Deserialize, JsonSchema)]
struct InformParams {
    items: Vec<InformItemParam>,
}

pub struct InformTool {
    parameters: Value,
}

impl Default for InformTool {
    fn default() -> Self {
        Self::new()
    }
}

impl InformTool {
    pub fn new() -> Self {
        let parameters = crate::tools::make_schema::<InformParams>();
        Self { parameters }
    }
}

#[async_trait::async_trait]
impl Tool for InformTool {
    fn name(&self) -> &str {
        "inform"
    }
    fn description(&self) -> &str {
        "Explain something important to the user and pause for confirmation. Each item needs an id and content; context is optional."
    }
    fn parameters(&self) -> &Value {
        &self.parameters
    }

    fn permission(&self, _args: &serde_json::Value) -> ToolPermission {
        ToolPermission::Passthrough
    }
    fn execution_signal(&self) -> ExecutionSignal {
        ExecutionSignal::Inform
    }

    async fn execute(
        &self,
        _args: Value,
        _rec: &dyn crate::types::Rec,
    ) -> Result<Value, crate::tools::ToolError> {
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
            .execute(
                serde_json::json!({"items": []}),
                &crate::Recorder::detached(),
            )
            .await
            .unwrap();
        assert_eq!(result, Value::String("inform_signal".into()));
    }

    #[test]
    fn inform_permission_is_passthrough() {
        let tool = InformTool::new();
        assert!(matches!(
            tool.permission(&serde_json::json!({})),
            crate::types::ToolPermission::Passthrough
        ));
    }

    #[test]
    fn inform_execution_signal_is_inform() {
        let tool = InformTool::new();
        assert_eq!(
            tool.execution_signal(),
            crate::types::ExecutionSignal::Inform
        );
    }
}

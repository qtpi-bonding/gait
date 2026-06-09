use crate::types::{Tool, ToolPermission, ExecutionSignal};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize, JsonSchema)]
struct AnnotateArgs {
    /// Items from the content identified as relevant to the current task
    relevant: Vec<String>,
    /// Items from the content identified as not relevant to the current task
    irrelevant: Vec<String>,
}

pub struct AnnotateTool {
    parameters: Value,
}

impl AnnotateTool {
    pub fn new() -> Self {
        Self { parameters: crate::tools::make_schema::<AnnotateArgs>() }
    }
}

#[async_trait::async_trait]
impl Tool for AnnotateTool {
    fn name(&self) -> &str { "annotate" }
    fn description(&self) -> &str {
        "Return relevance annotations for items from the provided content. Call once with all relevant and irrelevant items identified. This is the only tool available — call it to complete the task."
    }
    fn parameters(&self) -> &Value { &self.parameters }

    fn permission(&self, _args: &Value) -> ToolPermission {
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

    #[test]
    fn name_is_annotate() {
        assert_eq!(AnnotateTool::new().name(), "annotate");
    }

    #[test]
    fn permission_is_passthrough() {
        let t = AnnotateTool::new();
        assert_eq!(t.permission(&serde_json::json!({})), ToolPermission::Passthrough);
    }

    #[test]
    fn signal_is_turn_complete() {
        let t = AnnotateTool::new();
        assert_eq!(t.execution_signal(), ExecutionSignal::TurnComplete);
    }

    #[test]
    fn schema_has_relevant_and_irrelevant() {
        let t = AnnotateTool::new();
        let schema = t.parameters();
        let props = schema["properties"].as_object().unwrap();
        assert!(props.contains_key("relevant"), "missing relevant");
        assert!(props.contains_key("irrelevant"), "missing irrelevant");
    }

    #[tokio::test]
    async fn execute_returns_recorded() {
        let t = AnnotateTool::new();
        let r = t.execute(serde_json::json!({
            "relevant": ["error on line 3"],
            "irrelevant": ["warning: unused variable"]
        })).await.unwrap();
        assert_eq!(r, Value::String("recorded".into()));
    }
}

//! `run_workflow` — first-class builtin tool that delegates to a
//! [`WorkflowRunner`] injected via [`ToolCtx`]. Present in the tool list only
//! when the runner is wired (`ToolCtx::with_workflow_runner`).

use serde_json::Value;
use std::sync::Arc;

use crate::tools::ToolError;
use crate::types::{PermissionRequest, Tool, ToolPermission};
use crate::workflow_runner::WorkflowRunner;

pub const TOOL_NAME: &str = "run_workflow";

pub struct RunWorkflowTool {
    runner: Arc<dyn WorkflowRunner>,
    schema: Value,
}

impl RunWorkflowTool {
    pub fn new(runner: Arc<dyn WorkflowRunner>) -> Self {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Workflow name (without .yaml). Loaded from \
                                    `{gait_dir}/workflows/{name}.yaml`."
                },
                "args": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional positional arguments forwarded to the workflow. \
                                    Each entry becomes one element of `args` exposed to nodes \
                                    (env var `ARGS`, vars context, etc.)."
                }
            },
            "required": ["name"],
            "additionalProperties": false
        });
        Self { runner, schema }
    }
}

#[async_trait::async_trait]
impl Tool for RunWorkflowTool {
    fn name(&self) -> &str {
        TOOL_NAME
    }

    fn description(&self) -> &str {
        "Start a named workflow from gait-dir/workflows/<name>.yaml. \
         Returns a run_id immediately — the workflow executes asynchronously. \
         Use `gait workflow list` to check status, `gait workflow log <run_id>` \
         to inspect the per-run event log, and `gait workflow continue <run_id>` \
         to resume after a gate pause."
    }

    fn parameters(&self) -> &Value {
        &self.schema
    }

    async fn execute(&self, args: Value, _rec: &crate::Recorder) -> Result<Value, ToolError> {
        let name = match args.get("name").and_then(|v| v.as_str()) {
            Some(n) if !n.is_empty() => n.to_string(),
            Some(_) => {
                return Ok(Value::String(
                    "workflow error: `name` must be non-empty".into(),
                ))
            }
            None => {
                return Ok(Value::String(
                    "workflow error: missing required field: name".into(),
                ))
            }
        };
        let wf_args: Vec<String> = args
            .get("args")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let result = self.runner.run(name, wf_args).await;
        Ok(Value::String(result))
    }

    fn permission(&self, _args: &Value) -> ToolPermission {
        ToolPermission::RequiresApproval(vec![PermissionRequest {
            display: "run_workflow".into(),
            patterns: vec!["run_workflow".into()],
        }])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow_runner::WorkflowRunner;
    use std::sync::Arc;

    #[derive(Debug)]
    struct StubRunner(String);

    #[async_trait::async_trait]
    impl WorkflowRunner for StubRunner {
        async fn run(&self, _name: String, _args: Vec<String>) -> String {
            self.0.clone()
        }
    }

    #[test]
    fn tool_name_is_run_workflow() {
        let t = RunWorkflowTool::new(Arc::new(StubRunner("ok".into())));
        assert_eq!(t.name(), "run_workflow");
    }

    #[test]
    fn schema_requires_name_field() {
        let t = RunWorkflowTool::new(Arc::new(StubRunner("ok".into())));
        let req = t
            .parameters()
            .get("required")
            .and_then(|r| r.as_array())
            .unwrap();
        assert!(req.iter().any(|v| v.as_str() == Some("name")));
    }

    #[tokio::test]
    async fn execute_returns_runner_result() {
        use serde_json::json;
        let t = RunWorkflowTool::new(Arc::new(StubRunner("workflow started: foo-123".into())));
        let rec = crate::Recorder::detached();
        let result = t.execute(json!({"name": "foo"}), &rec).await.unwrap();
        assert_eq!(result.as_str().unwrap(), "workflow started: foo-123");
    }

    #[tokio::test]
    async fn execute_returns_error_string_for_empty_name() {
        use serde_json::json;
        let t = RunWorkflowTool::new(Arc::new(StubRunner("ok".into())));
        let rec = crate::Recorder::detached();
        let result = t.execute(json!({"name": ""}), &rec).await.unwrap();
        assert!(
            result.as_str().unwrap().contains("non-empty"),
            "got: {result}"
        );
    }

    #[tokio::test]
    async fn execute_returns_error_string_for_missing_name() {
        use serde_json::json;
        let t = RunWorkflowTool::new(Arc::new(StubRunner("ok".into())));
        let rec = crate::Recorder::detached();
        let result = t.execute(json!({}), &rec).await.unwrap();
        assert!(result.as_str().unwrap().contains("name"), "got: {result}");
    }
}

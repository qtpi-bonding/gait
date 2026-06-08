use serde::Deserialize;
use schemars::JsonSchema;
use serde_json::Value;
use crate::types::{Tool, ToolPermission, PermissionRequest, ExecutionSignal};
use crate::config::{McpToolDef, SkillDef};

#[derive(Debug, Deserialize, JsonSchema)]
struct LoadParams {
    /// Name of the MCP tool or skill to enable or disable
    name: String,
    /// true = enable (inject into context), false = disable
    enabled: bool,
}

pub struct LoadTool {
    mcp_registry: Vec<McpToolDef>,
    skill_registry: Vec<SkillDef>,
    parameters: Value,
}

impl LoadTool {
    pub fn new(mcp_registry: Vec<McpToolDef>, skill_registry: Vec<SkillDef>) -> Self {
        let parameters = crate::tools::make_schema::<LoadParams>();
        Self { mcp_registry, skill_registry, parameters }
    }
}

#[async_trait::async_trait]
impl Tool for LoadTool {
    fn name(&self) -> &str { "load" }

    fn description(&self) -> &str {
        "Enable or disable an MCP tool or skill for this session. \
         enabled=true activates it; enabled=false deactivates. \
         Check <loadable-index> in the system prompt for available items."
    }

    fn parameters(&self) -> &Value { &self.parameters }

    fn permission(&self, args: &Value) -> ToolPermission {
        let name = args.get("name").and_then(|v| v.as_str()).unwrap_or("*");
        ToolPermission::RequiresApproval(vec![PermissionRequest {
            display: name.to_string(),
            patterns: vec!["load".to_string(), format!("load:{}", name)],
        }])
    }

    fn execution_signal(&self) -> ExecutionSignal { ExecutionSignal::LoadContext }

    async fn execute(&self, args: Value) -> anyhow::Result<Value> {
        let name = args.get("name").and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("load: missing 'name'"))?;
        let enabled = args.get("enabled").and_then(|v| v.as_bool())
            .ok_or_else(|| anyhow::anyhow!("load: missing 'enabled'"))?;

        if !enabled {
            return Ok(Value::String(format!("unloaded: {}", name)));
        }

        if let Some(def) = self.mcp_registry.iter().find(|d| d.name == name) {
            let schema_str = serde_json::to_string_pretty(&def.schema)
                .unwrap_or_else(|_| "{}".into());
            return Ok(Value::String(format!(
                "Tool '{}' loaded.\nDescription: {}\nSchema:\n{}",
                name, def.description, schema_str
            )));
        }

        if let Some(def) = self.skill_registry.iter().find(|d| d.name == name) {
            return Ok(Value::String(def.content.clone()));
        }

        Err(anyhow::anyhow!(
            "load: unknown '{}'. Check <loadable-index> for available items.", name
        ))
    }
}

// Test helpers and tests for the unified LoadTool.
#[cfg(test)]
fn make_mcp_registry() -> Vec<McpToolDef> {
    vec![McpToolDef {
        server_name: "slack".into(),
        name: "slack_send".into(),
        description: "post a message to Slack".into(),
        schema: serde_json::json!({
            "type": "object",
            "properties": { "channel": {"type":"string"}, "text": {"type":"string"} },
            "required": ["channel","text"]
        }),
    }]
}

#[cfg(test)]
fn make_skill_registry() -> Vec<SkillDef> {
    vec![SkillDef {
        name: "tdd".into(),
        description: "test-driven workflow".into(),
        content: "# TDD\n1. Write failing test\n2. Make it pass".into(),
        enabled: true,
    }]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ExecutionSignal, Tool, ToolPermission};

    #[tokio::test]
    async fn load_mcp_returns_schema() {
        let tool = LoadTool::new(make_mcp_registry(), make_skill_registry());
        let result = tool.execute(serde_json::json!({"name":"slack_send","enabled":true})).await.unwrap();
        assert!(result.as_str().unwrap().contains("channel"));
    }

    #[tokio::test]
    async fn load_skill_returns_content() {
        let tool = LoadTool::new(make_mcp_registry(), make_skill_registry());
        let result = tool.execute(serde_json::json!({"name":"tdd","enabled":true})).await.unwrap();
        assert!(result.as_str().unwrap().contains("TDD"));
    }

    #[tokio::test]
    async fn unload_returns_marker() {
        let tool = LoadTool::new(make_mcp_registry(), make_skill_registry());
        let result = tool.execute(serde_json::json!({"name":"tdd","enabled":false})).await.unwrap();
        assert_eq!(result.as_str().unwrap(), "unloaded: tdd");
    }

    #[tokio::test]
    async fn unknown_name_returns_helpful_error() {
        let tool = LoadTool::new(make_mcp_registry(), make_skill_registry());
        let err = tool.execute(serde_json::json!({"name":"no_such","enabled":true})).await;
        assert!(err.unwrap_err().to_string().contains("unknown"));
    }

    #[test]
    fn permission_has_load_prefix_patterns() {
        let tool = LoadTool::new(vec![], vec![]);
        let perm = tool.permission(&serde_json::json!({"name":"tdd","enabled":true}));
        match perm {
            ToolPermission::RequiresApproval(reqs) => {
                assert!(reqs[0].patterns.contains(&"load:tdd".to_string()));
            }
            other => panic!("expected RequiresApproval, got {other:?}"),
        }
    }

    #[test]
    fn execution_signal_is_load_context() {
        assert_eq!(LoadTool::new(vec![], vec![]).execution_signal(),
            ExecutionSignal::LoadContext);
    }
}

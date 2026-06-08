use serde::Deserialize;
use schemars::JsonSchema;
use serde_json::Value;
use crate::types::{Tool, ToolPermission, PermissionRequest, ExecutionSignal};
use crate::config::SkillDef;

#[derive(Debug, Deserialize, JsonSchema)]
struct SkillParams {
    /// Name of the skill to load or unload
    name: String,
    /// true = load (inject skill content into context), false = unload
    context: bool,
}

pub struct SkillTool {
    registry: Vec<SkillDef>,
    parameters: Value,
}

impl SkillTool {
    pub fn new(registry: Vec<SkillDef>) -> Self {
        let parameters = crate::tools::make_schema::<SkillParams>();
        Self { registry, parameters }
    }
}

#[async_trait::async_trait]
impl Tool for SkillTool {
    fn name(&self) -> &str { "skill" }

    fn description(&self) -> &str {
        "Load or unload a skill. Call with context=true to inject the full skill content \
         into the conversation so you can follow it. Call with context=false when done. \
         Check the <skill-index> in the system prompt for available skills."
    }

    fn parameters(&self) -> &Value { &self.parameters }

    fn permission(&self, args: &serde_json::Value) -> ToolPermission {
        let name = args.get("name").and_then(|v| v.as_str()).unwrap_or("*");
        ToolPermission::RequiresApproval(vec![PermissionRequest {
            display: name.to_string(),
            patterns: vec!["skill".to_string(), format!("skill:{}", name)],
        }])
    }
    fn execution_signal(&self) -> ExecutionSignal {
        ExecutionSignal::LoadContext
    }

    async fn execute(&self, args: Value) -> anyhow::Result<Value> {
        let name = args.get("name").and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("skill: missing 'name' argument"))?;
        let context = args.get("context").and_then(|v| v.as_bool())
            .ok_or_else(|| anyhow::anyhow!("skill: missing 'context' argument"))?;

        if !context {
            return Ok(Value::String(format!("skill({}) unloaded", name)));
        }

        let def = self.registry.iter().find(|d| d.name == name)
            .ok_or_else(|| anyhow::anyhow!("skill: unknown skill '{}'. Check <skill-index> for available skills.", name))?;

        Ok(Value::String(def.content.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_registry() -> Vec<SkillDef> {
        vec![SkillDef {
            name: "tdd".into(),
            description: "test-driven development workflow".into(),
            content: "# TDD\n1. Write failing test\n2. Make it pass\n3. Refactor".into(),
        }]
    }

    #[tokio::test]
    async fn load_returns_full_content() {
        let tool = SkillTool::new(make_registry());
        let result = tool.execute(serde_json::json!({ "name": "tdd", "context": true })).await.unwrap();
        let s = result.as_str().unwrap();
        assert!(s.contains("TDD"), "result: {s}");
        assert!(s.contains("failing test"), "result: {s}");
    }

    #[tokio::test]
    async fn unload_returns_marker() {
        let tool = SkillTool::new(make_registry());
        let result = tool.execute(serde_json::json!({ "name": "tdd", "context": false })).await.unwrap();
        assert_eq!(result.as_str().unwrap(), "skill(tdd) unloaded");
    }

    #[tokio::test]
    async fn unknown_skill_name_returns_error() {
        let tool = SkillTool::new(make_registry());
        let err = tool.execute(serde_json::json!({ "name": "no_such_skill", "context": true })).await;
        assert!(err.is_err());
        assert!(err.unwrap_err().to_string().contains("unknown skill"));
    }

    #[tokio::test]
    async fn missing_context_arg_returns_error() {
        let tool = SkillTool::new(make_registry());
        let err = tool.execute(serde_json::json!({ "name": "tdd" })).await;
        assert!(err.is_err());
    }

    #[test]
    fn has_correct_name() {
        let tool = SkillTool::new(vec![]);
        assert_eq!(tool.name(), "skill");
    }

    #[test]
    fn skill_permission_has_name_patterns() {
        let tool = SkillTool::new(vec![]);
        let perm = tool.permission(&serde_json::json!({"name": "tdd", "context": true}));
        match perm {
            crate::types::ToolPermission::RequiresApproval(reqs) => {
                assert_eq!(reqs[0].patterns, vec!["skill", "skill:tdd"]);
            }
            other => panic!("expected RequiresApproval, got {:?}", other),
        }
    }

    #[test]
    fn skill_execution_signal_is_load_context() {
        let tool = SkillTool::new(vec![]);
        assert_eq!(tool.execution_signal(), crate::types::ExecutionSignal::LoadContext);
    }
}

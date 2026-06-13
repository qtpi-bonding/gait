use serde::Deserialize;
use schemars::JsonSchema;
use serde_json::Value;
use crate::types::{Tool, ToolPermission, PermissionRequest, ExecutionSignal};
use crate::config::{McpToolDef, SkillDef, WasmPluginDef};

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
    plugin_registry: Vec<WasmPluginDef>,
    parameters: Value,
}

impl LoadTool {
    pub fn new(mcp_registry: Vec<McpToolDef>, skill_registry: Vec<SkillDef>, plugin_registry: Vec<WasmPluginDef>) -> Self {
        let parameters = crate::tools::make_schema::<LoadParams>();
        Self { mcp_registry, skill_registry, plugin_registry, parameters }
    }
}

#[async_trait::async_trait]
impl Tool for LoadTool {
    fn name(&self) -> &str { "enable" }

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
            patterns: vec!["enable".to_string(), format!("enable:{}", name)],
            preview: None,
        }])
    }

    fn execution_signal(&self) -> ExecutionSignal { ExecutionSignal::LoadContext }

    async fn execute(&self, args: Value) -> anyhow::Result<Value> {
        let name = args.get("name").and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("enable: missing 'name'"))?;
        let enabled = args.get("enabled").and_then(|v| v.as_bool())
            .ok_or_else(|| anyhow::anyhow!("enable: missing 'enabled'"))?;

        if !enabled {
            return Ok(Value::String(format!("disabled: {}", name)));
        }

        if let Some(def) = self.mcp_registry.iter().find(|d| d.name == name) {
            let schema_str = serde_json::to_string_pretty(&def.schema)
                .unwrap_or_else(|_| "{}".into());
            return Ok(Value::String(format!(
                "Tool '{}' enabled.\nDescription: {}\nSchema:\n{}",
                name, def.description, schema_str
            )));
        }

        if let Some(def) = self.skill_registry.iter().find(|d| d.name == name) {
            return Ok(Value::String(def.content.clone()));
        }

        if let Some(def) = self.plugin_registry.iter().find(|d| d.name == name) {
            return Ok(Value::String(format!(
                "Plugin '{}' enabled (execution stubbed; wasm runtime not yet implemented).",
                def.name
            )));
        }

        Err(anyhow::anyhow!(
            "enable: unknown '{}'. Check <loadable-index> for available items.", name
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
        let tool = LoadTool::new(make_mcp_registry(), make_skill_registry(), vec![]);
        let result = tool.execute(serde_json::json!({"name":"slack_send","enabled":true})).await.unwrap();
        assert!(result.as_str().unwrap().contains("channel"));
    }

    #[tokio::test]
    async fn load_skill_returns_content() {
        let tool = LoadTool::new(make_mcp_registry(), make_skill_registry(), vec![]);
        let result = tool.execute(serde_json::json!({"name":"tdd","enabled":true})).await.unwrap();
        assert!(result.as_str().unwrap().contains("TDD"));
    }

    #[tokio::test]
    async fn disable_returns_marker() {
        let tool = LoadTool::new(make_mcp_registry(), make_skill_registry(), vec![]);
        let result = tool.execute(serde_json::json!({"name":"tdd","enabled":false})).await.unwrap();
        assert_eq!(result.as_str().unwrap(), "disabled: tdd");
    }

    #[tokio::test]
    async fn unknown_name_returns_helpful_error() {
        let tool = LoadTool::new(make_mcp_registry(), make_skill_registry(), vec![]);
        let err = tool.execute(serde_json::json!({"name":"no_such","enabled":true})).await;
        assert!(err.unwrap_err().to_string().contains("unknown"));
    }

    #[test]
    fn permission_has_enable_prefix_patterns() {
        let tool = LoadTool::new(vec![], vec![], vec![]);
        let perm = tool.permission(&serde_json::json!({"name":"tdd","enabled":true}));
        match perm {
            ToolPermission::RequiresApproval(reqs) => {
                assert!(reqs[0].patterns.contains(&"enable:tdd".to_string()));
            }
            other => panic!("expected RequiresApproval, got {other:?}"),
        }
    }

    #[test]
    fn execution_signal_is_load_context() {
        assert_eq!(LoadTool::new(vec![], vec![], vec![]).execution_signal(),
            ExecutionSignal::LoadContext);
    }
    #[tokio::test]
    async fn disk_skill_loads_via_load_tool() {
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let skill_dir = dir.path().join("skills").join("retro");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: retro\ndescription: Retrospective interview workflow\n---\n# Retro\nDo a retro.\n",
        ).unwrap();

        // Scanner builds the registry directly from disk (mimics profile.rs path)
        let skill_registry = crate::skills::scan_skills(dir.path());
        assert_eq!(skill_registry.len(), 1, "scanner must find the disk skill");
        assert_eq!(skill_registry[0].name, "retro");
        assert_eq!(skill_registry[0].description, "Retrospective interview workflow");

        // LoadTool returns the markdown body
        let tool = LoadTool::new(vec![], skill_registry, vec![]);
        let result = tool.execute(serde_json::json!({"name":"retro","enabled":true})).await.unwrap();
        assert!(result.as_str().unwrap().contains("Retro"));
    }

    #[test]
    fn disk_skill_appears_in_loadable_index() {
        use tempfile::TempDir;
        use crate::types::{LoadableEntry, LoadableKind};
        let dir = TempDir::new().unwrap();
        let skill_dir = dir.path().join("skills").join("refactor");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: refactor\ndescription: Refactoring patterns\n---\n# Refactor\nclean code.\n",
        ).unwrap();
        let skills = crate::skills::scan_skills(dir.path());
        let entries: Vec<LoadableEntry> = skills.iter().map(|s| LoadableEntry {
            name: s.name.clone(),
            kind: LoadableKind::Skill,
            description: s.description.clone(),
            enabled: s.enabled,
        }).collect();
        let idx = crate::turn::build_loadable_index(&entries);
        assert!(idx.contains("refactor"), "index must list skill name");
        assert!(idx.contains("Refactoring patterns"), "index must show description");
        assert!(idx.contains("enabled=true"), "disk skill defaults to enabled=true");
    }

    #[tokio::test]
    async fn enable_plugin_returns_stub_message() {
        use crate::config::WasmPluginDef;
        let plugins = vec![WasmPluginDef {
            name: "myfmt".into(),
            path: "/tmp/myfmt.wasm".into(),
            enabled: false,
            allow_exec: vec![],
            allow_fs: vec![],
        }];
        let tool = LoadTool::new(vec![], vec![], plugins);
        let result = tool.execute(serde_json::json!({"name": "myfmt", "enabled": true})).await.unwrap();
        assert!(result.as_str().unwrap().contains("myfmt"), "stub should mention name: {:?}", result);
    }

    #[tokio::test]
    async fn disable_plugin_returns_marker() {
        use crate::config::WasmPluginDef;
        let plugins = vec![WasmPluginDef {
            name: "myfmt".into(),
            path: "/tmp/myfmt.wasm".into(),
            enabled: true,
            allow_exec: vec![],
            allow_fs: vec![],
        }];
        let tool = LoadTool::new(vec![], vec![], plugins);
        let result = tool.execute(serde_json::json!({"name": "myfmt", "enabled": false})).await.unwrap();
        assert_eq!(result.as_str().unwrap(), "disabled: myfmt");
    }

}

use serde::Deserialize;
use schemars::JsonSchema;
use serde_json::Value;
use crate::types::Tool;
use crate::config::McpToolDef;

#[derive(Debug, Deserialize, JsonSchema)]
struct McpParams {
    /// Name of the MCP tool to load or unload
    name: String,
    /// true = load (inject schema into context), false = unload
    context: bool,
}

pub struct McpTool {
    registry: Vec<McpToolDef>,
    parameters: Value,
}

impl McpTool {
    pub fn new(registry: Vec<McpToolDef>) -> Self {
        let parameters = crate::tools::make_schema::<McpParams>();
        Self { registry, parameters }
    }

    pub fn boxed(self) -> Box<dyn Tool> {
        Box::new(self)
    }
}

#[async_trait::async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str { "mcp" }

    fn description(&self) -> &str {
        "Load or unload an MCP tool. Call with context=true to inject the tool schema into \
         the conversation (enabling you to call it). Call with context=false to unload it. \
         Check the <mcp-index> in the system prompt for available tools."
    }

    fn parameters(&self) -> &Value { &self.parameters }

    async fn execute(&self, args: Value) -> anyhow::Result<Value> {
        let name = args.get("name").and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("mcp: missing 'name' argument"))?;
        let context = args.get("context").and_then(|v| v.as_bool())
            .ok_or_else(|| anyhow::anyhow!("mcp: missing 'context' argument"))?;

        if !context {
            return Ok(Value::String(format!("unloaded: {}", name)));
        }

        let def = self.registry.iter().find(|d| d.name == name)
            .ok_or_else(|| anyhow::anyhow!("mcp: unknown tool '{}'. Check <mcp-index> for available tools.", name))?;

        let schema_str = serde_json::to_string_pretty(&def.schema)
            .unwrap_or_else(|_| "{}".into());
        Ok(Value::String(format!(
            "Tool '{}' loaded.\nDescription: {}\nSchema:\n{}",
            name, def.description, schema_str
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_registry() -> Vec<McpToolDef> {
        vec![McpToolDef {
            server_name: "slack".into(),
            name: "slack_send".into(),
            description: "post a message to Slack".into(),
            schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "channel": { "type": "string" },
                    "text": { "type": "string" }
                },
                "required": ["channel", "text"]
            }),
        }]
    }

    #[tokio::test]
    async fn load_returns_schema_and_description() {
        let tool = McpTool::new(make_registry());
        let result = tool.execute(serde_json::json!({ "name": "slack_send", "context": true })).await.unwrap();
        let s = result.as_str().unwrap();
        assert!(s.contains("slack_send"), "result: {s}");
        assert!(s.contains("channel"), "schema should be in result: {s}");
    }

    #[tokio::test]
    async fn unload_returns_marker() {
        let tool = McpTool::new(make_registry());
        let result = tool.execute(serde_json::json!({ "name": "slack_send", "context": false })).await.unwrap();
        let s = result.as_str().unwrap();
        assert_eq!(s, "unloaded: slack_send");
    }

    #[tokio::test]
    async fn unknown_tool_name_returns_error() {
        let tool = McpTool::new(make_registry());
        let err = tool.execute(serde_json::json!({ "name": "no_such_tool", "context": true })).await;
        assert!(err.is_err());
        assert!(err.unwrap_err().to_string().contains("unknown tool"));
    }

    #[tokio::test]
    async fn missing_name_arg_returns_error() {
        let tool = McpTool::new(make_registry());
        let err = tool.execute(serde_json::json!({ "context": true })).await;
        assert!(err.is_err());
    }

    #[test]
    fn has_correct_name() {
        let tool = McpTool::new(vec![]);
        assert_eq!(tool.name(), "mcp");
    }
}

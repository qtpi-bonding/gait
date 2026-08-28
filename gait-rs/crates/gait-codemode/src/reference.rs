//! Lossless, deterministic references for enabled MCP servers.

use serde_json::Value;

#[derive(Clone, Debug, PartialEq)]
pub struct ToolReference {
    pub name: String,
    pub description: Option<String>,
    pub input_schema: Value,
    pub output_schema: Option<Value>,
}

impl ToolReference {
    pub fn new(name: impl Into<String>, input_schema: Value) -> Self {
        Self {
            name: name.into(),
            description: None,
            input_schema,
            output_schema: None,
        }
    }
}

fn identifier(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c == '_' || c == '$' || c.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c == '$' || c.is_ascii_alphanumeric())
}

fn access(base: &str, name: &str) -> String {
    if identifier(name) {
        format!("{base}.{name}")
    } else {
        format!(
            "{base}[{}]",
            serde_json::to_string(name).expect("string is serializable")
        )
    }
}

fn canonical(value: &Value) -> String {
    serde_jcs::to_string(value).expect("serde_json::Value is JCS serializable")
}

/// Render a complete direct façade reference, preserving schemas verbatim as
/// canonical JSON and sorting methods by their original MCP name.
pub fn render_reference(server: &str, tools: &[ToolReference]) -> String {
    let server_path = access("gait", server);
    let mut tools = tools.to_vec();
    tools.sort_by(|a, b| a.name.cmp(&b.name));
    let mut rendered = format!("## MCP server `{server}`\n\n");
    for tool in tools {
        let path = access(&server_path, &tool.name);
        let result = if tool.output_schema.is_some() {
            "Promise<unknown>"
        } else {
            "Promise<string>"
        };
        rendered.push_str(&format!("### `{path}`\n\n"));
        if let Some(description) = tool.description {
            rendered.push_str(&description);
            rendered.push_str("\n\n");
        }
        rendered.push_str(&format!("`{path}(args): {result}`\n\n"));
        rendered.push_str("Input JSON Schema (canonical):\n```json\n");
        rendered.push_str(&canonical(&tool.input_schema));
        rendered.push_str("\n```\n");
        if let Some(schema) = tool.output_schema {
            rendered.push_str("Output JSON Schema (canonical):\n```json\n");
            rendered.push_str(&canonical(&schema));
            rendered.push_str("\n```\n");
        } else {
            rendered.push_str(&format!(
                "\nText content can be decoded with `JSON.parse(await {path}(args))`.\n"
            ));
        }
        rendered.push('\n');
    }
    rendered
}

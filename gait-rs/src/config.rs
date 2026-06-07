use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigTool {
    pub name: String,
    pub description: String,
    pub cmd: String,
    #[serde(default = "default_cwd")]
    pub cwd: String,
}

fn default_cwd() -> String { "codebase".into() }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpToolDef {
    pub name: String,
    pub description: String,
    pub schema: serde_json::Value,
    pub cmd: String,
    #[serde(default = "default_cwd")]
    pub cwd: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillDef {
    pub name: String,
    pub description: String,
    pub content: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GaitConfig {
    pub model: String,
    #[serde(default)]
    pub allow: Vec<String>,
    #[serde(default)]
    pub tools: Vec<ConfigTool>,
    #[serde(default = "default_compaction_threshold")]
    pub compaction_threshold: f64,
    #[serde(default)]
    pub mcp_tools: Vec<McpToolDef>,
    #[serde(default)]
    pub skills: Vec<SkillDef>,
}

fn default_compaction_threshold() -> f64 { 0.8 }

impl Default for GaitConfig {
    fn default() -> Self {
        Self {
            model: "anthropic/claude-sonnet-4-6".into(),
            allow: vec!["read".into(), "glob".into(), "grep".into(), "ls".into()],
            tools: vec![],
            compaction_threshold: 0.8,
            mcp_tools: vec![],
            skills: vec![],
        }
    }
}

fn strip_jsonc_comments(input: &str) -> String {
    input.lines().map(|line| {
        // Find // that isn't inside a string (count unescaped " before pos)
        let mut in_string = false;
        let mut prev_char = '\0';
        let chars: Vec<char> = line.chars().collect();
        for i in 0..chars.len() {
            if chars[i] == '"' && prev_char != '\\' { in_string = !in_string; }
            if !in_string && i + 1 < chars.len() && chars[i] == '/' && chars[i+1] == '/' {
                return line[..i].trim_end().to_string();
            }
            prev_char = chars[i];
        }
        line.to_string()
    }).collect::<Vec<_>>().join("\n")
}

pub fn load_config(workspace: &Path) -> GaitConfig {
    let path = workspace.join("gait.jsonc");
    if !path.exists() { return GaitConfig::default(); }
    let raw = std::fs::read_to_string(&path).unwrap_or_default();
    let stripped = strip_jsonc_comments(&raw);
    serde_json::from_str(&stripped).unwrap_or_else(|e| {
        eprintln!("[gait] warning: could not parse gait.jsonc: {e}");
        GaitConfig::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn loads_default_when_no_file() {
        let dir = TempDir::new().unwrap();
        let cfg = load_config(dir.path());
        assert_eq!(cfg.model, "anthropic/claude-sonnet-4-6");
    }

    #[test]
    fn parses_model_and_allow() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("gait.jsonc"), r#"
{
  "model": "minimax/MiniMax-M1",
  "allow": ["read", "write", "shell"]
}
"#).unwrap();
        let cfg = load_config(dir.path());
        assert_eq!(cfg.model, "minimax/MiniMax-M1");
        assert_eq!(cfg.allow, vec!["read", "write", "shell"]);
    }

    #[test]
    fn strips_line_comments() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("gait.jsonc"), r#"
{
  // this is the model
  "model": "anthropic/claude-sonnet-4-6",
  "allow": ["read"] // read is safe
}
"#).unwrap();
        let cfg = load_config(dir.path());
        assert_eq!(cfg.model, "anthropic/claude-sonnet-4-6");
    }

    #[test]
    fn parses_config_defined_tools() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("gait.jsonc"), r#"
{
  "model": "anthropic/claude-sonnet-4-6",
  "allow": [],
  "tools": [
    { "name": "run_tests", "description": "Run tests", "cmd": "cargo test", "cwd": "codebase" },
    { "name": "lint", "description": "Run linter", "cmd": "cargo clippy" }
  ]
}
"#).unwrap();
        let cfg = load_config(dir.path());
        assert_eq!(cfg.tools.len(), 2);
        assert_eq!(cfg.tools[0].name, "run_tests");
        assert_eq!(cfg.tools[0].cmd, "cargo test");
        assert_eq!(cfg.tools[0].cwd, "codebase");
        assert_eq!(cfg.tools[1].name, "lint");
        assert_eq!(cfg.tools[1].cwd, "codebase"); // default
    }

    #[test]
    fn parses_mcp_tools_and_skills() {
        let dir = TempDir::new().unwrap();
        // Build the JSON content using concat! and individual line strings to avoid
        // Rust raw-string backslash-line-continuation issues with literal "\n" sequences.
        let content_str = "# TDD\n1. Write failing test\n2. Make it pass\n3. Refactor";
        let content_json = serde_json::to_string(content_str).unwrap();
        let jsonc = format!(
            "{{\n  \"model\": \"minimax/MiniMax-M1\",\n  \"mcp_tools\": [\n    {{\n      \"name\": \"slack_send\",\n      \"description\": \"post a message to Slack\",\n      \"schema\": {{ \"type\": \"object\", \"properties\": {{ \"channel\": {{ \"type\": \"string\" }}, \"text\": {{ \"type\": \"string\" }} }}, \"required\": [\"channel\", \"text\"] }},\n      \"cmd\": \"node mcp-slack.js\",\n      \"cwd\": \"workspace\"\n    }}\n  ],\n  \"skills\": [\n    {{ \"name\": \"tdd\", \"description\": \"test-driven development workflow\", \"content\": {content_json} }}\n  ]\n}}\n"
        );
        std::fs::write(dir.path().join("gait.jsonc"), jsonc).unwrap();
        let cfg = load_config(dir.path());
        assert_eq!(cfg.mcp_tools.len(), 1);
        assert_eq!(cfg.mcp_tools[0].name, "slack_send");
        assert_eq!(cfg.mcp_tools[0].cmd, "node mcp-slack.js");
        assert_eq!(cfg.mcp_tools[0].cwd, "workspace");
        assert!(cfg.mcp_tools[0].schema.is_object());
        assert_eq!(cfg.skills.len(), 1);
        assert_eq!(cfg.skills[0].name, "tdd");
        assert!(cfg.skills[0].content.contains("TDD"));
    }
}

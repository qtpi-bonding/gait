use serde::{Deserialize, Serialize};
use std::path::Path;

pub const DEFAULT_MODEL: &str = "anthropic/claude-sonnet-4-6";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigTool {
    pub name: String,
    pub description: String,
    pub cmd: String,
    #[serde(default = "default_cwd")]
    pub cwd: String,
}

fn default_cwd() -> String { "codebase".into() }

fn default_enabled() -> bool { true }

/// Config-level: how to connect to one MCP server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerDef {
    pub name: String,
    /// Ecosystem-standard stdio: executable name (e.g. "npx", "uvx", "node")
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Ecosystem-standard stdio: arguments to pass to the command
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,
    /// Ecosystem-standard stdio: environment variables to set
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<std::collections::HashMap<String, String>>,
    /// Ecosystem-standard HTTP: URL of an already-running MCP server
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Deprecated compat: full "prog args…" string, equivalent to command+args
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spawn: Option<String>,
    /// Deprecated compat: HTTP URL, equivalent to url
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connect: Option<String>,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// Optional description for the <loadable-index>.
    #[serde(default)]
    pub description: Option<String>,
}

/// Runtime: one tool discovered from an MCP server via tools/list.
/// Populated by McpClient::discover_tools(), not from config.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpToolDef {
    pub server_name: String,
    pub name: String,
    pub description: String,
    pub schema: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillDef {
    pub name: String,
    pub description: String,
    pub content: String,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

/// Stub for future WASM plugin support.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WasmPluginDef {
    pub name: String,
    pub path: String,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub allow_exec: Vec<String>,
    #[serde(default)]
    pub allow_fs: Vec<String>,
}
/// One entry in the `gait.jsonc` `"hooks"` array.
/// `event` is the lifecycle point ("post_tool", "pre_tool", "post_turn", "pre_turn").
/// `tool` filters to a specific tool name; absent means all tools.
/// `run` is a shell command; env vars `GAIT_TOOL_NAME`, `GAIT_TOOL_RESULT`,
/// and `GAIT_ARG_<KEY>` (one per tool arg, KEY uppercased) are set before execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookDef {
    pub event: String,
    #[serde(default)]
    pub tool: Option<String>,
    pub run: String,
}

/// When to render edit/write previews. `on-ask` (default): only at the permission
/// prompt. `always`: also stream the diff live (ephemeral) for auto/allow-listed
/// tools. `off`: never.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum PreviewPolicy {
    #[default]
    OnAsk,
    Always,
    Off,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GaitConfig {
    pub model: String,
    #[serde(default)]
    pub allow: Vec<String>,
    #[serde(default)]
    pub deny: Vec<String>,
    #[serde(default)]
    pub tools: Vec<ConfigTool>,
    #[serde(default = "default_compaction_threshold")]
    pub compaction_threshold: f64,
    #[serde(default)]
    pub mcp_servers: Vec<McpServerDef>,
    #[serde(default)]
    /// Deprecated (O4): skill bodies should live in skills/<name>/SKILL.md.
    pub skills: Vec<SkillDef>,
    #[serde(default)]
    pub wasm_plugins: Vec<WasmPluginDef>,
    #[serde(default)]
    pub hooks: Vec<HookDef>,
    /// Max retries when an LLM result fails output_schema validation.
    #[serde(default = "default_output_schema_retries")]
    pub output_schema_retries: u8,
    /// Max turn iterations before giving up. Defaults to 100 when absent.
    #[serde(default)]
    pub max_iter: Option<usize>,
    /// When to render edit/write previews. Defaults to on-ask.
    #[serde(default)]
    pub preview: PreviewPolicy,

    /// JSON Schema that `result` in `structured_done` must conform to.
    /// If set, harness validates and retries on mismatch (up to output_schema_retries).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_schema: Option<serde_json::Value>,
}

fn default_compaction_threshold() -> f64 { 0.8 }

fn default_output_schema_retries() -> u8 { 3 }

impl Default for GaitConfig {
    fn default() -> Self {
        Self {
            model: DEFAULT_MODEL.into(),
            allow: vec![
                "read".into(), "glob".into(), "grep".into(), "ls".into(),
                "clarify".into(), "inform".into(),
            ],
            deny: vec![],
            tools: vec![],
            compaction_threshold: 0.8,
            mcp_servers: vec![],
            skills: vec![],
            wasm_plugins: vec![],
            hooks: vec![],
            output_schema_retries: 3,
            max_iter: None,
            preview: PreviewPolicy::OnAsk,
            result_schema: None,
        }
    }
}

pub fn strip_jsonc_comments(input: &str) -> String {
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

pub fn load_config(gait_dir: &Path) -> GaitConfig {
    let path = gait_dir.join("gait.jsonc");
    if !path.exists() { return GaitConfig::default(); }
    let raw = std::fs::read_to_string(&path).unwrap_or_default();
    let stripped = strip_jsonc_comments(&raw);
    let config: GaitConfig = serde_json::from_str(&stripped).unwrap_or_else(|e| {
        eprintln!("[gait] warning: could not parse gait.jsonc: {e}");
        GaitConfig::default()
    });
    // O4: inline skill bodies in gait.jsonc are deprecated; use skills/<name>/SKILL.md.
    if !config.skills.is_empty() {
        eprintln!("[gait] warning: gait.jsonc `skills:` field is deprecated (O4). \
                   Move skill bodies to skills/<name>/SKILL.md");
    }
    config
}

pub fn append_allow_pattern(gait_dir: &Path, pattern: &str) -> anyhow::Result<()> {
    let path = gait_dir.join("gait.jsonc");
    let mut obj: serde_json::Map<String, serde_json::Value> = if path.exists() {
        let raw = std::fs::read_to_string(&path)?;
        let stripped = strip_jsonc_comments(&raw);
        match serde_json::from_str::<serde_json::Value>(&stripped) {
            Ok(serde_json::Value::Object(m)) => m,
            Ok(_) => anyhow::bail!("gait.jsonc root is not a JSON object"),
            Err(e) => anyhow::bail!("gait.jsonc parse error: {e}"),
        }
    } else {
        let mut m = serde_json::Map::new();
        m.insert("model".into(), serde_json::Value::String(DEFAULT_MODEL.into()));
        m
    };
    let allow = obj.entry("allow")
        .or_insert_with(|| serde_json::Value::Array(vec![]))
        .as_array_mut()
        .ok_or_else(|| anyhow::anyhow!("allow field is not an array"))?;
    if !allow.iter().any(|v| v.as_str() == Some(pattern)) {
        allow.push(serde_json::Value::String(pattern.to_string()));
    }
    let pretty = serde_json::to_string_pretty(&serde_json::Value::Object(obj))?;
    std::fs::write(&path, pretty + "\n")?;
    Ok(())
}

pub fn append_deny_pattern(gait_dir: &Path, pattern: &str) -> anyhow::Result<()> {
    let path = gait_dir.join("gait.jsonc");
    let mut obj: serde_json::Map<String, serde_json::Value> = if path.exists() {
        let raw = std::fs::read_to_string(&path)?;
        let stripped = strip_jsonc_comments(&raw);
        match serde_json::from_str::<serde_json::Value>(&stripped) {
            Ok(serde_json::Value::Object(m)) => m,
            Ok(_) => anyhow::bail!("gait.jsonc root is not a JSON object"),
            Err(e) => anyhow::bail!("gait.jsonc parse error: {e}"),
        }
    } else {
        let mut m = serde_json::Map::new();
        m.insert("model".into(), serde_json::Value::String(DEFAULT_MODEL.into()));
        m
    };
    let deny = obj.entry("deny")
        .or_insert_with(|| serde_json::Value::Array(vec![]))
        .as_array_mut()
        .ok_or_else(|| anyhow::anyhow!("deny field is not an array"))?;
    if !deny.iter().any(|v| v.as_str() == Some(pattern)) {
        deny.push(serde_json::Value::String(pattern.to_string()));
    }
    let pretty = serde_json::to_string_pretty(&serde_json::Value::Object(obj))?;
    std::fs::write(&path, pretty + "\n")?;
    Ok(())
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
  "model": "minimax/MiniMax-M3",
  "allow": ["read", "write", "shell"]
}
"#).unwrap();
        let cfg = load_config(dir.path());
        assert_eq!(cfg.model, "minimax/MiniMax-M3");
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
    fn append_allow_pattern_adds_to_existing_allow() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("gait.jsonc"), r#"{"model":"minimax/MiniMax-M3","allow":["read"]}"#).unwrap();
        append_allow_pattern(dir.path(), "shell:cargo *").unwrap();
        let cfg = load_config(dir.path());
        assert!(cfg.allow.contains(&"shell:cargo *".to_string()));
    }

    #[test]
    fn append_allow_pattern_preserves_model_field() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("gait.jsonc"), r#"{"model":"minimax/MiniMax-M3","allow":[]}"#).unwrap();
        append_allow_pattern(dir.path(), "shell:git *").unwrap();
        assert_eq!(load_config(dir.path()).model, "minimax/MiniMax-M3");
    }

    #[test]
    fn append_allow_pattern_creates_file_when_missing() {
        let dir = TempDir::new().unwrap();
        append_allow_pattern(dir.path(), "shell:cargo *").unwrap();
        assert!(load_config(dir.path()).allow.contains(&"shell:cargo *".to_string()));
    }

    #[test]
    fn append_allow_pattern_deduplicates() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("gait.jsonc"), r#"{"model":"minimax/MiniMax-M3","allow":["shell:cargo *"]}"#).unwrap();
        append_allow_pattern(dir.path(), "shell:cargo *").unwrap();
        assert_eq!(load_config(dir.path()).allow.iter().filter(|p| p.as_str() == "shell:cargo *").count(), 1);
    }

    #[test]
    fn append_allow_pattern_bails_on_parse_error() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("gait.jsonc"), "{ invalid json").unwrap();
        assert!(append_allow_pattern(dir.path(), "shell:cargo *").is_err());
        // Must NOT overwrite the broken file
        assert!(std::fs::read_to_string(dir.path().join("gait.jsonc")).unwrap().contains("invalid json"));
    }

    #[test]
    fn parses_mcp_servers_spawn_and_connect() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("gait.jsonc"), r#"
{
  "model": "anthropic/claude-sonnet-4-6",
  "mcp_servers": [
    { "name": "fs", "spawn": "npx @modelcontextprotocol/server-filesystem ." },
    { "name": "remote", "connect": "http://localhost:3001/mcp" }
  ]
}
"#).unwrap();
        let cfg = load_config(dir.path());
        assert_eq!(cfg.mcp_servers.len(), 2);
        assert_eq!(cfg.mcp_servers[0].name, "fs");
        assert!(cfg.mcp_servers[0].spawn.is_some());
        assert!(cfg.mcp_servers[0].connect.is_none());
        assert_eq!(cfg.mcp_servers[1].name, "remote");
        assert!(cfg.mcp_servers[1].connect.is_some());
        assert!(cfg.mcp_servers[1].spawn.is_none());
    }

    #[test]
    fn append_allow_creates_file_with_correct_default_model() {
        let dir = TempDir::new().unwrap();
        // No gait.jsonc exists yet — append_allow_pattern must create one
        append_allow_pattern(dir.path(), "read").unwrap();
        let content = std::fs::read_to_string(dir.path().join("gait.jsonc")).unwrap();
        let val: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(
            val["model"].as_str().unwrap(),
            "anthropic/claude-sonnet-4-6",
            "newly created gait.jsonc must use the same default model as GaitConfig::default()"
        );
    }

    #[test]
    fn enabled_defaults_to_true_for_backward_compat() {
        let json = r##"{ "model": "anthropic/claude-sonnet-4-6",
            "mcp_servers": [{ "name": "slack", "spawn": "node mcp-slack.js" }],
            "skills": [{ "name": "tdd", "description": "tdd workflow", "content": "# TDD" }]
        }"##;
        let cfg: GaitConfig = serde_json::from_str(json).unwrap();
        assert!(cfg.mcp_servers[0].enabled, "mcp without enabled field should default true");
        assert!(cfg.skills[0].enabled, "skill without enabled field should default true");
    }

    #[test]
    fn enabled_false_is_respected() {
        let json = r#"{ "model": "anthropic/claude-sonnet-4-6",
            "mcp_servers": [{ "name": "slack", "spawn": "node mcp-slack.js", "enabled": false }]
        }"#;
        let cfg: GaitConfig = serde_json::from_str(json).unwrap();
        assert!(!cfg.mcp_servers[0].enabled);
    }

    #[test]
    fn wasm_plugin_def_parses() {
        let json = r#"{ "model": "anthropic/claude-sonnet-4-6",
            "wasm_plugins": [{ "name": "formatter", "path": "plugins/fmt.wasm", "enabled": true }]
        }"#;
        let cfg: GaitConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.wasm_plugins[0].name, "formatter");
        assert!(cfg.wasm_plugins[0].enabled);
    }

    #[test]
    fn parses_mcp_servers_and_skills() {
        let dir = TempDir::new().unwrap();
        let content_str = "# TDD\n1. Write failing test\n2. Make it pass\n3. Refactor";
        let content_json = serde_json::to_string(content_str).unwrap();
        let jsonc = format!(
            "{{\n  \"model\": \"minimax/MiniMax-M3\",\n  \"mcp_servers\": [\n    {{ \"name\": \"slack\", \"spawn\": \"node mcp-slack.js\" }}\n  ],\n  \"skills\": [\n    {{ \"name\": \"tdd\", \"description\": \"test-driven development workflow\", \"content\": {content_json} }}\n  ]\n}}\n"
        );
        std::fs::write(dir.path().join("gait.jsonc"), jsonc).unwrap();
        let cfg = load_config(dir.path());
        assert_eq!(cfg.mcp_servers.len(), 1);
        assert_eq!(cfg.mcp_servers[0].name, "slack");
        assert_eq!(cfg.mcp_servers[0].spawn.as_deref(), Some("node mcp-slack.js"));
        assert_eq!(cfg.skills.len(), 1);
    }

    #[test]
    fn parses_deny_list() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("gait.jsonc"), r#"
{
  "model": "minimax/MiniMax-M3",
  "allow": ["read"],
  "deny": ["shell:rm *", "shell:git push *"]
}
"#).unwrap();
        let cfg = load_config(dir.path());
        assert_eq!(cfg.deny, vec!["shell:rm *", "shell:git push *"]);
    }

    #[test]
    fn deny_defaults_to_empty() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("gait.jsonc"), r#"{"model":"minimax/MiniMax-M3","allow":[]}"#).unwrap();
        let cfg = load_config(dir.path());
        assert!(cfg.deny.is_empty());
    }

    #[test]
    fn append_deny_pattern_adds_to_deny() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("gait.jsonc"), r#"{"model":"minimax/MiniMax-M3","allow":[],"deny":[]}"#).unwrap();
        append_deny_pattern(dir.path(), "shell:rm *").unwrap();
        let cfg = load_config(dir.path());
        assert!(cfg.deny.contains(&"shell:rm *".to_string()));
    }

    #[test]
    fn append_deny_pattern_deduplicates() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("gait.jsonc"), r#"{"model":"minimax/MiniMax-M3","allow":[],"deny":["shell:rm *"]}"#).unwrap();
        append_deny_pattern(dir.path(), "shell:rm *").unwrap();
        let cfg = load_config(dir.path());
        assert_eq!(cfg.deny.iter().filter(|p| p.as_str() == "shell:rm *").count(), 1);
    }

    #[test]
    fn output_schema_retries_defaults_to_3() {
        let cfg = GaitConfig::default();
        assert_eq!(cfg.output_schema_retries, 3);
    }

    #[test]
    fn preview_policy_parses_from_jsonc() {
        let cfg: GaitConfig = serde_json::from_str(r#"{"model":"m","preview":"always"}"#).unwrap();
        assert_eq!(cfg.preview, PreviewPolicy::Always);
        let cfg: GaitConfig = serde_json::from_str(r#"{"model":"m","preview":"off"}"#).unwrap();
        assert_eq!(cfg.preview, PreviewPolicy::Off);
        // absent → default on-ask
        let cfg: GaitConfig = serde_json::from_str(r#"{"model":"m"}"#).unwrap();
        assert_eq!(cfg.preview, PreviewPolicy::OnAsk);
    }

    #[test]
    fn output_schema_retries_parses_from_json() {
        let json = r#"{"model":"test","output_schema_retries":5}"#;
        let cfg: GaitConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.output_schema_retries, 5);
    }
}

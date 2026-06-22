use serde::{Deserialize, Serialize};
use std::path::Path;

pub mod defaults;
pub mod resolver;

pub use resolver::{ConfigResolver, Overrides};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigTool {
    pub name: String,
    pub description: String,
    pub cmd: String,
    #[serde(default = "default_cwd")]
    pub cwd: String,
}

fn default_cwd() -> String {
    "codebase".into()
}

fn default_enabled() -> bool {
    true
}

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
/// `event` is the lifecycle point: "pre_turn" | "pre_step" | "post_step" | "pre_tool" | "post_tool" | "post_turn".
/// `tool` filters to a specific tool name (pre_tool / post_tool only); absent means all tools.
/// `run` is a shell command. All `GAIT_*` env vars are set before execution — see docs/hooks.md.

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookDef {
    pub event: String,
    #[serde(default)]
    pub tool: Option<String>,
    pub run: String,
}

// ── Sandbox ────────────────────────────────────────────────────────────────
// `NetworkConfig` deliberately uses a custom Deserialize (NOT `#[serde(untagged)]`)
// so that a bare hostname string like "github.com" is treated as an allowlist of
// one host — not silently coerced into the All-egress variant by serde's untagged
// matching.

/// Network policy for the sandbox.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum NetworkConfig {
    /// No network access (default).
    #[default]
    Off,
    /// Allow-list of hostnames.
    Hosts(Vec<String>),
    /// Allow all egress.
    All,
}

impl<'de> serde::Deserialize<'de> for NetworkConfig {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::{self, Unexpected};
        struct V;
        impl<'de> de::Visitor<'de> for V {
            type Value = NetworkConfig;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                write!(
                    f,
                    "false, \"*\", a hostname string, or an array of hostnames"
                )
            }
            fn visit_bool<E: de::Error>(self, v: bool) -> Result<NetworkConfig, E> {
                if !v {
                    Ok(NetworkConfig::Off)
                } else {
                    Err(E::invalid_value(Unexpected::Bool(v), &self))
                }
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<NetworkConfig, E> {
                if v == "*" {
                    Ok(NetworkConfig::All)
                } else {
                    Ok(NetworkConfig::Hosts(vec![v.to_owned()]))
                }
            }
            fn visit_seq<A: de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<NetworkConfig, A::Error> {
                let mut hosts = Vec::new();
                while let Some(h) = seq.next_element::<String>()? {
                    hosts.push(h);
                }
                Ok(NetworkConfig::Hosts(hosts))
            }
        }
        d.deserialize_any(V)
    }
}

impl serde::Serialize for NetworkConfig {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            NetworkConfig::Off => s.serialize_bool(false),
            NetworkConfig::All => s.serialize_str("*"),
            NetworkConfig::Hosts(h) => h.serialize(s),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxConfig {
    /// Whether the sandbox is engaged. Default: true (strict).
    #[serde(default = "default_true")]
    pub on: bool,
    /// Paths the sandbox allows reading from.
    #[serde(default)]
    pub readable: Vec<String>,
    /// Paths the sandbox allows writing to.
    #[serde(default)]
    pub writable: Vec<String>,
    /// Network policy applied to sandboxed processes.
    #[serde(default)]
    pub network: NetworkConfig,
}

fn default_true() -> bool {
    true
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            on: true,
            readable: vec![],
            writable: vec![],
            network: NetworkConfig::default(),
        }
    }
}

// ── LLM parameter surface ─────────────────────────────────────────────────────
// These serde structs are the JSON-facing layer (gait.jsonc + agent frontmatter).
// The proto `LlmParams` message family is the log-facing layer.
// Conversion: events::llm_params_to_proto(&LlmParamsConfig) → pb::LlmParams.

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AnthropicThinkingConfig {
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>, // "adaptive"|"disabled"|"enabled"
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<String>, // "summarized"|"omitted"
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AnthropicLlmParamsConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>, // "low"|"medium"|"high"|"xhigh"|"max"
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_k: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<AnthropicThinkingConfig>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OpenAILlmParamsConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>, // "none"|"minimal"|"low"|"medium"|"high"|"xhigh"
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frequency_penalty: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub presence_penalty: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<i64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stop: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GeminiThinkingConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_thoughts: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<String>, // "minimal"|"low"|"medium"|"high"
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GeminiLlmParamsConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_k: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate_count: Option<i32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stop_sequences: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frequency_penalty: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub presence_penalty: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<GeminiThinkingConfig>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LlmParamsConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    // NOTE: show_thinking is intentionally absent here — it belongs to GaitConfig
    // as a pure UI render toggle. ThinkingBlock events are always written to the log.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anthropic: Option<AnthropicLlmParamsConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub openai: Option<OpenAILlmParamsConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gemini: Option<GeminiLlmParamsConfig>,
}

impl LlmParamsConfig {
    /// Merge config-level params with agent-level params. Agent fields win
    /// (agent-specific settings override the workspace defaults).
    pub fn merged_with(self, agent: LlmParamsConfig) -> LlmParamsConfig {
        LlmParamsConfig {
            max_tokens: agent.max_tokens.or(self.max_tokens),
            temperature: agent.temperature.or(self.temperature),
            top_p: agent.top_p.or(self.top_p),
            anthropic: agent.anthropic.or(self.anthropic),
            openai: agent.openai.or(self.openai),
            gemini: agent.gemini.or(self.gemini),
        }
    }
}

/// When to render edit/write previews. `on-ask` (default): only at the permission
/// prompt. `always`: also stream the diff live (ephemeral) for auto/allow-listed
/// tools. `off`: never.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum PreviewPolicy {
    #[default]
    OnAsk,
    Always,
    Off,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GaitConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
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

    /// LLM parameter overrides: temperature, max_tokens, thinking, effort, etc.
    /// Merged with agent frontmatter (agent wins per-field).
    #[serde(default)]
    pub llm_params: LlmParamsConfig,

    /// Whether to render thinking/reasoning blocks in the terminal output.
    /// This is a pure UI preference — ThinkingBlock events are ALWAYS written
    /// to the session log regardless of this flag. Default: false.
    #[serde(default)]
    pub show_thinking: bool,

    /// Sandbox policy for shell/process/file execution. Strict-by-default
    /// (`on: true`, empty allow-lists, network off) so that engagement is
    /// safe unless the user explicitly opts in.
    #[serde(default)]
    pub sandbox: SandboxConfig,
}

fn default_compaction_threshold() -> f64 {
    defaults::default_compaction_threshold()
}

fn default_output_schema_retries() -> u8 {
    defaults::default_output_schema_retries()
}

impl Default for GaitConfig {
    fn default() -> Self {
        Self {
            model: defaults::default_model(),
            allow: defaults::default_allow(),
            deny: defaults::default_deny(),
            tools: vec![],
            compaction_threshold: defaults::default_compaction_threshold(),
            mcp_servers: vec![],
            skills: vec![],
            wasm_plugins: vec![],
            hooks: vec![],
            output_schema_retries: defaults::default_output_schema_retries(),
            max_iter: defaults::default_max_iter(),
            preview: defaults::default_preview(),
            result_schema: None,
            llm_params: LlmParamsConfig::default(),
            show_thinking: defaults::default_show_thinking(),
            sandbox: defaults::default_sandbox(),
        }
    }
}

pub fn strip_jsonc_comments(input: &str) -> String {
    input
        .lines()
        .map(|line| {
            // Find // that isn't inside a string (count unescaped " before pos)
            let mut in_string = false;
            let mut prev_char = '\0';
            let chars: Vec<char> = line.chars().collect();
            for i in 0..chars.len() {
                if chars[i] == '"' && prev_char != '\\' {
                    in_string = !in_string;
                }
                if !in_string && i + 1 < chars.len() && chars[i] == '/' && chars[i + 1] == '/' {
                    return line[..i].trim_end().to_string();
                }
                prev_char = chars[i];
            }
            line.to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn load_config(gait_dir: &Path) -> GaitConfig {
    let path = gait_dir.join("gait.jsonc");
    if !path.exists() {
        return GaitConfig::default();
    }
    let raw = std::fs::read_to_string(&path).unwrap_or_default();
    let stripped = strip_jsonc_comments(&raw);
    // Parse failures silently fall back to defaults; the library must not write to stderr.
    let config: GaitConfig =
        serde_json::from_str(&stripped).unwrap_or_else(|_| GaitConfig::default());
    // O4: the `skills:` field is deprecated. Dropped silently — a library must not write to stderr.
    let _ = &config.skills;
    config
}

fn append_list_field(field: &str, pattern: &str, path: &Path) -> anyhow::Result<()> {
    let mut obj: serde_json::Map<String, serde_json::Value> = if path.exists() {
        let raw = std::fs::read_to_string(path)?;
        let stripped = strip_jsonc_comments(&raw);
        match serde_json::from_str::<serde_json::Value>(&stripped) {
            Ok(serde_json::Value::Object(m)) => m,
            Ok(_) => anyhow::bail!("gait.jsonc root is not a JSON object"),
            Err(e) => anyhow::bail!("gait.jsonc parse error: {e}"),
        }
    } else {
        serde_json::Map::new()
    };
    let list = obj
        .entry(field)
        .or_insert_with(|| serde_json::Value::Array(vec![]))
        .as_array_mut()
        .ok_or_else(|| anyhow::anyhow!("{field} field is not an array"))?;
    if !list.iter().any(|v| v.as_str() == Some(pattern)) {
        list.push(serde_json::Value::String(pattern.to_string()));
    }
    let pretty = serde_json::to_string_pretty(&serde_json::Value::Object(obj))?;
    std::fs::write(path, pretty + "\n")?;
    Ok(())
}

pub fn append_allow_pattern(gait_dir: &Path, pattern: &str) -> anyhow::Result<()> {
    append_list_field("allow", pattern, &gait_dir.join("gait.jsonc"))
}

pub fn append_deny_pattern(gait_dir: &Path, pattern: &str) -> anyhow::Result<()> {
    append_list_field("deny", pattern, &gait_dir.join("gait.jsonc"))
}

/// Append a value to a list nested at `obj["sandbox"][field]`, creating the
/// sandbox block and/or array if absent. Strips JSONC comments (consistent
/// with pre-existing behavior of `append_list_field`).
fn append_sandbox_field(field: &str, value: &str, path: &Path) -> anyhow::Result<()> {
    let mut obj: serde_json::Map<String, serde_json::Value> = if path.exists() {
        let raw = std::fs::read_to_string(path)?;
        let stripped = strip_jsonc_comments(&raw);
        match serde_json::from_str::<serde_json::Value>(&stripped) {
            Ok(serde_json::Value::Object(m)) => m,
            Ok(_) => anyhow::bail!("gait.jsonc root is not a JSON object"),
            Err(e) => anyhow::bail!("gait.jsonc parse error: {e}"),
        }
    } else {
        serde_json::Map::new()
    };

    // Navigate/create sandbox block
    let sandbox = obj
        .entry("sandbox")
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()))
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("gait.jsonc 'sandbox' is not an object"))?;

    let list = sandbox
        .entry(field)
        .or_insert_with(|| serde_json::Value::Array(vec![]))
        .as_array_mut()
        .ok_or_else(|| anyhow::anyhow!("sandbox.{field} is not an array"))?;

    if !list.iter().any(|v| v.as_str() == Some(value)) {
        list.push(serde_json::Value::String(value.to_string()));
    }

    let pretty = serde_json::to_string_pretty(&serde_json::Value::Object(obj))?;
    std::fs::write(path, pretty + "\n")?;
    Ok(())
}

/// Append a path to `gait.jsonc` → `sandbox.writable` (global grant persistence).
pub fn append_sandbox_writable(gait_dir: &Path, path: &str) -> anyhow::Result<()> {
    append_sandbox_field("writable", path, &gait_dir.join("gait.jsonc"))
}

/// Append a path to `gait.jsonc` → `sandbox.readable` (global grant persistence).
pub fn append_sandbox_readable(gait_dir: &Path, path: &str) -> anyhow::Result<()> {
    append_sandbox_field("readable", path, &gait_dir.join("gait.jsonc"))
}

/// Append a host to `gait.jsonc` → `sandbox.network` (global grant persistence).
/// If `sandbox.network` is currently `false` (Off), it becomes `[host]`.
/// If it is `"*"` (All), it stays `"*"` (already maximally permissive).
pub fn append_sandbox_network_host(gait_dir: &Path, host: &str) -> anyhow::Result<()> {
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
        serde_json::Map::new()
    };

    let sandbox = obj
        .entry("sandbox")
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()))
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("gait.jsonc 'sandbox' is not an object"))?;

    // If network is "*" (All), nothing to do — already maximally permissive.
    if sandbox.get("network").and_then(|v| v.as_str()) == Some("*") {
        return Ok(());
    }

    // Normalize to an array (handles false / absent / existing array)
    let list = match sandbox.get_mut("network") {
        Some(serde_json::Value::Array(arr)) => arr,
        _ => {
            sandbox.insert("network".to_string(), serde_json::Value::Array(vec![]));
            sandbox["network"].as_array_mut().unwrap()
        }
    };

    if !list.iter().any(|v| v.as_str() == Some(host)) {
        list.push(serde_json::Value::String(host.to_string()));
    }

    let pretty = serde_json::to_string_pretty(&serde_json::Value::Object(obj))?;
    std::fs::write(&path, pretty + "\n")?;
    Ok(())
}

#[cfg(test)]
mod tests;

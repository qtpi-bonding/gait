use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::Path;

pub mod defaults;
pub mod resolver;

pub use resolver::{ConfigResolver, Overrides};

/// A config-defined shell-command tool exposed to the model.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ConfigTool {
    /// Name used to invoke the tool.
    pub name: String,
    /// Description shown to the model.
    pub description: String,
    /// Shell command to run.
    pub cmd: String,
    /// Working directory for the command; defaults to `codebase`.
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillDef {
    pub name: String,
    pub description: String,
    pub content: String,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// Whether this skill is exposed to model-facing catalogs and loading.
    #[serde(default = "default_model_invocable")]
    pub model_invocable: bool,
}

fn default_model_invocable() -> bool {
    true
}

/// Definition of a disk-discovered WASM plugin.
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
/// One entry in the `gait.jsonc` `"hooks"` array, running a shell command at a lifecycle event.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct HookDef {
    /// Lifecycle event: `pre_turn`, `pre_step`, `post_step`, `pre_tool`, `post_tool`, or `post_turn`.
    pub event: String,
    /// Optional tool-name filter, applicable to `pre_tool` and `post_tool`; absent means all tools.
    #[serde(default)]
    pub tool: Option<String>,
    /// Shell command to run; all `GAIT_*` environment variables are set before execution.
    pub run: String,
}

// ── Sandbox ────────────────────────────────────────────────────────────────
// `NetworkConfig` deliberately uses a custom Deserialize (NOT `#[serde(untagged)]`)
// so bare strings get exact, non-silent handling: "*" is All, "off" is Off, and
// anything else (e.g. a bare hostname, or a typo like "all") is a hard parse
// error rather than silently becoming a one-host allowlist. Multi-host allowlists
// must use the JSON array form.

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
                write!(f, "false, \"off\", \"*\", or an array of hostnames")
            }
            fn visit_bool<E: de::Error>(self, v: bool) -> Result<NetworkConfig, E> {
                if !v {
                    Ok(NetworkConfig::Off)
                } else {
                    Err(E::invalid_value(Unexpected::Bool(v), &self))
                }
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<NetworkConfig, E> {
                match v {
                    "*" => Ok(NetworkConfig::All),
                    "off" => Ok(NetworkConfig::Off),
                    other => Err(E::invalid_value(Unexpected::Str(other), &self)),
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

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
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
    /// Network policy applied to sandboxed processes: `false`/`off` disables, `*` allows all, or an array of hostnames.
    #[serde(default)]
    #[schemars(with = "serde_json::Value")]
    pub network: NetworkConfig,
    /// Shell-only command patterns pre-authorized for extra sandbox scope.
    /// Grants may also include `unsandbox` to run the matching hand without its
    /// kernel seal; the brain remains sandboxed.
    /// Only `shell` tool calls are ever checked against this list — see
    /// `whitelist_match` in gait-state::permissions::matching.
    #[serde(default)]
    pub shell_whitelist: Vec<SandboxWhitelistEntry>,
    /// Per-tool overrides of the built-in sandbox floor (see
    /// `gait_sandbox::ceiling::access_for_tool` for the defaults each tool
    /// gets when not listed here). Unset fields on an entry keep the
    /// built-in default for that dimension.
    #[serde(default)]
    pub tools: std::collections::HashMap<String, ToolAccessOverride>,
    /// Deny patterns layered inside `readable`/`writable`. Present in a user's
    /// `gait.jsonc` (scaffolded by `gait init`), authoritative once there — see
    /// spec §3. Missing sub-fields (not present at all, distinct from an empty
    /// array) fall back to `default_deny_patterns()` via the function-based
    /// serde default above.
    #[serde(default)]
    pub deny: DenyConfig,
}

/// Enforcement: read via `gait_core::turn::hand::classify_proactive_denial` —
/// an app-level check-before-serve gate for the `read`/`write`/`apply_patch`
/// tools (path/glob match in plain Rust, run before any disk I/O). There is
/// NO kernel-level (Seatbelt/Landlock) enforcement of this list — an SBPL
/// codegen attempt was built and reverted after proving unreliable on real
/// hardware (see git history / `docs/superpowers/plans/2026-07-25-sandbox-
/// file-deny-policy-plan.md`'s notes: an undocumented macOS sandbox-compiler
/// quirk silently drops trailing `deny` rules in common rule shapes). The
/// `shell` tool is also NOT covered — it can touch arbitrary paths gait can't
/// enumerate without parsing shell syntax. Populating this list is real and
/// effective for `read`/`write`/`apply_patch`; don't assume it's a hard
/// kernel-level boundary or that it covers `shell`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DenyConfig {
    /// Patterns denied within the read scope. This list IS the effective list —
    /// no hidden merge with built-in defaults once present in a user's config.
    #[serde(default = "crate::config::defaults::default_deny_patterns")]
    pub read: Vec<String>,
    /// Patterns denied within the write scope.
    #[serde(default = "crate::config::defaults::default_deny_patterns")]
    pub write: Vec<String>,
}

impl Default for DenyConfig {
    fn default() -> Self {
        Self {
            read: crate::config::defaults::default_deny_patterns(),
            write: crate::config::defaults::default_deny_patterns(),
        }
    }
}

/// A shell command pattern and the extra sandbox scope it grants when matched.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct SandboxWhitelistEntry {
    /// Shell command pattern to match.
    pub pattern: String,
    /// Extra scope granted by a match, e.g. `network`; `unsandbox` runs the matching hand without its kernel seal.
    #[serde(default)]
    pub grants: Vec<String>,
}

/// Per-tool overrides for the built-in sandbox access floor.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ToolAccessOverride {
    /// Override whether this tool may read when set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reads: Option<bool>,
    /// Override whether this tool may write when set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub writes: Option<bool>,
    /// Override whether this tool may use the network when set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<bool>,
    /// Override whether this tool may use local bind when set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_bind: Option<bool>,
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
            shell_whitelist: vec![],
            tools: std::collections::HashMap::new(),
            deny: DenyConfig::default(),
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strict_tools: Option<bool>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strict_tools: Option<bool>,
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

/// Settings for Gait's in-process GGUF provider. The path is deliberately an
/// authored inference setting rather than an environment variable: it becomes
/// part of the selected inference snapshot and therefore of the session log.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LocalLlmParamsConfig {
    pub model_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gpu_layers: Option<u32>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local: Option<LocalLlmParamsConfig>,
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
            local: agent.local.or(self.local),
        }
    }
}

/// When to render edit/write previews. `on-ask` (default): only at the permission
/// prompt. `always`: also stream the diff live (ephemeral) for auto/allow-listed
/// tools. `off`: never.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum PreviewPolicy {
    #[default]
    OnAsk,
    Always,
    Off,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct GaitConfig {
    /// The selected model, e.g. "anthropic/claude-sonnet-4-6".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inference: Option<String>,
    /// Tool-name / pattern allowlist.
    #[serde(default)]
    pub allow: Vec<String>,
    /// Tool-name / pattern denylist.
    #[serde(default)]
    pub deny: Vec<String>,
    /// Config-defined shell-command tools.
    #[serde(default)]
    pub tools: Vec<ConfigTool>,
    /// Lifecycle hook definitions (event, tool filter, shell command).
    #[serde(default)]
    pub hooks: Vec<HookDef>,
    /// Max retries when an LLM result fails output_schema validation.
    #[serde(default = "default_output_schema_retries")]
    pub output_schema_retries: u8,
    /// Max turn iterations before giving up. Defaults to 100 when absent.
    #[serde(default)]
    pub max_iter: Option<usize>,
    /// Wall-clock timeout (seconds) for a foreground `shell` command. On expiry
    /// the command's process subtree is SIGKILLed and the tool returns exit 124
    /// with a nudge to use `detached:true`. User-set only; the agent never picks it.
    #[serde(default = "default_shell_timeout_secs")]
    pub shell_timeout_secs: u64,
    /// Wall-clock timeout (seconds) for a single ripgrep subprocess invocation
    /// backed by grep/codesearch/ls. On expiry the tool returns an error result
    /// instead of blocking the turn on a hung `rg` process — the same
    /// unbounded-subprocess risk `shell_timeout_secs` guards against, so it
    /// shares that default. NOT sized to preempt the spine's watchdog: the
    /// watchdog's phase-duration cap is already suppressed while tools are
    /// running (see `SpinePhase::HandsPhase` in `watchdog_breach`,
    /// `turn/spine.rs`) and its only live check during a tool call is brain
    /// ping-liveness, which is unaffected by how long a tool takes. User-set
    /// only; the agent never picks it.
    #[serde(default = "default_tool_timeout_secs")]
    pub tool_timeout_secs: u64,
    /// When to render edit/write previews. Defaults to on-ask.
    #[serde(default)]
    pub preview: PreviewPolicy,

    /// JSON Schema that `result` in `structured_done` must conform to.
    /// If set, harness validates and retries on mismatch (up to output_schema_retries).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_schema: Option<serde_json::Value>,

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

fn default_output_schema_retries() -> u8 {
    defaults::default_output_schema_retries()
}

fn default_shell_timeout_secs() -> u64 {
    300
}

fn default_tool_timeout_secs() -> u64 {
    300
}

impl Default for GaitConfig {
    fn default() -> Self {
        Self {
            inference: None,
            allow: defaults::default_allow(),
            deny: defaults::default_deny(),
            tools: vec![],
            hooks: vec![],
            output_schema_retries: defaults::default_output_schema_retries(),
            max_iter: defaults::default_max_iter(),
            shell_timeout_secs: 300,
            tool_timeout_secs: 300,
            preview: defaults::default_preview(),
            result_schema: None,
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

/// How `load_config_with` reacts to a *present-but-invalid* `gait.jsonc`.
///
/// A missing file is not an error in either policy (the library must not
/// require the config to exist). The split exists because silently defaulting
/// at CLI/REPL entry points was the original footgun: one stray character and
/// gait would run on full defaults (different model, default allow/deny) with
/// no signal, then a later `append_*` could persist on top of the misparse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigLoadPolicy {
    /// Surface parse errors to the caller. Use this at CLI/REPL entry points.
    Strict,
    /// Silently fall back to defaults on parse errors. Use this for the ~20
    /// deep runtime reload sites (tools, spine mid-turn) where surfacing a
    /// parse error mid-tool-call would be worse than the silent default.
    Lenient,
}

#[derive(Debug, thiserror::Error, coded_error::CodedError)]
#[coded(namespace = "gait.config")]
pub enum ConfigError {
    #[error("{0}")]
    #[coded(code = "invalid", class = Logical)]
    Invalid(String),
}

fn invalid(message: impl Into<String>) -> anyhow::Error {
    coded_error::into_anyhow(ConfigError::Invalid(message.into()))
}

pub fn load_config_with(gait_dir: &Path, policy: ConfigLoadPolicy) -> anyhow::Result<GaitConfig> {
    let path = gait_dir.join("gait.jsonc");
    if !path.exists() {
        return Ok(GaitConfig::default());
    }
    let raw = std::fs::read_to_string(&path).unwrap_or_default();
    let stripped = strip_jsonc_comments(&raw);
    match serde_json::from_str::<GaitConfig>(&stripped) {
        Ok(config) => Ok(config),
        Err(e) => match policy {
            ConfigLoadPolicy::Strict => Err(invalid(format!(
                "gait.jsonc parse error in {}: {e}",
                path.display()
            ))),
            ConfigLoadPolicy::Lenient => Ok(GaitConfig::default()),
        },
    }
}

/// Infallible, lenient config load — preserves the historical signature so the
/// ~20 deep mid-turn callers (tools, spine, exec_prompt, spawn_agent, etc.) do
/// not need to handle parse errors. New CLI/REPL entry points should use
/// `load_config_with(..., ConfigLoadPolicy::Strict)` instead.
pub fn load_config(gait_dir: &Path) -> GaitConfig {
    // Lenient + unwrap_or_default matches the historical `unwrap_or_default()`
    // on both the file read and the parse. The `Err` branch of
    // `load_config_with(_, Lenient)` is unreachable — Lenient always returns
    // `Ok(default)` on parse failure and `Ok(default)` on missing file —
    // but we keep `unwrap_or_default` so a future invariant violation still
    // degrades to defaults rather than panicking the whole process.
    load_config_with(gait_dir, ConfigLoadPolicy::Lenient).unwrap_or_default()
}
fn append_list_field(field: &str, pattern: &str, path: &Path) -> anyhow::Result<()> {
    let mut obj: serde_json::Map<String, serde_json::Value> = if path.exists() {
        let raw = std::fs::read_to_string(path)?;
        let stripped = strip_jsonc_comments(&raw);
        match serde_json::from_str::<serde_json::Value>(&stripped) {
            Ok(serde_json::Value::Object(m)) => m,
            Ok(_) => return Err(invalid("gait.jsonc root is not a JSON object")),
            Err(e) => return Err(invalid(format!("gait.jsonc parse error: {e}"))),
        }
    } else {
        serde_json::Map::new()
    };
    let list = obj
        .entry(field)
        .or_insert_with(|| serde_json::Value::Array(vec![]))
        .as_array_mut()
        .ok_or_else(|| invalid(format!("{field} field is not an array")))?;
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

/// Append a value to a list nested at `obj["sandbox"][field]`, creating the
/// sandbox block and/or array if absent. Strips JSONC comments (consistent
/// with pre-existing behavior of `append_list_field`).
fn append_sandbox_field(field: &str, value: &str, path: &Path) -> anyhow::Result<()> {
    let mut obj: serde_json::Map<String, serde_json::Value> = if path.exists() {
        let raw = std::fs::read_to_string(path)?;
        let stripped = strip_jsonc_comments(&raw);
        match serde_json::from_str::<serde_json::Value>(&stripped) {
            Ok(serde_json::Value::Object(m)) => m,
            Ok(_) => return Err(invalid("gait.jsonc root is not a JSON object")),
            Err(e) => return Err(invalid(format!("gait.jsonc parse error: {e}"))),
        }
    } else {
        serde_json::Map::new()
    };

    // Navigate/create sandbox block
    let sandbox = obj
        .entry("sandbox")
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()))
        .as_object_mut()
        .ok_or_else(|| invalid("gait.jsonc 'sandbox' is not an object"))?;

    let list = sandbox
        .entry(field)
        .or_insert_with(|| serde_json::Value::Array(vec![]))
        .as_array_mut()
        .ok_or_else(|| invalid(format!("sandbox.{field} is not an array")))?;

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
            Ok(_) => return Err(invalid("gait.jsonc root is not a JSON object")),
            Err(e) => return Err(invalid(format!("gait.jsonc parse error: {e}"))),
        }
    } else {
        serde_json::Map::new()
    };

    let sandbox = obj
        .entry("sandbox")
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()))
        .as_object_mut()
        .ok_or_else(|| invalid("gait.jsonc 'sandbox' is not an object"))?;

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

#[cfg(test)]
mod schema_tests {
    use super::GaitConfig;

    #[test]
    fn nested_config_schema_documents_every_sandbox_field() {
        let schema = schemars::schema_for!(GaitConfig);

        for type_name in [
            "SandboxConfig",
            "DenyConfig",
            "ConfigTool",
            "HookDef",
            "SandboxWhitelistEntry",
            "ToolAccessOverride",
        ] {
            let definition = schema
                .definitions
                .get(type_name)
                .unwrap_or_else(|| panic!("missing schema definition for {type_name}"));
            let definition = serde_json::to_value(definition).expect("schema definition JSON");
            let properties = definition
                .get("properties")
                .and_then(serde_json::Value::as_object)
                .unwrap_or_else(|| panic!("missing properties for {type_name}"));
            for (field_name, field) in properties {
                assert!(
                    field
                        .get("description")
                        .and_then(serde_json::Value::as_str)
                        .is_some(),
                    "missing description for {type_name}.{field_name}"
                );
            }
        }
    }
}

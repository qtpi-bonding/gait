use super::*;
use tempfile::TempDir;

#[test]
fn sandbox_config_defaults_to_strict_on() {
    let c = GaitConfig::default();
    assert!(c.sandbox.on);
    assert!(c.sandbox.readable.is_empty());
    assert!(matches!(c.sandbox.network, NetworkConfig::Off));
}

#[test]
fn sandbox_config_parses_block() {
    let json = r##"{"model":"x/y","sandbox":{"readable":["~/.cargo"],"network":"*"}}"##;
    let c: GaitConfig = serde_json::from_str(json).unwrap();
    assert_eq!(c.sandbox.readable, vec!["~/.cargo"]);
    assert!(matches!(c.sandbox.network, NetworkConfig::All));
}

#[test]
fn sandbox_network_config_parses_false_as_off() {
    let json = r##"{"model":"x/y","sandbox":{"network":false}}"##;
    let c: GaitConfig = serde_json::from_str(json).unwrap();
    assert!(matches!(c.sandbox.network, NetworkConfig::Off));
}

#[test]
fn sandbox_network_config_parses_string_host_as_hosts() {
    let json = r##"{"model":"x/y","sandbox":{"network":"github.com"}}"##;
    let c: GaitConfig = serde_json::from_str(json).unwrap();
    assert!(matches!(c.sandbox.network, NetworkConfig::Hosts(ref h) if h == &["github.com"]));
}

#[test]
fn loads_default_when_no_file() {
    let dir = TempDir::new().unwrap();
    let cfg = load_config(dir.path());
    assert_eq!(cfg.model, None);
}

#[test]
fn parses_model_and_allow() {
    let dir = TempDir::new().unwrap();
    std::fs::write(
        dir.path().join("gait.jsonc"),
        r#"
{
  "model": "minimax/MiniMax-M3",
  "allow": ["read", "write", "shell"]
}
"#,
    )
    .unwrap();
    let cfg = load_config(dir.path());
    assert_eq!(cfg.model.as_deref(), Some("minimax/MiniMax-M3"));
    assert_eq!(cfg.allow, vec!["read", "write", "shell"]);
}

#[test]
fn strips_line_comments() {
    let dir = TempDir::new().unwrap();
    std::fs::write(
        dir.path().join("gait.jsonc"),
        r#"
{
  // this is the model
  "model": "anthropic/claude-sonnet-4-6",
  "allow": ["read"] // read is safe
}
"#,
    )
    .unwrap();
    let cfg = load_config(dir.path());
    assert_eq!(cfg.model.as_deref(), Some("anthropic/claude-sonnet-4-6"));
}

#[test]
fn parses_config_defined_tools() {
    let dir = TempDir::new().unwrap();
    std::fs::write(
        dir.path().join("gait.jsonc"),
        r#"
{
  "model": "anthropic/claude-sonnet-4-6",
  "allow": [],
  "tools": [
    { "name": "run_tests", "description": "Run tests", "cmd": "cargo test", "cwd": "codebase" },
    { "name": "lint", "description": "Run linter", "cmd": "cargo clippy" }
  ]
}
"#,
    )
    .unwrap();
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
    std::fs::write(
        dir.path().join("gait.jsonc"),
        r#"{"model":"minimax/MiniMax-M3","allow":["read"]}"#,
    )
    .unwrap();
    append_allow_pattern(dir.path(), "shell:cargo *").unwrap();
    let cfg = load_config(dir.path());
    assert!(cfg.allow.contains(&"shell:cargo *".to_string()));
}

#[test]
fn append_allow_pattern_preserves_model_field() {
    let dir = TempDir::new().unwrap();
    std::fs::write(
        dir.path().join("gait.jsonc"),
        r#"{"model":"minimax/MiniMax-M3","allow":[]}"#,
    )
    .unwrap();
    append_allow_pattern(dir.path(), "shell:git *").unwrap();
    assert_eq!(
        load_config(dir.path()).model.as_deref(),
        Some("minimax/MiniMax-M3")
    );
}

#[test]
fn append_allow_pattern_creates_file_when_missing() {
    let dir = TempDir::new().unwrap();
    append_allow_pattern(dir.path(), "shell:cargo *").unwrap();
    assert!(load_config(dir.path())
        .allow
        .contains(&"shell:cargo *".to_string()));
}

#[test]
fn append_allow_pattern_deduplicates() {
    let dir = TempDir::new().unwrap();
    std::fs::write(
        dir.path().join("gait.jsonc"),
        r#"{"model":"minimax/MiniMax-M3","allow":["shell:cargo *"]}"#,
    )
    .unwrap();
    append_allow_pattern(dir.path(), "shell:cargo *").unwrap();
    assert_eq!(
        load_config(dir.path())
            .allow
            .iter()
            .filter(|p| p.as_str() == "shell:cargo *")
            .count(),
        1
    );
}

#[test]
fn append_allow_pattern_bails_on_parse_error() {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("gait.jsonc"), "{ invalid json").unwrap();
    assert!(append_allow_pattern(dir.path(), "shell:cargo *").is_err());
    // Must NOT overwrite the broken file
    assert!(std::fs::read_to_string(dir.path().join("gait.jsonc"))
        .unwrap()
        .contains("invalid json"));
}

#[test]
fn parses_mcp_servers_spawn_and_connect() {
    let dir = TempDir::new().unwrap();
    std::fs::write(
        dir.path().join("gait.jsonc"),
        r#"
{
  "model": "anthropic/claude-sonnet-4-6",
  "mcp_servers": [
    { "name": "fs", "spawn": "npx @modelcontextprotocol/server-filesystem ." },
    { "name": "remote", "connect": "http://localhost:3001/mcp" }
  ]
}
"#,
    )
    .unwrap();
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
fn append_allow_creates_file_without_model_field() {
    let dir = TempDir::new().unwrap();
    // No gait.jsonc exists yet — append_allow_pattern must create one without a model key
    // (model has no compiled-in default; it must be set explicitly in gait.jsonc or via --model)
    append_allow_pattern(dir.path(), "read").unwrap();
    let content = std::fs::read_to_string(dir.path().join("gait.jsonc")).unwrap();
    let val: serde_json::Value = serde_json::from_str(&content).unwrap();
    assert!(
        val.get("model").is_none() || val["model"].is_null(),
        "newly created gait.jsonc must NOT hardcode a default model"
    );
}

#[test]
fn enabled_defaults_to_true_for_backward_compat() {
    let json = r##"{ "model": "anthropic/claude-sonnet-4-6",
        "mcp_servers": [{ "name": "slack", "spawn": "node mcp-slack.js" }],
        "skills": [{ "name": "tdd", "description": "tdd workflow", "content": "# TDD" }]
    }"##;
    let cfg: GaitConfig = serde_json::from_str(json).unwrap();
    assert!(
        cfg.mcp_servers[0].enabled,
        "mcp without enabled field should default true"
    );
    assert!(
        cfg.skills[0].enabled,
        "skill without enabled field should default true"
    );
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
        "{{
  \"model\": \"minimax/MiniMax-M3\",
  \"mcp_servers\": [
    {{ \"name\": \"slack\", \"spawn\": \"node mcp-slack.js\" }}
  ],
  \"skills\": [
    {{ \"name\": \"tdd\", \"description\": \"test-driven development workflow\", \"content\": {content_json} }}
  ]
}}
"
    );
    std::fs::write(dir.path().join("gait.jsonc"), jsonc).unwrap();
    let cfg = load_config(dir.path());
    assert_eq!(cfg.mcp_servers.len(), 1);
    assert_eq!(cfg.mcp_servers[0].name, "slack");
    assert_eq!(
        cfg.mcp_servers[0].spawn.as_deref(),
        Some("node mcp-slack.js")
    );
    assert_eq!(cfg.skills.len(), 1);
}

#[test]
fn parses_deny_list() {
    let dir = TempDir::new().unwrap();
    std::fs::write(
        dir.path().join("gait.jsonc"),
        r#"
{
  "model": "minimax/MiniMax-M3",
  "allow": ["read"],
  "deny": ["shell:rm *", "shell:git push *"]
}
"#,
    )
    .unwrap();
    let cfg = load_config(dir.path());
    assert_eq!(cfg.deny, vec!["shell:rm *", "shell:git push *"]);
}

#[test]
fn deny_defaults_to_empty() {
    let dir = TempDir::new().unwrap();
    std::fs::write(
        dir.path().join("gait.jsonc"),
        r#"{"model":"minimax/MiniMax-M3","allow":[]}"#,
    )
    .unwrap();
    let cfg = load_config(dir.path());
    assert!(cfg.deny.is_empty());
}

#[test]
fn append_deny_pattern_adds_to_deny() {
    let dir = TempDir::new().unwrap();
    std::fs::write(
        dir.path().join("gait.jsonc"),
        r#"{"model":"minimax/MiniMax-M3","allow":[],"deny":[]}"#,
    )
    .unwrap();
    append_deny_pattern(dir.path(), "shell:rm *").unwrap();
    let cfg = load_config(dir.path());
    assert!(cfg.deny.contains(&"shell:rm *".to_string()));
}

#[test]
fn append_deny_pattern_deduplicates() {
    let dir = TempDir::new().unwrap();
    std::fs::write(
        dir.path().join("gait.jsonc"),
        r#"{"model":"minimax/MiniMax-M3","allow":[],"deny":["shell:rm *"]}"#,
    )
    .unwrap();
    append_deny_pattern(dir.path(), "shell:rm *").unwrap();
    let cfg = load_config(dir.path());
    assert_eq!(
        cfg.deny
            .iter()
            .filter(|p| p.as_str() == "shell:rm *")
            .count(),
        1
    );
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

#[test]
fn llm_params_defaults_are_none() {
    use crate::config::LlmParamsConfig;
    let p = LlmParamsConfig::default();
    assert!(p.max_tokens.is_none());
    assert!(p.temperature.is_none());
    assert!(p.top_p.is_none());
    // show_thinking is NOT on LlmParamsConfig — it lives on GaitConfig
    assert!(p.anthropic.is_none());
    assert!(p.openai.is_none());
    assert!(p.gemini.is_none());
}

#[test]
fn llm_params_deserializes_from_json() {
    use crate::config::LlmParamsConfig;
    let json = r#"{
        "max_tokens": 512,
        "temperature": 0.5,
        "anthropic": {
            "effort": "high",
            "top_k": 5,
            "thinking": {
                "type": "enabled",
                "budget_tokens": 1000,
                "display": "summarized"
            }
        }
    }"#;
    let p: LlmParamsConfig = serde_json::from_str(json).expect("deser");
    assert_eq!(p.max_tokens, Some(512));
    assert_eq!(p.temperature, Some(0.5));
    let a = p.anthropic.as_ref().expect("anthropic");
    assert_eq!(a.effort, Some("high".into()));
    assert_eq!(a.top_k, Some(5));
    let t = a.thinking.as_ref().expect("thinking");
    assert_eq!(t.kind, Some("enabled".into()));
    assert_eq!(t.budget_tokens, Some(1000));
    assert_eq!(t.display, Some("summarized".into()));
}

#[test]
fn gait_config_show_thinking_defaults_false() {
    use crate::config::GaitConfig;
    let c = GaitConfig::default();
    assert!(!c.show_thinking, "show_thinking should default to false");
}

#[test]
fn gait_config_show_thinking_deserializes() {
    use crate::config::{strip_jsonc_comments, GaitConfig};
    let jsonc = r#"{ "model": "anthropic/claude-opus-4-8", "show_thinking": true }"#;
    let config: GaitConfig = serde_json::from_str(&strip_jsonc_comments(jsonc)).expect("deser");
    assert!(config.show_thinking);
}

#[test]
fn llm_params_openai_deserializes() {
    use crate::config::LlmParamsConfig;
    let json = r#"{
        "openai": {
            "reasoning_effort": "high",
            "frequency_penalty": 0.1,
            "seed": 42,
            "stop": ["<|end|>"]
        }
    }"#;
    let p: LlmParamsConfig = serde_json::from_str(json).expect("deser");
    let o = p.openai.as_ref().expect("openai");
    assert_eq!(o.reasoning_effort, Some("high".into()));
    assert_eq!(o.frequency_penalty, Some(0.1));
    assert_eq!(o.seed, Some(42));
    assert_eq!(o.stop, vec!["<|end|>".to_string()]);
}

#[test]
fn llm_params_merged_with_agent_wins() {
    use crate::config::{AnthropicLlmParamsConfig, LlmParamsConfig};
    let config_params = LlmParamsConfig {
        max_tokens: Some(1024),
        temperature: Some(0.3),
        top_p: None,
        // show_thinking is NOT on LlmParamsConfig
        anthropic: Some(AnthropicLlmParamsConfig {
            effort: Some("low".into()),
            top_k: None,
            thinking: None,
            strict_tools: None,
        }),
        openai: None,
        gemini: None,
    };
    let agent_params = LlmParamsConfig {
        max_tokens: Some(2048),
        temperature: None, // not set in agent → config wins
        top_p: Some(0.9),
        anthropic: None, // agent doesn't set → config's anthropic survives
        openai: None,
        gemini: None,
    };
    let merged = config_params.merged_with(agent_params);
    assert_eq!(merged.max_tokens, Some(2048)); // agent wins
    assert_eq!(merged.temperature, Some(0.3)); // config fallback
    assert_eq!(merged.top_p, Some(0.9)); // agent wins
    assert!(merged.anthropic.is_some()); // config fallback
    assert_eq!(
        merged.anthropic.as_ref().unwrap().effort,
        Some("low".into())
    );
}

#[test]
fn gait_config_default_has_empty_llm_params() {
    use crate::config::GaitConfig;
    let c = GaitConfig::default();
    assert!(c.llm_params.max_tokens.is_none());
}

#[test]
fn gait_config_deserializes_llm_params() {
    use crate::config::{strip_jsonc_comments, GaitConfig};
    let jsonc = r#"{
        "model": "anthropic/claude-opus-4-8",
        "llm_params": {
            "max_tokens": 4096,
            "temperature": 0.7
        }
    }"#;
    let config: GaitConfig = serde_json::from_str(&strip_jsonc_comments(jsonc)).expect("deser");
    assert_eq!(config.llm_params.max_tokens, Some(4096));
    assert_eq!(config.llm_params.temperature, Some(0.7));
}

#[test]
fn append_sandbox_writable_creates_nested_entry() {
    let dir = TempDir::new().unwrap();
    std::fs::write(
        dir.path().join("gait.jsonc"),
        r#"{"model": "test/model", "sandbox": {"on": true, "writable": []}}"#,
    )
    .unwrap();

    super::append_sandbox_writable(dir.path(), "/some/external/path").unwrap();

    let raw = std::fs::read_to_string(dir.path().join("gait.jsonc")).unwrap();
    let val: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let writable = val["sandbox"]["writable"].as_array().unwrap();
    assert_eq!(writable.len(), 1);
    assert_eq!(writable[0].as_str().unwrap(), "/some/external/path");
}

#[test]
fn append_sandbox_writable_idempotent() {
    let dir = TempDir::new().unwrap();
    std::fs::write(
        dir.path().join("gait.jsonc"),
        r#"{"model": "test/model", "sandbox": {"on": true, "writable": ["/already/here"]}}"#,
    )
    .unwrap();

    super::append_sandbox_writable(dir.path(), "/already/here").unwrap();

    let raw = std::fs::read_to_string(dir.path().join("gait.jsonc")).unwrap();
    let val: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let writable = val["sandbox"]["writable"].as_array().unwrap();
    assert_eq!(writable.len(), 1); // no duplicate
}

#[test]
fn append_sandbox_writable_creates_sandbox_block_if_absent() {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("gait.jsonc"), r#"{"model": "test/model"}"#).unwrap();

    super::append_sandbox_writable(dir.path(), "/new/path").unwrap();

    let raw = std::fs::read_to_string(dir.path().join("gait.jsonc")).unwrap();
    let val: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let writable = val["sandbox"]["writable"].as_array().unwrap();
    assert_eq!(writable[0].as_str().unwrap(), "/new/path");
}

#[test]
fn append_sandbox_readable_creates_nested_entry() {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("gait.jsonc"), r#"{"model":"m"}"#).unwrap();
    super::append_sandbox_readable(dir.path(), "/read/path").unwrap();
    let raw = std::fs::read_to_string(dir.path().join("gait.jsonc")).unwrap();
    let val: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(
        val["sandbox"]["readable"][0].as_str().unwrap(),
        "/read/path"
    );
}

#[test]
fn append_sandbox_network_host_creates_nested_entry() {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("gait.jsonc"), r#"{"model":"m"}"#).unwrap();
    super::append_sandbox_network_host(dir.path(), "api.example.com").unwrap();
    let raw = std::fs::read_to_string(dir.path().join("gait.jsonc")).unwrap();
    let val: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let hosts = val["sandbox"]["network"].as_array().unwrap();
    assert_eq!(hosts[0].as_str().unwrap(), "api.example.com");
}

#[test]
fn strict_tools_round_trips_and_defaults_none() {
    use crate::config::OpenAILlmParamsConfig;
    let c: OpenAILlmParamsConfig = serde_json::from_str("{}").unwrap();
    assert_eq!(c.strict_tools, None);
    let c: OpenAILlmParamsConfig = serde_json::from_str(r#"{"strict_tools": true}"#).unwrap();
    assert_eq!(c.strict_tools, Some(true));
}

#[test]
fn shell_timeout_defaults_to_300() {
    let cfg: GaitConfig = serde_json::from_str("{}").unwrap();
    assert_eq!(cfg.shell_timeout_secs, 300);
}

#[test]
fn shell_timeout_parses_override() {
    let cfg: GaitConfig = serde_json::from_str(r#"{"shell_timeout_secs": 60}"#).unwrap();
    assert_eq!(cfg.shell_timeout_secs, 60);
}

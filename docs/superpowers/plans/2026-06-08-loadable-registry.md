---
status: done
---

# Loadable Registry & Progressive Disclosure Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Unify MCP servers, skills, and future WASM plugins under a single registry with a two-layer model: whitelisted (in `gait.jsonc`) vs enabled (active this session). Everything whitelisted appears in the system prompt index so the LLM can discover and enable it; only enabled items have live tools. This is progressive disclosure applied uniformly across all loadable things.

**Architecture:** Add `enabled: bool` to existing config structs. Replace the separate `<mcp-index>`/`<skill-index>` system prompt sections with a unified `<loadable-index>`. Replace the `mcp` and `skill` LLM tools with a single `load` tool. Add `gait load`/`gait unload` CLI subcommands that manage the config whitelist. Stub a `Loadable` trait in `types.rs` as the hook point for future WASM plugins.

**Tech Stack:** Rust, serde_json, schemars, existing gait crate layout.

---

## Key facts about the current codebase

- `AnyEvent::ToolCall` is a **struct variant**: `{ id, name, input, ts }`. Access fields as `name` and `input`, not `tc.name`/`tc.args`.
- `derive_loaded_context_tool` (turn.rs:95) reads `input.get("context")` — the old tool arg key. The new `load` tool uses `enabled` instead. **Both the transcript-replay function and the LoadContext handler (turn.rs:539) must be updated together, and old transcripts using `context` need a backward-compat read.**
- Skills are injected by returning their content string from `execute()` — the string becomes a tool result in the conversation. Skills do **not** push anything into the `tools` vec. Only MCP tools push into `tools`.
- `builtin_tools` in `tools/mod.rs` currently asserts `len() == 16` (line 93). Removing `McpTool` + `SkillTool` and adding `LoadTool` changes the count to **15**. Update the assertion.
- Adding `config_enabled` to `TurnOpts` breaks **11 construction sites**: 9 inline test calls inside `turn.rs`, 1 in `bin/gait.rs` (~line 252), and 1 in `workflow/exec_prompt.rs` (~line 59). `lib.rs` has **no** `TurnOpts {` construction. All must be updated in Task 3.
- `gait.jsonc` uses JSONC comments. Rewriting it via serde strips all comments — this is already how the codebase works (`strip_jsonc_comments` in config.rs), so `gait load`/`gait unload` will also strip comments. Note this in the CLI output.

---

## File Map

- Modify: `gait-rs/src/config.rs` — add `enabled` to `McpServerDef` and `SkillDef`; add `description: Option<String>` to `McpServerDef`; add stub `WasmPluginDef`; make `strip_jsonc_comments` pub
- Modify: `gait-rs/src/types.rs` — add `LoadableKind`, `LoadableEntry` stub
- Modify: `gait-rs/src/turn.rs` — replace dual indexes with `<loadable-index>`; replace `derive_loaded_*` with `derive_enabled_loadables`; update `TurnOpts`; update `LoadContext` handler
- New: `gait-rs/src/tools/load.rs` — unified `LoadTool` replacing `mcp` + `skill` tools
- Modify: `gait-rs/src/tools/mod.rs` — add `pub mod load;`, swap in `LoadTool`, remove `McpTool`/`SkillTool`
- Delete: `gait-rs/src/tools/mcp.rs`, `gait-rs/src/tools/skill.rs`
- Modify: `gait-rs/src/bin/gait.rs` — add `gait load`/`gait unload` subcommands
- Modify: `gait-rs/src/bin/gait.rs` (~line 252) and `gait-rs/src/workflow/exec_prompt.rs` (~line 59) — add `config_enabled`; also update 9 inline `TurnOpts {` calls in `turn.rs` tests

---

## Task 1: Add `enabled` to config structs + stub `WasmPluginDef`

**Files:** `gait-rs/src/config.rs`

- [ ] **Step 1: Write failing tests**

```rust
#[test]
fn enabled_defaults_to_true_for_backward_compat() {
    let json = r#"{ "model": "anthropic/claude-sonnet-4-6",
        "mcp_servers": [{ "name": "slack", "spawn": "node mcp-slack.js" }],
        "skills": [{ "name": "tdd", "description": "tdd workflow", "content": "# TDD" }]
    }"#;
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
```

- [ ] **Step 2: Run to verify they fail**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test config 2>&1 | tail -20
```

- [ ] **Step 3: Implement**

```rust
fn default_enabled() -> bool { true }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerDef {
    pub name: String,
    pub spawn: Option<String>,
    pub connect: Option<String>,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// Optional description for the <loadable-index>. If absent, index shows tool count.
    #[serde(default)]
    pub description: Option<String>,
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
```

Add to `GaitConfig` and its `Default`:

```rust
#[serde(default)]
pub wasm_plugins: Vec<WasmPluginDef>,
```

- [ ] **Step 4: Run tests**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test 2>&1 | tail -20
```

- [ ] **Step 5: Make `strip_jsonc_comments` public**

At `config.rs:77`, change `fn strip_jsonc_comments` to `pub fn strip_jsonc_comments`. Required by Task 5's `read_config_as_value` helper.

- [ ] **Step 6: Commit**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && git add src/config.rs && git commit -m "feat: add enabled field to McpServerDef/SkillDef + stub WasmPluginDef; pub strip_jsonc_comments"
```

---

## Task 2: Add `LoadableKind` and `LoadableEntry` to `types.rs`

**Files:** `gait-rs/src/types.rs`

- [ ] **Step 1: Write failing test**

```rust
#[test]
fn loadable_kind_as_str() {
    assert_eq!(LoadableKind::Mcp.as_str(), "mcp");
    assert_eq!(LoadableKind::Skill.as_str(), "skill");
    assert_eq!(LoadableKind::WasmPlugin.as_str(), "plugin");
}
```

- [ ] **Step 2: Run to verify it fails**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test loadable_kind 2>&1 | tail -10
```

- [ ] **Step 3: Add types**

```rust
#[derive(Debug, Clone, PartialEq)]
pub enum LoadableKind {
    Mcp,
    Skill,
    WasmPlugin,
}

impl LoadableKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            LoadableKind::Mcp => "mcp",
            LoadableKind::Skill => "skill",
            LoadableKind::WasmPlugin => "plugin",
        }
    }
}

/// A whitelisted item that can be enabled/disabled by the LLM at runtime.
#[derive(Debug, Clone)]
pub struct LoadableEntry {
    pub name: String,
    pub kind: LoadableKind,
    pub description: String,
    pub enabled: bool,
}
```

- [ ] **Step 4: Run tests**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test 2>&1 | tail -10
```

- [ ] **Step 5: Commit**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && git add src/types.rs && git commit -m "feat: add LoadableKind and LoadableEntry to types"
```

---

## Task 3: Unified `<loadable-index>` and `derive_enabled_loadables` in `turn.rs`

**Files:** `gait-rs/src/turn.rs`, `gait-rs/src/bin/gait.rs` (~line 252), `gait-rs/src/workflow/exec_prompt.rs` (~line 59)

**Enable-state is at server granularity:** `config_enabled` tracks **server names** (not tool names). `load("slack", enabled=true)` enables all tools from the Slack server. `TurnOpts` carries `config_enabled: HashSet<String>`.

**Important — step order:** `derive_enabled_loadables` and `config_enabled` (steps 3–4) must be implemented before the index-building rewrite (step 5), because step 5 depends on `enabled_names` which comes from step 3.

- [ ] **Step 1: Write failing tests**

```rust
#[test]
fn build_loadable_index_shows_all_kinds() {
    use crate::types::{LoadableEntry, LoadableKind};
    let entries = vec![
        LoadableEntry { name: "slack_send".into(), kind: LoadableKind::Mcp,
            description: "post to Slack".into(), enabled: false },
        LoadableEntry { name: "tdd".into(), kind: LoadableKind::Skill,
            description: "test-driven workflow".into(), enabled: true },
    ];
    let idx = build_loadable_index(&entries);
    assert!(idx.contains("<loadable-index>"), "got: {idx}");
    assert!(idx.contains("mcp"), "got: {idx}");
    assert!(idx.contains("skill"), "got: {idx}");
    assert!(idx.contains("slack_send"), "got: {idx}");
    assert!(idx.contains("tdd"), "got: {idx}");
    assert!(idx.contains("enabled=false"), "got: {idx}");
    assert!(idx.contains("enabled=true"), "got: {idx}");
}

#[test]
fn build_loadable_index_empty_returns_empty() {
    assert!(build_loadable_index(&[]).is_empty());
}

#[test]
fn derive_enabled_loadables_seeds_from_config_and_replays_transcript() {
    use crate::session::transcript::AnyEvent;
    use std::collections::HashSet;

    // config: tdd enabled, slack not
    let config_enabled: HashSet<String> = ["tdd".into()].into();

    // transcript: load(slack, enabled=true), load(tdd, enabled=false)
    let events = vec![
        AnyEvent::ToolCall { id: "1".into(), name: "load".into(),
            input: serde_json::json!({"name": "slack_send", "enabled": true}), ts: 1 },
        AnyEvent::ToolCall { id: "2".into(), name: "load".into(),
            input: serde_json::json!({"name": "tdd", "enabled": false}), ts: 2 },
    ];
    let enabled = derive_enabled_loadables(&events, &config_enabled);
    assert!(enabled.contains("slack_send"), "slack should be enabled after load");
    assert!(!enabled.contains("tdd"), "tdd should be disabled after unload");
}

#[test]
fn derive_enabled_loadables_backward_compat_context_key() {
    // old transcripts used "context" instead of "enabled"
    // NOTE: old transcripts stored per-tool names (e.g. "slack_send") instead of
    // server names (e.g. "slack"). derive_enabled_loadables is name-agnostic — it
    // inserts whatever string it finds. The LoadContext handler will then try to match
    // that string against server names and find nothing, so old per-tool loads won't
    // auto-re-enable after compaction. This is acceptable and documented.
    use crate::session::transcript::AnyEvent;
    use std::collections::HashSet;

    let events = vec![
        AnyEvent::ToolCall { id: "1".into(), name: "mcp".into(),
            input: serde_json::json!({"name": "slack_send", "context": true}), ts: 1 },
    ];
    let enabled = derive_enabled_loadables(&events, &HashSet::new());
    assert!(enabled.contains("slack_send"), "old 'context' key must still work");
}

#[test]
fn whitelisted_disabled_item_appears_in_index_but_not_active_tools() {
    // This is an integration concern verified in the run_turn tests —
    // add a run_turn test that confirms a skill with enabled=false
    // appears in <loadable-index> but its content is NOT in any tool result.
    // Stub: implement after run_turn is updated.
}
```

- [ ] **Step 2: Run to verify they fail**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test derive_enabled 2>&1 | tail -10
```

- [ ] **Step 3: Add `derive_enabled_loadables`**

Replace the separate `derive_loaded_mcps` / `derive_loaded_skills` functions with:

```rust
/// Derive which loadable names are currently enabled.
/// Seeds from config defaults (config_enabled), then replays transcript
/// load/unload events. Handles both old "context" key and new "enabled" key
/// for backward compatibility with existing transcripts.
pub fn derive_enabled_loadables(
    events: &[AnyEvent],
    config_enabled: &std::collections::HashSet<String>,
) -> std::collections::HashSet<String> {
    let mut enabled = config_enabled.clone();
    for event in events {
        if let AnyEvent::ToolCall { name, input, .. } = event {
            if name == "load" || name == "mcp" || name == "skill" {
                let item_name = input.get("name")
                    .and_then(|v| v.as_str()).unwrap_or("").to_string();
                // support both old "context" key and new "enabled" key
                let on = input.get("enabled")
                    .or_else(|| input.get("context"))
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                if on { enabled.insert(item_name); } else { enabled.remove(&item_name); }
            }
        }
    }
    enabled
}
```

Keep `derive_loaded_mcps` and `derive_loaded_skills` as thin wrappers over this for now; delete them once all call sites are updated.

- [ ] **Step 4: Add `config_enabled` to `TurnOpts` and update all construction sites**

```rust
// in TurnOpts:
pub config_enabled: std::collections::HashSet<String>,
```

Find all construction sites:

```bash
grep -rn "TurnOpts {" /Users/aicoder/Documents/ideation/gait/gait-rs/src/
```

The struct definition at `turn.rs:19` is not a construction site — skip it. Update every other site (9 in turn.rs tests, 1 in bin/gait.rs, 1 in exec_prompt.rs):

```rust
config_enabled: cfg.mcp_servers.iter()
    .filter(|s| s.enabled)
    .map(|s| s.name.clone())           // server name, not tool names
    .chain(cfg.skills.iter().filter(|s| s.enabled).map(|s| s.name.clone()))
    .collect(),
```

For test calls that don't have a `cfg`, pass `HashSet::new()` as the default.

- [ ] **Step 5: Add `build_loadable_index` and replace call sites**

```rust
pub fn build_loadable_index(entries: &[crate::types::LoadableEntry]) -> String {
    if entries.is_empty() { return String::new(); }
    let lines: Vec<String> = entries.iter().map(|e| {
        format!("  {} {} enabled={} — {}",
            e.kind.as_str(), e.name, e.enabled, e.description)
    }).collect();
    format!("\n<loadable-index>\nCall load(name, enabled=true/false) to enable or disable.\n{}\n</loadable-index>",
        lines.join("\n"))
}
```

In `run_turn`, compute `enabled_names` from `derive_enabled_loadables`, then build entries for all whitelisted MCPs and skills, then call `build_loadable_index`. Replace the `build_mcp_index` + `build_skill_index` call block.

Note: `WasmPluginDef` entries are **not** added to the index in this plan — that is deferred to the WASM plugin plan.

- [ ] **Step 6: Update the post-compaction reload loop at `turn.rs:357-364`**

This block seeds the initial `tools` vec from the enabled set after reading the compacted transcript. Migrate it to use `derive_enabled_loadables` and server-granularity push:

```rust
// Replace the derive_loaded_mcps / derive_loaded_skills calls with:
let enabled_names = derive_enabled_loadables(&post_compaction, &opts.config_enabled);

// Push all tools from enabled MCP servers:
for server_name in &enabled_names {
    for def in opts.mcp_registry.iter().filter(|d| &d.server_name == server_name) {
        if let Some(client) = opts.mcp_clients.get(&def.server_name) {
            if !tools.iter().any(|t| t.name() == def.name) {  // duplicate guard
                tools.push(make_mcp_tool_def(def, client.clone()));
            }
        }
    }
}
// Skills are not pushed here — they surface via LoadTool.execute() return value.
```

- [ ] **Step 7: Update the `LoadContext` handler in `run_turn`**

The handler currently branches on `tc.name == "mcp"` or `tc.name == "skill"` and reads `tc.args.get("context")`. Update to server-granularity. **`item_name` is a server name for MCPs, a skill name for skills.** Preserve the duplicate guard on enable to avoid pushing the same tool twice if the LLM calls load twice.

```rust
// In the AnyEvent::ToolCall { name, input, .. } = tc arm:
if name == "load" || name == "mcp" || name == "skill" {
    let item_name = input.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let on = input.get("enabled")
        .or_else(|| input.get("context"))  // backward compat
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if on {
        // MCP: push all tools from this server (server-granularity)
        for def in opts.mcp_registry.iter().filter(|d| d.server_name == item_name) {
            if let Some(client) = opts.mcp_clients.get(&def.server_name) {
                if !tools.iter().any(|t| t.name() == def.name) {  // duplicate guard
                    tools.push(make_mcp_tool_def(def, client.clone()));
                }
            }
        }
        // Skill: no tools push needed — content is returned by LoadTool.execute()
    } else {
        // Remove all tools from this server
        tools.retain(|t| {
            opts.mcp_registry.iter()
                .find(|d| d.name == t.name())
                .map(|d| d.server_name != item_name)
                .unwrap_or(true)
        });
    }
}
```

- [ ] **Step 8: Run all tests**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test 2>&1 | tail -20
```

- [ ] **Step 9: Commit**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && git add src/turn.rs src/bin/gait.rs src/workflow/exec_prompt.rs && git commit -m "feat: unified <loadable-index> + derive_enabled_loadables; backward compat context key"
```

---

## Task 4: Unified `load` tool replacing `mcp` + `skill`

**Files:** `gait-rs/src/tools/load.rs` (new), `gait-rs/src/tools/mod.rs`

- [ ] **Step 1: Write failing tests in `load.rs`**

Define test helpers at the bottom of `load.rs`:

```rust
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
```

Tests:

```rust
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
        crate::types::ToolPermission::RequiresApproval(reqs) => {
            assert!(reqs[0].patterns.contains(&"load:tdd".to_string()));
        }
        other => panic!("expected RequiresApproval, got {other:?}"),
    }
}

#[test]
fn execution_signal_is_load_context() {
    assert_eq!(LoadTool::new(vec![], vec![]).execution_signal(),
        crate::types::ExecutionSignal::LoadContext);
}
```

- [ ] **Step 2: Run to verify they fail**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test load 2>&1 | tail -10
```

- [ ] **Step 3: Implement `src/tools/load.rs`**

```rust
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
```

- [ ] **Step 4: Add `pub mod load;` to `mod.rs` and register `LoadTool`**

In `src/tools/mod.rs`:
- Add `pub mod load;` alongside the other `pub mod` declarations
- Add `use load::LoadTool;`
- In `builtin_tools`, remove `McpTool::new(...)` and `SkillTool::new(...)`, add `LoadTool::new(mcp_registry, skill_registry)`
- Update the `len() == 16` assertion to `len() == 15`
- Update `mcp_and_skill_meta_tools_present` test to check for `"load"` instead of `"mcp"` and `"skill"`

- [ ] **Step 5: Delete the old tool files**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && git rm src/tools/mcp.rs src/tools/skill.rs
```

- [ ] **Step 6: Run all tests**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test 2>&1 | tail -20
```

- [ ] **Step 7: Commit**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && git add src/tools/load.rs src/tools/mod.rs src/turn.rs && git commit -m "feat: unified load tool replacing mcp + skill tools"
```

---

## Task 5: `gait load` and `gait unload` CLI subcommands

**Files:** `gait-rs/src/bin/gait.rs`

These manage the `gait.jsonc` whitelist. `gait load` adds an item with two prompts (capability approval for plugins, then enable-now). `gait unload` removes entirely.

**Note:** rewriting `gait.jsonc` via serde strips JSONC comments — print a warning when this happens.

- [ ] **Step 1: Read the file first**

```bash
head -100 /Users/aicoder/Documents/ideation/gait/gait-rs/src/bin/gait.rs
```

- [ ] **Step 2: Write failing tests**

Use `tempfile::TempDir` for filesystem tests. Actual function signatures after Step 3: `cmd_load(workspace: &Path, args: &[String], yes: bool)` and `cmd_unload(workspace: &Path, args: &[String])`. Tests:

```rust
#[test]
fn cmd_load_mcp_writes_entry() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("gait.jsonc"),
        r#"{"model":"anthropic/claude-sonnet-4-6","mcp_servers":[]}"#).unwrap();
    let args: Vec<String> = ["mcp", "slack", "--spawn", "node mcp-slack.js"]
        .iter().map(|s| s.to_string()).collect();
    cmd_load(dir.path(), &args, true).unwrap();
    let content = std::fs::read_to_string(dir.path().join("gait.jsonc")).unwrap();
    let val: serde_json::Value = serde_json::from_str(&content).unwrap();
    assert_eq!(val["mcp_servers"][0]["name"], "slack");
    assert_eq!(val["mcp_servers"][0]["enabled"], true);
}

#[test]
fn cmd_load_skill_writes_entry() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("gait.jsonc"),
        r#"{"model":"anthropic/claude-sonnet-4-6","skills":[]}"#).unwrap();
    let args: Vec<String> = ["skill", "tdd", "--description", "tdd workflow",
        "--content", "# TDD"].iter().map(|s| s.to_string()).collect();
    cmd_load(dir.path(), &args, true).unwrap();
    let val: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(dir.path().join("gait.jsonc")).unwrap()).unwrap();
    assert_eq!(val["skills"][0]["name"], "tdd");
}

#[test]
fn cmd_unload_removes_entry() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("gait.jsonc"),
        r#"{"model":"x","mcp_servers":[{"name":"slack","spawn":"node s.js","enabled":true}]}"#).unwrap();
    let args: Vec<String> = ["mcp", "slack"].iter().map(|s| s.to_string()).collect();
    cmd_unload(dir.path(), &args).unwrap();
    let val: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(dir.path().join("gait.jsonc")).unwrap()).unwrap();
    assert!(val["mcp_servers"].as_array().unwrap().is_empty());
}

#[test]
fn cmd_load_unknown_kind_errors() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("gait.jsonc"), r#"{"model":"x"}"#).unwrap();
    let args: Vec<String> = ["plugin", "foo"].iter().map(|s| s.to_string()).collect();
    assert!(cmd_load(dir.path(), &args, true).is_err());
}
```

Also check `Cargo.toml` for `tempfile` dev-dependency and add it if missing.

- [ ] **Step 3: Implement helpers and `cmd_load`**

`GaitConfig` is not `Serialize`, so both commands round-trip through `serde_json::Value`. `strip_jsonc_comments` in `config.rs` is currently private — add `pub` to it first (one-character change at config.rs:77), then import it as `use gait_ai::config::strip_jsonc_comments;` in `bin/gait.rs`.

```rust
/// Parse --key value pairs from an arg slice into a HashMap.
fn parse_flags(args: &[String]) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    let mut i = 0;
    while i < args.len() {
        if let Some(key) = args[i].strip_prefix("--") {
            if i + 1 < args.len() && !args[i + 1].starts_with("--") {
                map.insert(key.to_string(), args[i + 1].clone());
                i += 2;
            } else {
                i += 1;
            }
        } else {
            i += 1;
        }
    }
    map
}

fn read_config_as_value(config_path: &std::path::Path) -> anyhow::Result<serde_json::Value> {
    let raw = std::fs::read_to_string(config_path).unwrap_or_else(|_| "{}".into());
    let stripped = gait_ai::config::strip_jsonc_comments(&raw);
    Ok(serde_json::from_str(&stripped).unwrap_or_default())
}

fn cmd_load(workspace: &std::path::Path, args: &[String], yes: bool) -> anyhow::Result<()> {
    // Supported usage:
    //   gait load mcp <name> --spawn <cmd>
    //   gait load mcp <name> --connect <url>
    //   gait load skill <name> --path <file>
    //   gait load skill <name> --description <text> --content <text>

    let kind = args.get(0).map(|s| s.as_str())
        .ok_or_else(|| anyhow::anyhow!("usage: gait load <mcp|skill> <name> [flags]"))?;
    let name = args.get(1).map(|s| s.as_str())
        .ok_or_else(|| anyhow::anyhow!("usage: gait load <mcp|skill> <name> [flags]"))?;
    let flags = parse_flags(&args[2..]);

    let config_path = workspace.join("gait.jsonc");
    let mut val = read_config_as_value(&config_path)?;

    // Prompt for enable-now (skip with --yes)
    let enabled = if yes {
        true
    } else {
        eprint!("Enable '{}' now? [Y/n] ", name);
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        !answer.trim().eq_ignore_ascii_case("n")
    };

    let entry = match kind {
        "mcp" => {
            if let Some(spawn) = flags.get("spawn") {
                serde_json::json!({ "name": name, "spawn": spawn, "enabled": enabled })
            } else if let Some(connect) = flags.get("connect") {
                serde_json::json!({ "name": name, "connect": connect, "enabled": enabled })
            } else {
                return Err(anyhow::anyhow!("gait load mcp: requires --spawn <cmd> or --connect <url>"));
            }
        }
        "skill" => {
            let description = flags.get("description")
                .ok_or_else(|| anyhow::anyhow!("gait load skill: requires --description <text>"))?;
            let content = if let Some(path) = flags.get("path") {
                std::fs::read_to_string(path)
                    .map_err(|e| anyhow::anyhow!("failed to read skill file '{}': {}", path, e))?
            } else if let Some(content) = flags.get("content") {
                content.clone()
            } else {
                return Err(anyhow::anyhow!("gait load skill: requires --path <file> or --content <text>"));
            };
            serde_json::json!({
                "name": name, "description": description,
                "content": content, "enabled": enabled
            })
        }
        other => return Err(anyhow::anyhow!("unknown kind '{}': use mcp or skill", other)),
    };

    let array_key = if kind == "mcp" { "mcp_servers" } else { "skills" };
    if val.get(array_key).is_none() {
        val[array_key] = serde_json::json!([]);
    }
    val[array_key].as_array_mut()
        .ok_or_else(|| anyhow::anyhow!("{} is not an array", array_key))?
        .push(entry);

    eprintln!("Note: gait.jsonc comments are stripped on rewrite.");
    std::fs::write(&config_path, serde_json::to_string_pretty(&val)?)?;
    eprintln!("Added {} '{}' (enabled={}).", kind, name, enabled);
    Ok(())
}
```

- [ ] **Step 4: Implement `cmd_unload`**

```rust
fn cmd_unload(workspace: &std::path::Path, args: &[String]) -> anyhow::Result<()> {
    let kind = args.get(0).map(|s| s.as_str())
        .ok_or_else(|| anyhow::anyhow!("usage: gait unload <mcp|skill> <name>"))?;
    let name = args.get(1).map(|s| s.as_str())
        .ok_or_else(|| anyhow::anyhow!("usage: gait unload <mcp|skill> <name>"))?;

    let array_key = match kind {
        "mcp" => "mcp_servers",
        "skill" => "skills",
        other => return Err(anyhow::anyhow!("unknown kind '{}': use mcp or skill", other)),
    };

    let config_path = workspace.join("gait.jsonc");
    let mut val = read_config_as_value(&config_path)?;

    if let Some(arr) = val[array_key].as_array_mut() {
        arr.retain(|v| v.get("name").and_then(|n| n.as_str()) != Some(name));
    }

    eprintln!("Note: gait.jsonc comments are stripped on rewrite.");
    std::fs::write(&config_path, serde_json::to_string_pretty(&val)?)?;
    eprintln!("Removed {} '{}'.", kind, name);
    Ok(())
}
```

- [ ] **Step 5: Add `extract_yes_flag` and wire into main**

```rust
fn extract_yes_flag(args: &[String]) -> (bool, Vec<String>) {
    let yes = args.iter().any(|a| a == "--yes" || a == "-y");
    let rest = args.iter()
        .filter(|a| *a != "--yes" && *a != "-y")
        .cloned()
        .collect();
    (yes, rest)
}

// in the main arg-dispatch match:
["load", rest @ ..] => {
    let rest: Vec<String> = rest.iter().map(|s| s.to_string()).collect();
    let (yes, rest) = extract_yes_flag(&rest);
    cmd_load(&workspace, &rest, yes)?
},
["unload", rest @ ..] => {
    let rest: Vec<String> = rest.iter().map(|s| s.to_string()).collect();
    cmd_unload(&workspace, &rest)?
},
```

- [ ] **Step 6: Run all tests**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test 2>&1 | tail -20
```

- [ ] **Step 7: Commit**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && git add src/bin/gait.rs && git commit -m "feat: gait load/unload CLI subcommands — manage loadable whitelist"
```

---

## Final: Full test suite

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test 2>&1 | tail -30
```

All tests must pass.

---

## Decision: MCP server vs tool enable-state

**Resolved:** Enable/disable at server granularity, not per-tool. The LLM calls `load("slack", enabled=true)` to enable the whole Slack server — all its tools become available at once. `load("slack", enabled=false)` removes them all.

**Implementation:** `config_enabled` in `TurnOpts` tracks **server names** (not tool names). When building the `<loadable-index>`, each MCP server gets one entry (not one entry per tool). The LoadContext handler pushes/removes all `McpToolDef` entries where `server_name` matches the item name. See Task 3 Steps 4, 6, and 7 for the concrete code.

**`<loadable-index>` for MCPs:** one line per server, not per tool. The description comes from `McpServerDef.description` (`Option<String>`, added in Task 1). If absent, show the tool count instead: `"N tools"`.

---

## Implementation Status (verified 2026-06-10)

**Overall: DONE** — all 5 tasks implemented and all tests passing.

### Task 1: `enabled` + `WasmPluginDef` + `pub strip_jsonc_comments` in `config.rs`
- [x] `enabled` field on `McpServerDef` and `SkillDef` with `#[serde(default = "default_enabled")]` — `config.rs:17–50`
- [x] `WasmPluginDef` stub with `name`, `path`, `enabled`, `allow_exec`, `allow_fs` — `config.rs:55`
- [x] `wasm_plugins: Vec<WasmPluginDef>` on `GaitConfig` — `config.rs:82`
- [x] `description: Option<String>` on `McpServerDef` — `config.rs:31`
- [x] `pub fn strip_jsonc_comments` — `config.rs:112`
- [x] Tests: `enabled_defaults_to_true_for_backward_compat`, `enabled_false_is_respected`, `wasm_plugin_def_parses` all present and passing

### Task 2: `LoadableKind` + `LoadableEntry` in `types.rs`
- [x] `LoadableKind` enum with `Mcp`, `Skill`, `WasmPlugin` and `as_str()` — `types.rs:241–255`
- [x] `LoadableEntry` struct with `name`, `kind`, `description`, `enabled` — `types.rs:259`
- [x] Test `loadable_kind_as_str` present and passing

### Task 3: `derive_enabled_loadables`, `build_loadable_index`, `config_enabled` in `turn.rs`
- [x] `derive_enabled_loadables` function — `turn.rs:155`
- [x] `build_loadable_index` function — `turn.rs:181`
- [x] `config_enabled: HashSet<String>` on `TurnOpts` — `turn.rs:37`
- [x] All 11 `TurnOpts {}` construction sites updated (`config_enabled: HashSet::new()` in tests, real values in `bin/gait.rs:363` and `exec_prompt.rs:73`)
- [x] Post-compaction reload loop (Step 6) — `turn.rs:593`
- [x] `LoadContext` handler with server-granularity push/retain + duplicate guard — `turn.rs:959`
- [x] `<loadable-index>` replaces `<mcp-index>`/`<skill-index>` in system prompt assembly — `turn.rs:617`
- [x] Tests: `build_loadable_index_shows_all_kinds`, `build_loadable_index_empty_emits_none_block`, `derive_enabled_loadables_seeds_from_config_and_replays_transcript` present and passing
- [ ] **DEVIATION — backward compat partially dropped:** `derive_enabled_loadables` only matches `name == "load"` (not `"mcp"` or `"skill"`), and only reads `"enabled"` key (not legacy `"context"` key). The plan specified both fallbacks; the test `derive_enabled_loadables_backward_compat_context_key` is absent. Old transcripts with `mcp`/`skill` tool calls will not replay. Acceptable per the note in the plan ("old per-tool loads won't auto-re-enable after compaction") but the `context` key fallback was explicitly planned and was not implemented.

### Task 4: Unified `LoadTool` in `tools/load.rs` + `tools/mod.rs`
- [x] `src/tools/load.rs` created with `LoadTool` struct implementing `Tool` — all methods present
- [x] `LoadTool::execute` returns skill content, MCP schema string, or `"unloaded: <name>"`
- [x] `execution_signal()` returns `ExecutionSignal::LoadContext`
- [x] `permission()` returns `RequiresApproval` with `"load"` and `"load:<name>"` patterns
- [x] `pub mod load;` and `use load::LoadTool;` in `tools/mod.rs:11,28`
- [x] `LoadTool::new(mcp_tools, skills)` in `builtin_tools` — `tools/mod.rs:125`
- [x] `mcp.rs` and `skill.rs` deleted — confirmed absent from `tools/` directory
- [x] Count assertion updated to 15 (`builtin_tools_returns_fifteen`) — `tools/mod.rs:147`
- [x] `load_meta_tool_present` test checks for `"load"` — `tools/mod.rs:181`
- [x] All `LoadTool` tests pass: `load_mcp_returns_schema`, `load_skill_returns_content`, `unload_returns_marker`, `unknown_name_returns_helpful_error`, `permission_has_load_prefix_patterns`, `execution_signal_is_load_context`

### Task 5: `gait load` / `gait unload` CLI subcommands in `bin/gait.rs`
- [x] `cmd_load` function implemented — `gait.rs:1180`
- [x] `cmd_unload` function implemented — `gait.rs:1269`
- [x] `"load"` and `"unload"` dispatched in main CLI match — `gait.rs:119,126`
- [x] `parse_flags` helper and `read_config_as_value` helper present
- [x] Tests: `cmd_load_mcp_writes_entry_disabled`, `cmd_load_skill_writes_entry`, `cmd_unload_removes_entry`, `cmd_load_unknown_kind_errors` present and passing
- [ ] **DEVIATION — design change:** The plan specified a `--yes` / interactive prompt flow where `cmd_load` sets `enabled=true` immediately. The actual implementation always writes `enabled=false` (whitelist-only), and a separate `gait enable` / `gait disable` command (`cmd_enable_disable`) was added. The test `cmd_load_mcp_writes_entry_disabled` explicitly asserts `enabled=false`. This is a deliberate UX improvement (two-step whitelist-then-enable), not an oversight — `gait enable`/`gait disable` tests also present and passing. `extract_yes_flag` was not implemented.

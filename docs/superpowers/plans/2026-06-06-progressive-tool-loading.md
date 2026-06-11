---
status: draft
---

# Progressive Tool Loading Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add `mcp(name, context)` and `skill(name, context)` meta-tools to gait-rs so the model can load/unload MCP tool schemas and skill content on demand, keeping the tools array small and the system prompt indexed but not bloated.

**Architecture:** Two new meta-tools (`McpTool`, `SkillTool`) always present in the tools array let the model inject schemas/content into the messages layer; loaded state is derived from the transcript on every turn (no separate storage); in-flight within-turn loading mutates the tools vec directly in the step loop. The system prompt gets `<mcp-index>` and `<skill-index>` sections generated from `gait.jsonc`.

**Tech Stack:** Rust, serde_json, schemars, tokio, existing gait-rs codebase patterns (see `src/tools/output.rs` for the minimal stateless tool pattern).

---

## File Structure

| Action | File | Responsibility |
|--------|------|----------------|
| Create | `gait-rs/src/tools/mcp.rs` | `McpTool` — always-present meta-tool; on load returns full schema; on unload returns marker |
| Create | `gait-rs/src/tools/skill.rs` | `SkillTool` — always-present meta-tool; on load returns skill content; on unload returns marker |
| Modify | `gait-rs/src/config.rs` | Add `McpToolDef`, `SkillDef` structs and `mcp_tools`/`skills` fields to `GaitConfig` |
| Modify | `gait-rs/src/tools/mod.rs` | Register new modules; update `builtin_tools` signature to accept registries |
| Modify | `gait-rs/src/session/context.rs` | Add `reconstruct_messages_from_events(&[AnyEvent])` so turn.rs can read events once |
| Modify | `gait-rs/src/turn.rs` | Add `derive_loaded_mcps`, `derive_loaded_skills`, index builders; augment tools/prompt; handle within-turn mutation |
| Modify | `gait-rs/src/bin/gait.rs` | Pass registries from config to `builtin_tools` and `TurnOpts` |
| Modify | `gait-rs/tests/integration_tools.rs` | Update tool count (11 → 13); add `mcp`/`skill` to expected names |

---

## Task 1: Config Types

**Files:**
- Modify: `gait-rs/src/config.rs`

- [x] **Step 1: Write the failing test**

Add to `gait-rs/src/config.rs` inside the `#[cfg(test)]` block:

```rust
#[test]
fn parses_mcp_tools_and_skills() {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("gait.jsonc"), r#"
{
  "model": "minimax/MiniMax-M3",
  "mcp_tools": [
    {
      "name": "slack_send",
      "description": "post a message to Slack",
      "schema": { "type": "object", "properties": { "channel": { "type": "string" }, "text": { "type": "string" } }, "required": ["channel", "text"] },
      "cmd": "node mcp-slack.js",
      "cwd": "workspace"
    }
  ],
  "skills": [
    { "name": "tdd", "description": "test-driven development workflow", "content": "# TDD\n1. Write failing test\n2. Make it pass\n3. Refactor" }
  ]
}
"#).unwrap();
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
```

- [x] **Step 2: Run test to verify it fails**

```bash
cd /path/to/gait-rs && cargo test parses_mcp_tools_and_skills 2>&1 | tail -5
```

Expected: FAIL — `GaitConfig` has no `mcp_tools` or `skills` field.

- [x] **Step 3: Add structs and fields to config.rs**

Add after the existing `ConfigTool` struct (around line 11):

```rust
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
```

Add `use serde::Serialize;` to the top import (it's currently only `Deserialize`):

```rust
use serde::{Deserialize, Serialize};
```

Add two new fields to `GaitConfig`:

```rust
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
```

Update `Default for GaitConfig`:

```rust
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
```

- [x] **Step 4: Run test to verify it passes**

```bash
cargo test parses_mcp_tools_and_skills 2>&1 | tail -5
```

Expected: PASS

- [x] **Step 5: Commit**

```bash
git add gait-rs/src/config.rs
git commit -m "feat(config): add McpToolDef, SkillDef types and mcp_tools/skills fields"
```

---

## Task 2: McpTool

**Files:**
- Create: `gait-rs/src/tools/mcp.rs`

The `McpTool` is always in the tools array. When the model calls `mcp("slack_send", true)`, it returns the full JSON schema of `slack_send` as a string — which lands in messages as a tool result. When called with `false`, it returns an unload marker. The harness (turn.rs) handles actually adding/removing the tool from the active tools vec.

- [x] **Step 1: Create mcp.rs with tests**

Create `gait-rs/src/tools/mcp.rs`:

```rust
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
            cmd: "node slack.js".into(),
            cwd: "workspace".into(),
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
```

- [x] **Step 2: Run tests to verify they fail**

```bash
cargo test mcp:: 2>&1 | tail -10
```

Expected: FAIL — module not registered yet.

- [x] **Step 3: Register module in mod.rs (minimal — just enough to compile)**

Add to `gait-rs/src/tools/mod.rs` top section:

```rust
pub mod mcp;
pub use mcp::McpTool;
```

- [x] **Step 4: Run tests to verify they pass**

```bash
cargo test mcp:: 2>&1 | tail -10
```

Expected: all 5 tests PASS.

- [x] **Step 5: Commit**

```bash
git add gait-rs/src/tools/mcp.rs gait-rs/src/tools/mod.rs
git commit -m "feat(tools): add McpTool for progressive MCP loading"
```

---

## Task 3: SkillTool

**Files:**
- Create: `gait-rs/src/tools/skill.rs`
- Modify: `gait-rs/src/tools/mod.rs`

- [x] **Step 1: Create skill.rs with tests**

Create `gait-rs/src/tools/skill.rs`:

```rust
use serde::Deserialize;
use schemars::JsonSchema;
use serde_json::Value;
use crate::types::Tool;
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

    pub fn boxed(self) -> Box<dyn Tool> {
        Box::new(self)
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
}
```

- [x] **Step 2: Run tests to verify they fail**

```bash
cargo test skill:: 2>&1 | tail -10
```

Expected: FAIL — module not registered yet.

- [x] **Step 3: Register in mod.rs**

Add to `gait-rs/src/tools/mod.rs`:

```rust
pub mod skill;
pub use skill::SkillTool;
```

- [x] **Step 4: Run tests to verify they pass**

```bash
cargo test skill:: 2>&1 | tail -10
```

Expected: all 5 tests PASS.

- [x] **Step 5: Commit**

```bash
git add gait-rs/src/tools/skill.rs gait-rs/src/tools/mod.rs
git commit -m "feat(tools): add SkillTool for progressive skill loading"
```

---

## Task 4: Update builtin_tools Signature

**Files:**
- Modify: `gait-rs/src/tools/mod.rs`
- Modify: `gait-rs/tests/integration_tools.rs`

The `builtin_tools` function must be updated to accept registries and include `McpTool` + `SkillTool`, bringing the total to 13.

- [x] **Step 1: Update the unit tests in mod.rs**

In `gait-rs/src/tools/mod.rs`, replace the existing tests block with:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn builtin_tools_returns_thirteen() {
        let tools = builtin_tools(PathBuf::from("/tmp"), vec![], vec![]);
        assert_eq!(tools.len(), 13);
    }

    #[test]
    fn all_tool_names_unique() {
        let tools = builtin_tools(PathBuf::from("/tmp"), vec![], vec![]);
        let mut names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        let original_len = names.len();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), original_len, "duplicate tool names found");
    }

    #[test]
    fn mcp_and_skill_meta_tools_present() {
        let tools = builtin_tools(PathBuf::from("/tmp"), vec![], vec![]);
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        assert!(names.contains(&"mcp"), "mcp tool missing, got: {:?}", names);
        assert!(names.contains(&"skill"), "skill tool missing, got: {:?}", names);
    }
}
```

- [x] **Step 2: Run tests to verify they fail**

```bash
cargo test tools::tests 2>&1 | tail -10
```

Expected: FAIL — `builtin_tools` still takes one arg.

- [x] **Step 3: Update builtin_tools in mod.rs**

Replace the current `builtin_tools` function and update imports:

```rust
use crate::config::{McpToolDef, SkillDef};

/// Returns all built-in tools configured for the given codebase root path.
pub fn builtin_tools(
    codebase_path: PathBuf,
    mcp_tools: Vec<McpToolDef>,
    skills: Vec<SkillDef>,
) -> Vec<Box<dyn Tool>> {
    vec![
        ReadTool::new(codebase_path.clone()).boxed(),
        WriteTool::new(codebase_path.clone()).boxed(),
        EditTool::new(codebase_path.clone()).boxed(),
        GlobTool::new(codebase_path.clone()).boxed(),
        GrepTool::new(codebase_path.clone()).boxed(),
        LsTool::new(codebase_path.clone()).boxed(),
        ShellTool::new(codebase_path.clone()).boxed(),
        OutputTool::new().boxed(),
        MultieditTool::new(codebase_path.clone()).boxed(),
        CodesearchTool::new(codebase_path.clone()).boxed(),
        WebfetchTool::new().boxed(),
        McpTool::new(mcp_tools).boxed(),
        SkillTool::new(skills).boxed(),
    ]
}
```

- [x] **Step 4: Run unit tests to verify they pass**

```bash
cargo test tools::tests 2>&1 | tail -10
```

Expected: all 3 tests PASS.

- [x] **Step 5: Update integration test**

In `gait-rs/tests/integration_tools.rs`, update `builtin_tools_returns_expected_tools`:

```rust
#[tokio::test]
async fn builtin_tools_returns_expected_tools() {
    let dir = TempDir::new().unwrap();
    let tools = builtin_tools(dir.path().to_path_buf(), vec![], vec![]);
    assert_eq!(tools.len(), 13);
    let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
    for expected in &["read", "write", "edit", "glob", "grep", "ls", "shell",
                      "output", "multiedit", "codesearch", "webfetch", "mcp", "skill"] {
        assert!(names.contains(expected), "missing tool '{}', got: {:?}", expected, names);
    }
}
```

Also update `all_tools_have_valid_json_schema_parameters` — just change the `builtin_tools` call:

```rust
let tools = builtin_tools(dir.path().to_path_buf(), vec![], vec![]);
```

And `read_write_roundtrip_via_builtin_tools` similarly:

```rust
let tools = builtin_tools(dir.path().to_path_buf(), vec![], vec![]);
```

- [x] **Step 6: Run all tests to confirm no regressions**

```bash
cargo test 2>&1 | tail -15
```

Expected: all tests pass (the bin/gait.rs call will fail to compile — fix in Task 7).

Actually, since `builtin_tools` call in `bin/gait.rs` will break compilation, compile-check before running:

```bash
cargo build 2>&1 | grep "^error" | head -10
```

Note the compile error location — you will fix it in Task 7.

- [x] **Step 7: Commit**

```bash
git add gait-rs/src/tools/mod.rs gait-rs/tests/integration_tools.rs
git commit -m "feat(tools): update builtin_tools to take mcp/skill registries, total 13 tools"
```

---

## Task 5: Derived State and Index Builders

**Files:**
- Modify: `gait-rs/src/session/context.rs`
- Modify: `gait-rs/src/turn.rs`

This task adds:
1. `reconstruct_messages_from_events` to context.rs (so turn.rs reads events once)
2. `derive_loaded_mcps`, `derive_loaded_skills` to turn.rs
3. `build_mcp_index`, `build_skill_index` to turn.rs

- [x] **Step 1: Write failing tests in turn.rs**

Add to `gait-rs/src/turn.rs` inside the `#[cfg(test)]` block:

```rust
#[test]
fn derive_loaded_mcps_empty_events() {
    let events: Vec<AnyEvent> = vec![];
    let loaded = derive_loaded_mcps(&events);
    assert!(loaded.is_empty());
}

#[test]
fn derive_loaded_mcps_single_load() {
    let events = vec![
        AnyEvent::ToolCall {
            id: "tc1".into(),
            name: "mcp".into(),
            input: serde_json::json!({ "name": "slack_send", "context": true }),
            ts: 1,
        },
    ];
    let loaded = derive_loaded_mcps(&events);
    assert_eq!(loaded.len(), 1);
    assert!(loaded.contains(&"slack_send".to_string()));
}

#[test]
fn derive_loaded_mcps_load_then_unload() {
    let events = vec![
        AnyEvent::ToolCall {
            id: "tc1".into(), name: "mcp".into(),
            input: serde_json::json!({ "name": "slack_send", "context": true }), ts: 1,
        },
        AnyEvent::ToolCall {
            id: "tc2".into(), name: "mcp".into(),
            input: serde_json::json!({ "name": "slack_send", "context": false }), ts: 2,
        },
    ];
    let loaded = derive_loaded_mcps(&events);
    assert!(loaded.is_empty(), "should be empty after unload, got: {:?}", loaded);
}

#[test]
fn derive_loaded_mcps_multiple_tools() {
    let events = vec![
        AnyEvent::ToolCall {
            id: "tc1".into(), name: "mcp".into(),
            input: serde_json::json!({ "name": "slack_send", "context": true }), ts: 1,
        },
        AnyEvent::ToolCall {
            id: "tc2".into(), name: "mcp".into(),
            input: serde_json::json!({ "name": "notion_create", "context": true }), ts: 2,
        },
        AnyEvent::ToolCall {
            id: "tc3".into(), name: "mcp".into(),
            input: serde_json::json!({ "name": "slack_send", "context": false }), ts: 3,
        },
    ];
    let loaded = derive_loaded_mcps(&events);
    assert_eq!(loaded.len(), 1);
    assert!(loaded.contains(&"notion_create".to_string()));
}

#[test]
fn build_mcp_index_empty() {
    use crate::config::McpToolDef;
    let index = build_mcp_index(&[]);
    assert!(index.is_empty());
}

#[test]
fn build_mcp_index_formats_correctly() {
    use crate::config::McpToolDef;
    let registry = vec![
        McpToolDef { name: "slack_send".into(), description: "post to Slack".into(),
            schema: serde_json::json!({}), cmd: "node s.js".into(), cwd: "workspace".into() },
    ];
    let index = build_mcp_index(&registry);
    assert!(index.contains("<mcp-index>"), "got: {index}");
    assert!(index.contains("slack_send"), "got: {index}");
    assert!(index.contains("post to Slack"), "got: {index}");
    assert!(index.contains("</mcp-index>"), "got: {index}");
}

#[test]
fn build_skill_index_formats_correctly() {
    use crate::config::SkillDef;
    let registry = vec![
        SkillDef { name: "tdd".into(), description: "TDD workflow".into(), content: "...".into() },
    ];
    let index = build_skill_index(&registry);
    assert!(index.contains("<skill-index>"), "got: {index}");
    assert!(index.contains("tdd"), "got: {index}");
    assert!(index.contains("TDD workflow"), "got: {index}");
    assert!(index.contains("</skill-index>"), "got: {index}");
}
```

- [x] **Step 2: Run tests to verify they fail**

```bash
cargo test "derive_loaded\|build_mcp_index\|build_skill_index" 2>&1 | tail -10
```

Expected: FAIL — functions don't exist yet.

- [x] **Step 3: Add derive functions and index builders to turn.rs**

Add these functions to `gait-rs/src/turn.rs` (before `run_turn`, after the imports):

```rust
use std::collections::HashMap;
use crate::config::{McpToolDef, SkillDef};

/// Scan transcript events to derive which MCP tools are currently loaded.
/// Last call per tool name wins. Returns names of tools where context=true.
/// IMPORTANT: Pass only the post-compaction slice (from last_compaction_index forward).
/// Loads before the compaction breakpoint are intentionally forgotten — the model
/// reloads only what it still needs after compaction.
pub fn derive_loaded_mcps(events: &[AnyEvent]) -> Vec<String> {
    let mut state: HashMap<String, bool> = HashMap::new();
    for event in events {
        if let AnyEvent::ToolCall { name, input, .. } = event {
            if name == "mcp" {
                if let (Some(tool_name), Some(context)) = (
                    input.get("name").and_then(|v| v.as_str()),
                    input.get("context").and_then(|v| v.as_bool()),
                ) {
                    state.insert(tool_name.to_string(), context);
                }
            }
        }
    }
    state.into_iter().filter(|(_, loaded)| *loaded).map(|(name, _)| name).collect()
}

/// Same as derive_loaded_mcps but for skills.
pub fn derive_loaded_skills(events: &[AnyEvent]) -> Vec<String> {
    let mut state: HashMap<String, bool> = HashMap::new();
    for event in events {
        if let AnyEvent::ToolCall { name, input, .. } = event {
            if name == "skill" {
                if let (Some(skill_name), Some(context)) = (
                    input.get("name").and_then(|v| v.as_str()),
                    input.get("context").and_then(|v| v.as_bool()),
                ) {
                    state.insert(skill_name.to_string(), context);
                }
            }
        }
    }
    state.into_iter().filter(|(_, loaded)| *loaded).map(|(name, _)| name).collect()
}

/// Build the <mcp-index> system prompt section. Returns empty string if registry is empty.
pub fn build_mcp_index(registry: &[McpToolDef]) -> String {
    if registry.is_empty() { return String::new(); }
    let lines: Vec<String> = registry.iter()
        .map(|d| format!("{} — {}", d.name, d.description))
        .collect();
    format!("\n<mcp-index>\n{}\n</mcp-index>", lines.join("\n"))
}

/// Build the <skill-index> system prompt section. Returns empty string if registry is empty.
pub fn build_skill_index(registry: &[SkillDef]) -> String {
    if registry.is_empty() { return String::new(); }
    let lines: Vec<String> = registry.iter()
        .map(|d| format!("{} — {}", d.name, d.description))
        .collect();
    format!("\n<skill-index>\n{}\n</skill-index>", lines.join("\n"))
}
```

- [x] **Step 4: Add reconstruct_messages_from_events to context.rs**

In `gait-rs/src/session/context.rs`, refactor `reconstruct_messages` to expose an events-based variant:

```rust
pub fn reconstruct_messages_from_events(all_events: &[AnyEvent]) -> anyhow::Result<Vec<Message>> {
    if all_events.is_empty() { return Ok(vec![]); }

    // Start from last compaction breakpoint (or beginning)
    let start = last_compaction_index(all_events).unwrap_or(0);
    let events = &all_events[start..];

    // ... (move the entire existing body of reconstruct_messages here, replacing `all_events` with the param) ...
}

pub fn reconstruct_messages(path: &Path) -> anyhow::Result<Vec<Message>> {
    let all_events = read_events(path)?;
    reconstruct_messages_from_events(&all_events)
}
```

The full body of `reconstruct_messages_from_events` is the current body of `reconstruct_messages` (lines 6–155 of context.rs), with only one change: replace `let all_events = read_events(path)?;` with the function parameter and keep the rest identical.

- [x] **Step 5: Run all tests**

```bash
cargo test 2>&1 | tail -15
```

Expected: all tests pass (the new derive/index tests + no regressions).

- [x] **Step 6: Commit**

```bash
git add gait-rs/src/turn.rs gait-rs/src/session/context.rs
git commit -m "feat(turn): add derive_loaded_mcps/skills, index builders; refactor context reconstruction"
```

---

## Task 6: Turn Loop Integration

**Files:**
- Modify: `gait-rs/src/turn.rs`

This task wires derived state into the actual `run_turn` loop:
1. Add `mcp_registry` and `skill_registry` to `TurnOpts`
2. At turn start: read events once, derive loaded MCPs, build index, augment tools and system prompt
3. In step loop: when `mcp`/`skill` tool is called with context=true, add the actual ToolDef to tools
4. Exempt `mcp` and `skill` from permission gating (like `output`)

- [x] **Step 1: Write failing test**

Add to `gait-rs/src/turn.rs` tests:

```rust
// NOTE: uses std::sync::Mutex — add `use std::sync::Mutex;` at the top of the test module.

// --- Helper: a provider that captures the tools list on the FIRST stream() call ---
struct CapturingProvider {
    captured_tools: Arc<Mutex<Vec<String>>>,
    captured_sys: Arc<Mutex<String>>,
    responses: Arc<Vec<Vec<StreamChunk>>>,
    call_idx: Arc<AtomicUsize>,
}
#[async_trait::async_trait]
impl Provider for CapturingProvider {
    fn model_id(&self) -> &str { "mock" }
    async fn stream(&self, system: &str, _msgs: &[Message], tools: &[Box<dyn Tool>])
        -> Result<std::pin::Pin<Box<dyn futures_util::Stream<Item = Result<StreamChunk, ProviderError>> + Send>>, ProviderError>
    {
        let idx = self.call_idx.fetch_add(1, Ordering::SeqCst);
        {
            let mut t = self.captured_tools.lock().unwrap();
            if t.is_empty() { *t = tools.iter().map(|t| t.name().to_string()).collect(); }
        }
        {
            let mut s = self.captured_sys.lock().unwrap();
            if s.is_empty() { *s = system.to_string(); }
        }
        let chunks = self.responses.get(idx).cloned().unwrap_or_default();
        use async_stream::stream;
        Ok(Box::pin(stream! { for c in chunks { yield Ok(c); } }))
    }
}

#[tokio::test]
async fn loaded_mcp_from_prior_turn_appears_in_tools() {
    // Run turn 1 where the model calls mcp("notify", true) — this writes the ToolCall
    // event to the transcript via the normal turn loop (avoids pre-writing outside of
    // create_session_worktree which would conflict).
    use crate::config::McpToolDef;
    let ws = TempDir::new().unwrap();
    init_git_repo(ws.path());
    use std::sync::Mutex;

    let notify_def = McpToolDef {
        name: "notify".into(), description: "send notification".into(),
        schema: serde_json::json!({ "type": "object", "properties": {} }),
        cmd: "echo notify".into(), cwd: "codebase".into(),
    };
    let mcp_tool = crate::tools::McpTool::new(vec![notify_def.clone()]);

    // Turn 1: model loads "notify" then says done
    let provider1 = Arc::new(MockProvider {
        responses: Arc::new(vec![
            vec![
                StreamChunk::ToolCallComplete {
                    tool_call_id: "tc_mcp".into(), tool_name: "mcp".into(),
                    args: serde_json::json!({ "name": "notify", "context": true }),
                },
                StreamChunk::Done,
            ],
            vec![StreamChunk::Text("done".into()), StreamChunk::Done],
        ]),
        call_idx: Arc::new(AtomicUsize::new(0)),
    });
    run_turn(
        TurnOpts {
            session_id: "smcp001".into(), task: Some("load notify".into()), auto: true,
            workspace: ws.path().to_path_buf(), codebase_path: ws.path().to_path_buf(),
            allow: vec![], system_prompt: "help".into(), model: "mock/test".into(),
            mcp_registry: vec![notify_def.clone()], skill_registry: vec![],
        },
        provider1,
        vec![Box::new(mcp_tool)],
    ).await.unwrap();

    // Turn 2: capture which tools the provider sees on first call
    let captured = Arc::new(Mutex::new(vec![]));
    let provider2 = Arc::new(CapturingProvider {
        captured_tools: captured.clone(),
        captured_sys: Arc::new(Mutex::new(String::new())),
        responses: Arc::new(vec![
            vec![StreamChunk::Text("done".into()), StreamChunk::Done],
        ]),
        call_idx: Arc::new(AtomicUsize::new(0)),
    });
    run_turn(
        TurnOpts {
            session_id: "smcp001".into(), task: Some("continue".into()), auto: true,
            workspace: ws.path().to_path_buf(), codebase_path: ws.path().to_path_buf(),
            allow: vec![], system_prompt: "help".into(), model: "mock/test".into(),
            mcp_registry: vec![notify_def.clone()], skill_registry: vec![],
        },
        provider2,
        vec![Box::new(crate::tools::McpTool::new(vec![notify_def.clone()]))],
    ).await.unwrap();

    let tools = captured.lock().unwrap();
    assert!(tools.contains(&"notify".to_string()),
        "MCP tool loaded in turn 1 should appear in tools array for turn 2, got: {:?}", tools);
}

#[tokio::test]
async fn within_turn_mcp_load_makes_tool_callable() {
    // Model calls mcp("notify", true) in step 1, then calls "notify" tool in step 2.
    use crate::config::McpToolDef;
    let ws = TempDir::new().unwrap();
    init_git_repo(ws.path());

    let notify_def = McpToolDef {
        name: "notify".into(), description: "send notification".into(),
        schema: serde_json::json!({ "type": "object", "properties": { "msg": { "type": "string" } }, "required": ["msg"] }),
        cmd: "echo notified".into(), cwd: "codebase".into(),
    };

    let provider = Arc::new(MockProvider {
        responses: Arc::new(vec![
            // Step 1: load the mcp tool
            vec![
                StreamChunk::ToolCallComplete {
                    tool_call_id: "tc1".into(), tool_name: "mcp".into(),
                    args: serde_json::json!({ "name": "notify", "context": true }),
                },
                StreamChunk::Done,
            ],
            // Step 2: call the notify tool (now available after within-turn mutation)
            vec![
                StreamChunk::ToolCallComplete {
                    tool_call_id: "tc2".into(), tool_name: "notify".into(),
                    args: serde_json::json!({ "msg": "hello" }),
                },
                StreamChunk::Done,
            ],
            // Step 3: done
            vec![StreamChunk::Text("all done".into()), StreamChunk::Done],
        ]),
        call_idx: Arc::new(AtomicUsize::new(0)),
    });

    run_turn(
        TurnOpts {
            session_id: "smcp_wt".into(), task: Some("use notify".into()), auto: true,
            workspace: ws.path().to_path_buf(), codebase_path: ws.path().to_path_buf(),
            allow: vec![], system_prompt: "help".into(), model: "mock/test".into(),
            mcp_registry: vec![notify_def.clone()], skill_registry: vec![],
        },
        provider,
        vec![Box::new(crate::tools::McpTool::new(vec![notify_def]))],
    ).await.unwrap();

    // Verify transcript: should have two tool_calls (mcp + notify) and turn_complete
    let tx_path = ws.path().join("sessions/smcp_wt/transcript.jsonl");
    let events = crate::session::transcript::read_events(&tx_path).unwrap();
    let tool_calls: Vec<&str> = events.iter().filter_map(|e| {
        if let AnyEvent::ToolCall { name, .. } = e { Some(name.as_str()) } else { None }
    }).collect();
    assert!(tool_calls.contains(&"mcp"), "mcp call missing: {:?}", tool_calls);
    assert!(tool_calls.contains(&"notify"), "notify call missing — within-turn mutation failed: {:?}", tool_calls);
    assert!(events.iter().any(|e| matches!(e, AnyEvent::TurnComplete { .. })));
}

#[tokio::test]
async fn system_prompt_contains_mcp_index_when_registry_nonempty() {
    use crate::config::McpToolDef;
    let ws = TempDir::new().unwrap();
    init_git_repo(ws.path());

    let captured_sys: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let cap = captured_sys.clone();

    struct SysCapturingProvider { cap: Arc<Mutex<String>> }
    #[async_trait::async_trait]
    impl Provider for SysCapturingProvider {
        fn model_id(&self) -> &str { "mock" }
        async fn stream(&self, system: &str, _msgs: &[Message], _tools: &[Box<dyn Tool>])
            -> Result<std::pin::Pin<Box<dyn futures_util::Stream<Item = Result<StreamChunk, ProviderError>> + Send>>, ProviderError>
        {
            let mut c = self.cap.lock().unwrap();
            if c.is_empty() { *c = system.to_string(); }
            use async_stream::stream;
            Ok(Box::pin(stream! {
                yield Ok(StreamChunk::Text("done".into()));
                yield Ok(StreamChunk::Done);
            }))
        }
    }

    run_turn(
        TurnOpts {
            session_id: "smcp002".into(),
            task: Some("do it".into()),
            auto: true,
            workspace: ws.path().to_path_buf(),
            codebase_path: ws.path().to_path_buf(),
            allow: vec![],
            system_prompt: "base prompt".into(),
            model: "mock/test".into(),
            mcp_registry: vec![McpToolDef {
                name: "slack_send".into(), description: "post to Slack".into(),
                schema: serde_json::json!({}), cmd: "node s.js".into(), cwd: "workspace".into(),
            }],
            skill_registry: vec![],
        },
        Arc::new(SysCapturingProvider { cap: cap }),
        vec![],
    ).await.unwrap();

    let sys = captured_sys.lock().unwrap();
    assert!(sys.contains("<mcp-index>"), "system prompt: {sys}");
    assert!(sys.contains("slack_send"), "system prompt: {sys}");
}
```

Note: these tests use `Arc<Mutex<...>>` to capture arguments passed to the provider. You'll need `use std::sync::Mutex;` at the top of the test module.

- [x] **Step 2: Run tests to verify they fail**

```bash
cargo test "loaded_mcp_from_prior\|system_prompt_contains_mcp" 2>&1 | tail -10
```

Expected: FAIL — `TurnOpts` missing `mcp_registry` and `skill_registry`.

- [x] **Step 3: Add fields to TurnOpts**

In `gait-rs/src/turn.rs`, update `TurnOpts`:

```rust
use crate::config::{McpToolDef, SkillDef};

pub struct TurnOpts {
    pub session_id: String,
    pub task: Option<String>,
    pub auto: bool,
    pub workspace: PathBuf,
    pub codebase_path: PathBuf,
    pub allow: Vec<String>,
    pub system_prompt: String,
    pub model: String,
    pub mcp_registry: Vec<McpToolDef>,
    pub skill_registry: Vec<SkillDef>,
}
```

- [x] **Step 4: Update run_turn to augment tools and system prompt**

In `run_turn`, after the `reconstruct_messages` call and before the step loop, add:

```rust
// Read events once for both message reconstruction and derived tool state.
// Remove the existing `let mut messages = reconstruct_messages(&tx_path)?;` line,
// and also remove `use crate::session::context::reconstruct_messages;` from imports —
// replace with `use crate::session::context::reconstruct_messages_from_events;`
// to avoid an unused-import compile error.
let events = crate::session::transcript::read_events(&tx_path)?;
let mut messages = crate::session::context::reconstruct_messages_from_events(&events)?;

// Prepend turn context as first user message
let turn_ctx = build_turn_context(&codebase_hash, &dirty_files);
messages.insert(0, Message::User { content: MessageContent::Text(turn_ctx) });

// Derive loaded MCP tools from the POST-COMPACTION slice only.
// Pre-compaction loads are intentionally forgotten (spec: model reloads after compaction).
let compaction_start = crate::session::transcript::last_compaction_index(&events).unwrap_or(0);
let post_compaction = &events[compaction_start..];
let loaded_mcp_names = derive_loaded_mcps(post_compaction);
let mut tools = tools;  // take ownership so we can mutate
for name in &loaded_mcp_names {
    if let Some(def) = opts.mcp_registry.iter().find(|d| d.name == *name) {
        tools.push(make_mcp_tool_def(def, &opts.workspace, &opts.codebase_path));
    }
}

// Augment system prompt with MCP/skill indexes
let mcp_idx = build_mcp_index(&opts.mcp_registry);
let skill_idx = build_skill_index(&opts.skill_registry);
let system_prompt = if mcp_idx.is_empty() && skill_idx.is_empty() {
    opts.system_prompt.clone()
} else {
    format!("{}{}{}", opts.system_prompt, mcp_idx, skill_idx)
};
```

Add the helper function just above `run_turn`:

```rust
fn make_mcp_tool_def(
    def: &McpToolDef,
    workspace: &std::path::Path,
    codebase_path: &std::path::Path,
) -> Box<dyn crate::types::Tool> {
    let cmd = def.cmd.clone();
    let cwd = if def.cwd == "workspace" {
        workspace.to_path_buf()
    } else {
        codebase_path.to_path_buf()
    };
    let name = def.name.clone();
    let description = def.description.clone();
    let schema = def.schema.clone();
    Box::new(crate::types::ToolDef {
        name,
        description,
        parameters: schema,
        execute: Box::new(move |_args| {
            let cmd = cmd.clone();
            let cwd = cwd.clone();
            Box::pin(async move {
                let output = tokio::process::Command::new("sh")
                    .arg("-c").arg(&cmd)
                    .current_dir(&cwd)
                    .output().await
                    .map_err(|e| anyhow::anyhow!("{}", e))?;
                let stdout = String::from_utf8_lossy(&output.stdout).to_string();
                let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                let combined = if stderr.is_empty() { stdout }
                    else { format!("{}\nstderr:\n{}", stdout, stderr) };
                Ok(serde_json::Value::String(combined))
            })
        }),
    })
}
```

Also change the `provider.stream()` call to use the local `system_prompt` instead of `&opts.system_prompt`:

```rust
let mut chunk_stream = provider.stream(&system_prompt, &messages, &tools).await
```

- [x] **Step 5: Add within-turn mcp mutation**

In the tool execution loop (after `let output = match tools.iter()...`), add handling after executing `mcp` and `skill` calls:

```rust
// Handle within-turn mcp/skill context changes
if tc.tool_name == "mcp" {
    if let (Some(name), Some(ctx)) = (
        tc.args.get("name").and_then(|v| v.as_str()),
        tc.args.get("context").and_then(|v| v.as_bool()),
    ) {
        if ctx {
            if let Some(def) = opts.mcp_registry.iter().find(|d| d.name == name) {
                // Only add if not already present
                if !tools.iter().any(|t| t.name() == name) {
                    tools.push(make_mcp_tool_def(def, &opts.workspace, &opts.codebase_path));
                }
            }
        } else {
            tools.retain(|t| t.name() != name);
        }
    }
}
```

Place this block right before the `append_event(ToolResult)` call in the tool execution loop (after `output` is computed).

- [x] **Step 6: Exempt mcp and skill from permission gating**

Find this line in `run_turn`:

```rust
let allowed = opts.auto || opts.allow.contains(&tc.tool_name) || tc.tool_name == "output";
```

Change to:

```rust
let allowed = opts.auto || opts.allow.contains(&tc.tool_name)
    || tc.tool_name == "output"
    || tc.tool_name == "mcp"
    || tc.tool_name == "skill";
```

- [x] **Step 7: Fix existing tests that construct TurnOpts**

All existing tests in `turn.rs` that construct `TurnOpts` must add the two new fields. Add to each:

```rust
mcp_registry: vec![],
skill_registry: vec![],
```

There are 4 such tests: `run_turn_writes_transcript_and_commits`, `output_tool_result_used_in_turn_complete`, `output_tool_exempt_from_permission_gating`, `permission_gating_pauses_turn_in_non_auto_mode`.

- [x] **Step 8: Run all tests**

```bash
cargo test 2>&1 | tail -20
```

Expected: all tests pass. Fix any compile errors before continuing.

- [x] **Step 9: Commit**

```bash
git add gait-rs/src/turn.rs gait-rs/src/session/context.rs
git commit -m "feat(turn): integrate progressive tool loading — derived state, index injection, within-turn mutation"
```

---

## Task 7: CLI Wiring

**Files:**
- Modify: `gait-rs/src/bin/gait.rs`

The `cmd_run` function currently calls `builtin_tools(codebase_path.clone())` and builds `TurnOpts` without mcp/skill registries. Both need updating.

- [x] **Step 1: Update builtin_tools call**

In `cmd_run`, find:

```rust
let mut tools = builtin_tools(codebase_path.clone());
```

Replace with:

```rust
let mut tools = builtin_tools(codebase_path.clone(), config.mcp_tools.clone(), config.skills.clone());
```

- [x] **Step 2: Update TurnOpts construction**

In `cmd_run`, find the `TurnOpts { ... }` struct literal and add:

```rust
mcp_registry: config.mcp_tools.clone(),
skill_registry: config.skills.clone(),
```

- [x] **Step 3: Build and verify**

```bash
cargo build 2>&1 | grep "^error" | head -10
```

Expected: clean build, no errors.

- [x] **Step 4: Run full test suite**

```bash
cargo test 2>&1 | tail -20
```

Expected: all tests pass.

- [x] **Step 5: Commit**

```bash
git add gait-rs/src/bin/gait.rs
git commit -m "feat(cli): wire mcp/skill registries from config to builtin_tools and TurnOpts"
```

---

## Task 8: End-to-End Smoke Test

**Files:**
- No code changes — manual verification only.

- [x] **Step 1: Build the release binary**

```bash
cargo build --release --manifest-path gait-rs/Cargo.toml 2>&1 | tail -5
```

Expected: compiles to `target/release/gait`.

- [x] **Step 2: Create a test workspace with mcp and skill config**

```bash
mkdir -p /tmp/gait-ptl-test && cd /tmp/gait-ptl-test && git init && git commit --allow-empty -m "init"
mkdir sessions
cat > gait.jsonc << 'EOF'
{
  "model": "minimax/MiniMax-M3",
  "allow": ["read", "glob", "grep", "ls", "mcp", "skill", "output"],
  "mcp_tools": [
    {
      "name": "echo_tool",
      "description": "echo the input back",
      "schema": { "type": "object", "properties": { "msg": { "type": "string" } }, "required": ["msg"] },
      "cmd": "echo $MSG",
      "cwd": "codebase"
    }
  ],
  "skills": [
    {
      "name": "hello_skill",
      "description": "a greeting skill",
      "content": "# Hello Skill\nAlways greet the user warmly."
    }
  ]
}
EOF
cat > AGENTS.md << 'EOF'
You are a test agent. Your task is to:
1. Load the `hello_skill` skill
2. Load the `echo_tool` MCP tool
3. Call output() with summary="loaded both tools successfully"
EOF
```

- [x] **Step 3: Run with rs-gait binary**

```bash
/path/to/gait-rs/target/release/gait run \
  --task "Load the hello_skill skill and echo_tool MCP tool, then call output()." \
  --workspace /tmp/gait-ptl-test \
  --codebase /tmp/gait-ptl-test \
  --auto
```

Expected output includes:
- `[gait] starting session s...`
- Model calls `skill("hello_skill", true)` → sees skill content in next step
- Model calls `mcp("echo_tool", true)` → sees schema in next step
- Model calls `output(summary="...")` → `[gait] turn 1 complete`

- [x] **Step 4: Verify system prompt contains indexes**

Check transcript for the session to confirm the session ran cleanly:

```bash
cat /tmp/gait-ptl-test/sessions/*/transcript.jsonl | python3 -m json.tool | grep '"type"'
```

Expected: `session_start`, `user_turn`, `tool_call` (skill), `tool_result`, `tool_call` (mcp), `tool_result`, `tool_call` (output), `tool_result`, `turn_complete`.

---

## Self-Review

**Spec coverage check:**

| Spec Requirement | Task |
|-----------------|------|
| `mcp(name, context)` tool always in tools array | Task 2, Task 4 |
| `skill(name, context)` tool always in tools array | Task 3, Task 4 |
| On mcp load: inject schema as tool result | Task 2 (McpTool.execute returns schema) |
| On skill load: inject content as tool result | Task 3 (SkillTool.execute returns content) |
| On unload: inject unload marker | Task 2 + Task 3 |
| Harness adds loaded MCP tool to tools array next call | Task 6 (within-turn mutation) |
| Harness removes on unload | Task 6 (tools.retain) |
| Derived state from transcript — no separate storage | Task 5 (derive_loaded_mcps) |
| Resume/crash correct: scan reconstructs state | Task 5 (reads events, derives on every turn start) |
| `<mcp-index>` in system prompt | Task 5 (build_mcp_index) + Task 6 |
| `<skill-index>` in system prompt | Task 5 (build_skill_index) + Task 6 |
| Tiny, stable, prompt-cached index | Task 5 (name + one-liner format) |
| After compaction: loaded set becomes empty, reloads naturally | `derive_loaded_mcps` receives only the post-compaction slice (via `last_compaction_index` in Task 6 step 4) — pre-compaction loads are not visible, so state correctly resets ✓ |
| mcp/skill tools exempt from permission gating | Task 6 (permission exemption) |
| Config-driven registries | Task 1 (McpToolDef, SkillDef) + Task 7 (wired from gait.jsonc) |

**Placeholder scan:** No TBDs, no "add error handling" comments. All steps have code.

**Type consistency check:**
- `McpToolDef` used in config.rs → Task 1 ✓; imported in tools/mcp.rs → Task 2 ✓; imported in tools/mod.rs → Task 4 ✓; in TurnOpts → Task 6 ✓; in bin/gait.rs → Task 7 ✓
- `derive_loaded_mcps(&[AnyEvent]) -> Vec<String>` defined in Task 5, used in Task 6 ✓
- `build_mcp_index(&[McpToolDef]) -> String` defined in Task 5, used in Task 6 ✓
- `make_mcp_tool_def(def: &McpToolDef, ..) -> Box<dyn Tool>` defined + used in Task 6 ✓
- `reconstruct_messages_from_events(&[AnyEvent])` defined in Task 5, used in Task 6 ✓

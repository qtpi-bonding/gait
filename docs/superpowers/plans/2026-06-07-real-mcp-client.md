---
status: done
---

# Real MCP Client Implementation Plan

**Status: DONE** — all four implementation tasks are fully complete as of 2026-06-10.

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the static/stub MCP tool execution with a real MCP protocol client using the `rmcp` v1.x crate, supporting both spawn (gait owns the subprocess, stdio transport) and connect (HTTP streamable transport) modes.

**Architecture:** Add an `McpClient` wrapper in `src/mcp_client.rs` that uses `rmcp` to discover tools via `tools/list` at startup and execute them via `tools/call` at runtime. Config gains `McpServerDef` (transport config) while `McpToolDef` becomes a pure runtime struct populated from discovery. Everything else — the toggle mechanic, context pruning, mcp-index — stays unchanged.

**Tech Stack:** `rmcp = { version = "1", features = ["client", "transport-child-process", "transport-streamable-http-client-reqwest"] }` (already added to Cargo.toml and building clean).

---

## Verified rmcp 1.7.0 API

These are exact types confirmed from source at `~/.cargo/registry/src/.../rmcp-1.7.0/src/`:

```rust
use rmcp::{ServiceExt, service::RunningService, RoleClient};
use rmcp::transport::{TokioChildProcess, ConfigureCommandExt, StreamableHttpClientTransport};
use rmcp::model::{CallToolRequestParams, Tool, RawContent};

// Spawn (stdio) transport
let transport = TokioChildProcess::new(
    tokio::process::Command::new("npx").configure(|cmd| { cmd.args([...]); })
)?;
let client: RunningService<RoleClient, ()> = ().serve(transport).await?;

// Connect (HTTP streamable) transport
let transport = StreamableHttpClientTransport::from_uri("http://host/mcp");
let client: RunningService<RoleClient, ()> = ().serve(transport).await?;

// Discover tools (handles pagination automatically)
let tools: Vec<Tool> = client.list_all_tools().await?;
// Tool fields: tool.name (Cow<'static,str>), tool.description (Option<Cow<'static,str>>),
//              tool.input_schema (Arc<serde_json::Map<String, Value>>)

// Call a tool
use serde_json::Map;
let result = client.call_tool(
    CallToolRequestParams::new("tool_name")
        .with_arguments(map)  // map: serde_json::Map<String, Value>
).await?;
// result.content: Vec<Annotated<RawContent>>, result.is_error: Option<bool>
// Annotated<T> derefs to T, so match &*item to get RawContent
// Text variant: RawContent::Text(t) => t.text (String)
```

---

## File Map

| File | Action | Responsibility |
|------|--------|----------------|
| `gait-rs/Cargo.toml` | Done | `rmcp` v1 added, builds clean |
| `gait-rs/src/config.rs` | Modify | Add `McpServerDef`; strip `cmd`/`cwd` from `McpToolDef`; add `server_name`; swap `GaitConfig.mcp_tools → mcp_servers` |
| `gait-rs/src/mcp_client.rs` | Create | `McpClient` — spawn/connect, `discover_tools()`, `call_tool()` |
| `gait-rs/src/lib.rs` | Modify | `pub mod mcp_client;` |
| `gait-rs/src/turn.rs` | Modify | `TurnOpts` gains `mcp_clients`; `make_mcp_tool_def` takes `Arc<McpClient>`; toggle handler uses client map |
| `gait-rs/src/bin/gait.rs` | Modify | Call discovery at startup; pass client map + registry to `TurnOpts` |
| `gait-rs/src/tools/mod.rs` | Verify | `builtin_tools` receives `Vec<McpToolDef>` — confirm it still compiles after struct change |

---

## Task 1: Update config types

**Files:**
- Modify: `gait-rs/src/config.rs`

Split "server transport config" (how to connect) from "runtime tool definition" (what was discovered). Config describes *how to connect*; tool schemas come from discovery at runtime.

- [ ] **Step 1: Write failing test for McpServerDef deserialization**

Add to the `#[cfg(test)]` block in `config.rs`:

```rust
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
```

- [ ] **Step 2: Run test to verify it fails**

```bash
cd gait-rs && cargo test parses_mcp_servers_spawn_and_connect 2>&1 | tail -5
```

Expected: compile error — `McpServerDef` and `mcp_servers` field don't exist yet.

- [ ] **Step 3: Add McpServerDef, update McpToolDef, update GaitConfig**

In `config.rs`, **replace** the current `McpToolDef` struct with these two:

```rust
/// Config-level: how to connect to one MCP server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerDef {
    pub name: String,
    /// Shell command to spawn an MCP server subprocess (stdio transport).
    pub spawn: Option<String>,
    /// URL of an already-running MCP server (HTTP streamable transport).
    pub connect: Option<String>,
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
```

In `GaitConfig`, replace `mcp_tools: Vec<McpToolDef>` with:

```rust
#[serde(default)]
pub mcp_servers: Vec<McpServerDef>,
```

In `GaitConfig::default()`, replace `mcp_tools: vec![]` with `mcp_servers: vec![]`.

- [ ] **Step 4: Run test to verify it passes**

```bash
cd gait-rs && cargo test parses_mcp_servers_spawn_and_connect 2>&1 | tail -5
```

Expected: PASS.

- [ ] **Step 5: Update the old mcp_tools test to use new shape**

The test `parses_mcp_tools_and_skills` will fail — rename and update it:

```rust
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
```

- [ ] **Step 6: Verify config tests pass (compile errors elsewhere are OK for now)**

```bash
cd gait-rs && cargo test --lib config 2>&1 | tail -10
```

Expected: all config module tests pass.

- [ ] **Step 7: Commit**

```bash
cd gait-rs && git add src/config.rs
git commit -m "refactor: McpServerDef (config transport) + McpToolDef (runtime discovery)"
```

---

## Task 2: Implement McpClient

**Files:**
- Create: `gait-rs/src/mcp_client.rs`
- Modify: `gait-rs/src/lib.rs`

- [ ] **Step 1: Create mcp_client.rs with stub + tests**

```rust
use std::sync::Arc;
use serde_json::{Value, Map};
use crate::config::{McpServerDef, McpToolDef};
use rmcp::{ServiceExt, RoleClient, service::RunningService};
use rmcp::transport::{TokioChildProcess, ConfigureCommandExt, StreamableHttpClientTransport};
use rmcp::model::{CallToolRequestParams, RawContent};

pub struct McpClient {
    server_name: String,
    handle: RunningService<RoleClient, ()>,
}

impl McpClient {
    pub async fn connect(def: &McpServerDef) -> anyhow::Result<Self> {
        todo!()
    }

    pub async fn discover_tools(&self) -> anyhow::Result<Vec<McpToolDef>> {
        todo!()
    }

    pub async fn call_tool(&self, name: &str, args: Value) -> anyhow::Result<Value> {
        todo!()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn connect_fails_gracefully_when_binary_missing() {
        let def = McpServerDef {
            name: "test".into(),
            spawn: Some("no-such-binary-xyz-abc --foo".into()),
            connect: None,
        };
        let result = McpClient::connect(&def).await;
        assert!(result.is_err(), "expected error for missing binary");
    }

    #[tokio::test]
    async fn connect_fails_gracefully_when_url_unreachable() {
        let def = McpServerDef {
            name: "test".into(),
            spawn: None,
            connect: Some("http://127.0.0.1:19191/mcp".into()),
        };
        let result = McpClient::connect(&def).await;
        assert!(result.is_err(), "expected error for unreachable URL");
    }

    #[tokio::test]
    async fn connect_fails_when_neither_spawn_nor_connect() {
        let def = McpServerDef { name: "test".into(), spawn: None, connect: None };
        let result = McpClient::connect(&def).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("neither spawn nor connect"));
    }
}
```

- [ ] **Step 2: Add module to lib.rs**

```rust
pub mod mcp_client;
```

- [ ] **Step 3: Run tests to confirm todo!() panics (harness wired)**

```bash
cd gait-rs && cargo test mcp_client 2>&1 | tail -10
```

Expected: tests run and panic with `not yet implemented`.

- [ ] **Step 4: Implement connect()**

Replace the `connect` body:

```rust
pub async fn connect(def: &McpServerDef) -> anyhow::Result<Self> {
    let handle: RunningService<RoleClient, ()> = if let Some(ref cmd_str) = def.spawn {
        let mut parts = cmd_str.split_whitespace();
        let prog = parts.next()
            .ok_or_else(|| anyhow::anyhow!("empty spawn command for MCP server '{}'", def.name))?;
        let rest: Vec<&str> = parts.collect();
        let transport = TokioChildProcess::new(
            tokio::process::Command::new(prog).configure(|cmd| { cmd.args(&rest); })
        ).map_err(|e| anyhow::anyhow!("MCP spawn '{}' failed: {e}", def.name))?;
        ().serve(transport).await
            .map_err(|e| anyhow::anyhow!("MCP handshake failed (spawn '{}'):  {e}", def.name))?
    } else if let Some(ref url) = def.connect {
        let transport = StreamableHttpClientTransport::from_uri(url.as_str());
        ().serve(transport).await
            .map_err(|e| anyhow::anyhow!("MCP handshake failed (connect '{}'): {e}", def.name))?
    } else {
        anyhow::bail!("MCP server '{}' has neither spawn nor connect", def.name);
    };
    Ok(Self { server_name: def.name.clone(), handle })
}
```

- [ ] **Step 5: Implement discover_tools()**

```rust
pub async fn discover_tools(&self) -> anyhow::Result<Vec<McpToolDef>> {
    let tools = self.handle.list_all_tools().await
        .map_err(|e| anyhow::anyhow!("tools/list failed for '{}': {e}", self.server_name))?;
    Ok(tools.into_iter().map(|t| McpToolDef {
        server_name: self.server_name.clone(),
        name: t.name.to_string(),
        description: t.description.as_deref().unwrap_or("").to_string(),
        schema: serde_json::Value::Object((*t.input_schema).clone()),
    }).collect())
}
```

- [ ] **Step 6: Implement call_tool()**

```rust
pub async fn call_tool(&self, name: &str, args: Value) -> anyhow::Result<Value> {
    let arguments: Option<Map<String, Value>> = match args {
        Value::Object(map) => Some(map),
        Value::Null => None,
        other => anyhow::bail!("MCP tool args must be a JSON object, got: {other}"),
    };
    let mut req = CallToolRequestParams::new(name.to_string());
    if let Some(map) = arguments {
        req = req.with_arguments(map);
    }
    let result = self.handle.call_tool(req).await
        .map_err(|e| anyhow::anyhow!("tools/call '{}' on '{}' failed: {e}", name, self.server_name))?;

    let text: String = result.content.iter()
        .filter_map(|item| match &**item {
            RawContent::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");

    if result.is_error == Some(true) {
        anyhow::bail!("MCP tool '{}' returned error: {}", name, text);
    }
    Ok(Value::String(text))
}
```

- [ ] **Step 7: Run mcp_client tests**

```bash
cd gait-rs && cargo test mcp_client 2>&1 | tail -10
```

Expected: all 3 tests pass — missing binary fails gracefully, unreachable URL fails gracefully, missing transport fails.

- [ ] **Step 8: Commit**

```bash
cd gait-rs && git add src/mcp_client.rs src/lib.rs
git commit -m "feat: McpClient wrapping rmcp for stdio spawn and HTTP streamable transports"
```

---

## Task 3: Wire discovery at startup

**Files:**
- Modify: `gait-rs/src/bin/gait.rs`

At startup, connect to each configured MCP server, discover its tools, and build two things: the `mcp_registry: Vec<McpToolDef>` (for the index + toggle) and `mcp_clients: HashMap<String, Arc<McpClient>>` (for routing `tools/call`).

- [ ] **Step 1: Add imports at top of gait.rs**

```rust
use std::collections::HashMap;
use std::sync::Arc;
use gait_ai::mcp_client::McpClient;
```

- [ ] **Step 2: Replace config.mcp_tools with discovery**

Find the section in `gait.rs` where `config.mcp_tools` is referenced. Replace it with:

```rust
// Discover tools from all configured MCP servers
let mut mcp_registry: Vec<gait_ai::config::McpToolDef> = vec![];
let mut mcp_clients: HashMap<String, Arc<McpClient>> = HashMap::new();

for server_def in &config.mcp_servers {
    match McpClient::connect(server_def).await {
        Ok(client) => {
            match client.discover_tools().await {
                Ok(tools) => {
                    eprintln!("[gait] mcp '{}': {} tool(s)", server_def.name, tools.len());
                    mcp_registry.extend(tools);
                    mcp_clients.insert(server_def.name.clone(), Arc::new(client));
                }
                Err(e) => eprintln!("[gait] warning: mcp '{}' tools/list: {e}", server_def.name),
            }
        }
        Err(e) => eprintln!("[gait] warning: mcp '{}' connect: {e}", server_def.name),
    }
}
```

- [ ] **Step 3: Update builtin_tools call**

Change `config.mcp_tools.clone()` to `mcp_registry.clone()`:

```rust
let mut tools = builtin_tools(
    codebase_path.clone(),
    session_id.clone(),
    workspace.clone(),
    mcp_registry.clone(),
    config.skills.clone(),
);
```

- [ ] **Step 4: Update TurnOpts (mcp_clients field added in Task 4)**

Change `mcp_registry: config.mcp_tools` to:

```rust
mcp_registry: mcp_registry,
mcp_clients: mcp_clients,
```

- [ ] **Step 5: Verify compile errors are only about the new mcp_clients field**

```bash
cd gait-rs && cargo build 2>&1 | grep "^error" | head -10
```

Expected: only `no field 'mcp_clients' on type 'TurnOpts'` — that gets fixed in Task 4.

- [ ] **Step 6: Commit**

```bash
cd gait-rs && git add src/bin/gait.rs
git commit -m "feat: discover MCP tools at startup, build client registry"
```

---

## Task 4: Wire call_tool into turn.rs

**Files:**
- Modify: `gait-rs/src/turn.rs`

Update `TurnOpts`, `make_mcp_tool_def`, and both call sites to route through the real `McpClient`.

- [ ] **Step 1: Add imports to turn.rs**

```rust
use std::collections::HashMap;
use std::sync::Arc;
use crate::mcp_client::McpClient;
```

- [ ] **Step 2: Add mcp_clients field to TurnOpts**

```rust
pub struct TurnOpts {
    // ... existing fields ...
    pub mcp_registry: Vec<McpToolDef>,
    pub mcp_clients: HashMap<String, Arc<McpClient>>,  // keyed by server_name
    pub skill_registry: Vec<SkillDef>,
}
```

- [ ] **Step 3: Replace make_mcp_tool_def**

The current signature takes `workspace` and `codebase_path` and runs a shell command. Replace the entire function:

```rust
fn make_mcp_tool_def(def: &McpToolDef, client: Arc<McpClient>) -> Box<dyn crate::types::Tool> {
    let name = def.name.clone();
    let description = def.description.clone();
    let schema = def.schema.clone();
    Box::new(crate::types::ToolDef {
        name: name.clone(),
        description,
        parameters: schema,
        execute: Box::new(move |args| {
            let client = client.clone();
            let name = name.clone();
            Box::pin(async move { client.call_tool(&name, args).await })
        }),
    })
}
```

- [ ] **Step 4: Update call site 1 — restore after compaction**

Find the block around `derive_loaded_mcps` (around line 380). Update to look up the client:

```rust
let loaded_mcp_names = derive_loaded_mcps(post_compaction);
for name in &loaded_mcp_names {
    if let Some(def) = opts.mcp_registry.iter().find(|d| d.name == *name) {
        if let Some(client) = opts.mcp_clients.get(&def.server_name) {
            if !tools.iter().any(|t| t.name() == name.as_str()) {
                tools.push(make_mcp_tool_def(def, client.clone()));
            }
        }
    }
}
```

- [ ] **Step 5: Update call site 2 — mcp toggle handler**

Find the `"mcp" => {` branch (around line 549). Update the load arm:

```rust
"mcp" => {
    if let (Some(name), Some(ctx)) = (
        tc.args.get("name").and_then(|v| v.as_str()),
        tc.args.get("context").and_then(|v| v.as_bool()),
    ) {
        if ctx {
            if let Some(def) = opts.mcp_registry.iter().find(|d| d.name == name) {
                if !tools.iter().any(|t| t.name() == name) {
                    if let Some(client) = opts.mcp_clients.get(&def.server_name) {
                        tools.push(make_mcp_tool_def(def, client.clone()));
                    }
                }
            }
        } else {
            tools.retain(|t| t.name() != name);
        }
    }
    // execute_tool_call for the mcp meta-tool itself continues below unchanged
```

- [ ] **Step 6: Fix TurnOpts test constructions**

Search for all test TurnOpts builds:

```bash
grep -n "mcp_registry:" gait-rs/src/turn.rs
```

For each one, add `mcp_clients: std::collections::HashMap::new(),` next to `mcp_registry: vec![],`.

- [ ] **Step 7: Full build**

```bash
cd gait-rs && cargo build 2>&1 | grep "^error" | head -10
```

Expected: clean.

- [ ] **Step 8: All tests pass**

```bash
cd gait-rs && cargo test 2>&1 | tail -10
```

Expected: all pass.

- [ ] **Step 9: Commit**

```bash
cd gait-rs && git add src/turn.rs
git commit -m "feat: route MCP tool execution through real McpClient.call_tool"
```

---

## Task 5: Smoke test

Manual end-to-end verification with the official filesystem MCP server.

- [ ] **Step 1: Create a test workspace**

```bash
mkdir -p /tmp/gait-mcp-test
cat > /tmp/gait-mcp-test/gait.jsonc << 'EOF'
{
  "model": "anthropic/claude-sonnet-4-6",
  "mcp_servers": [
    {
      "name": "filesystem",
      "spawn": "npx -y @modelcontextprotocol/server-filesystem /tmp"
    }
  ]
}
EOF
```

- [ ] **Step 2: Confirm discovery logs on startup**

```bash
cd gait-rs && cargo run -- --workspace /tmp/gait-mcp-test "What MCP tools are available?" 2>&1 | head -10
```

Expected: `[gait] mcp 'filesystem': N tool(s)` in stderr before the agent responds.

- [ ] **Step 3: Confirm toggle works — load then unload**

```bash
cd gait-rs && cargo run -- --workspace /tmp/gait-mcp-test \
  "Call mcp with name=read_file and context=true to load it. Then call mcp with context=false to unload it. Confirm it was removed." 2>&1 | tail -20
```

Expected: agent loads the tool, then unloads it, confirms absence.

- [ ] **Step 4: Final commit**

```bash
cd gait-rs && git add -A && git status
git commit -m "feat: real MCP client integration complete"
```

---

## Self-Review

**Spec coverage:**
- [x] Real `tools/list` discovery at startup — Task 3
- [x] Spawn mode (stdio, gait owns subprocess) — `McpClient::connect` + `TokioChildProcess`
- [x] Connect mode (HTTP streamable to existing server) — `McpClient::connect` + `StreamableHttpClientTransport`
- [x] Real `tools/call` on tool use — `McpClient::call_tool` + `make_mcp_tool_def`
- [x] Toggle mechanic preserved — Task 4 call sites, same toggle logic
- [x] mcp-index and context pruning unchanged — `build_mcp_index`, `derive_loaded_mcps` untouched
- [x] Run-and-done safe — subprocess killed when `RunningService` drops at process exit

**Risks:**
- `CallToolRequestParams::new` takes `impl Into<Cow<'static, str>>` — `name.to_string()` may need `.into()` or `Cow::Owned(name.to_string())`. Adjust if compile error.
- `RawContent::Text(t)` — `t.text` field name: verified as `String` in `RawTextContent`. If it's `Cow`, use `.as_ref()`.
- Subprocess lifetime: `RunningService` holds the child process; when it drops, rmcp should kill it. If zombie processes appear, call `client.handle.cancel().await` explicitly in a Drop impl.

---

## Implementation Status (verified 2026-06-10)

**Overall: DONE** — all tasks implemented and code is present in main.

### Task 1: Update config types — DONE

- [x] `McpServerDef` struct with `name`, `spawn`, `connect`, `enabled`, `description` fields — `gait-rs/src/config.rs:21-32`
- [x] `McpToolDef` runtime struct with `server_name`, `name`, `description`, `schema` — `gait-rs/src/config.rs:36-42`
- [x] `GaitConfig.mcp_servers: Vec<McpServerDef>` (replaces old `mcp_tools`) — `gait-rs/src/config.rs:78`
- [x] `GaitConfig::default()` uses `mcp_servers: vec![]` — `gait-rs/src/config.rs:104`
- [x] Tests: `parses_mcp_servers_spawn_and_connect`, `parses_mcp_servers_and_skills`, `enabled_defaults_to_true_for_backward_compat`, `enabled_false_is_respected` — `gait-rs/src/config.rs:298-377`
- [x] `McpServerDef` has two extra fields beyond the plan (`enabled`, `description`) — additive, not breaking

### Task 2: Implement McpClient — DONE

- [x] `gait-rs/src/mcp_client.rs` created
- [x] `McpClient { server_name, handle: RunningService<RoleClient, ()> }` — `mcp_client.rs:8-11`
- [x] `connect()` — spawn branch uses `TokioChildProcess` + `ConfigureCommandExt`; connect branch uses `StreamableHttpClientTransport` — `mcp_client.rs:14-33`
- [x] `discover_tools()` — calls `list_all_tools()`, maps to `McpToolDef` — `mcp_client.rs:35-44`
- [x] `call_tool()` — builds `CallToolRequestParams`, collects `RawContent::Text` — `mcp_client.rs:46-71`
- [x] `pub mod mcp_client;` in `lib.rs:24`
- [x] Three error-path tests (`connect_fails_gracefully_when_binary_missing`, `connect_fails_gracefully_when_url_unreachable`, `connect_fails_when_neither_spawn_nor_connect`) — `mcp_client.rs:78-113`
- [x] `rmcp = { version = "1", features = [...] }` in `Cargo.toml:30`

### Task 3: Wire discovery at startup — DONE

- [x] `mcp_registry` and `mcp_clients` built at startup in `bin/gait.rs:263-279`
- [x] Iterates `config.mcp_servers`, calls `McpClient::connect`, then `discover_tools` — `bin/gait.rs:265-278`
- [x] `eprintln!("[gait] mcp {}: {} tool(s)", ...)` logged on success — `bin/gait.rs:270`
- [x] Both maps passed into `TurnOpts` — `bin/gait.rs:360-361`
- [x] `builtin_tools` receives `mcp_registry.clone()` — `bin/gait.rs:288`

### Task 4: Wire call_tool into turn.rs — DONE

- [x] `TurnOpts.mcp_registry: Vec<McpToolDef>` and `TurnOpts.mcp_clients: HashMap<String, Arc<McpClient>>` — `turn.rs:32-33`
- [x] `make_mcp_tool_def(def, client)` implemented using `ToolDef` with async `call_tool` closure — `turn.rs:208-222`
- [x] Restore-after-compaction call site uses `opts.mcp_clients.get(&def.server_name)` — `turn.rs:595-598`
- [x] Toggle handler (mcp load/unload) uses same client lookup — `turn.rs:968-971`
- [x] All test `TurnOpts` constructions include `mcp_clients: std::collections::HashMap::new()` — multiple sites in `turn.rs`

### Task 5: Smoke test — NOT VERIFIED

- [ ] Manual end-to-end smoke test with `@modelcontextprotocol/server-filesystem` not recorded in code history. Discovery logging and toggle wiring are in place; live test outcome unknown.

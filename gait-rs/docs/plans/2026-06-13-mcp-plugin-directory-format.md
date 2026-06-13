---
status: done
---

# MCP & Plugin Directory Format Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move MCP server configs to ecosystem-standard `mcp/<name>.json` flat files and WASM plugin configs to `plugins/<name>/PLUGIN.md` directory files, with the gait three-tier model (register → enable → allow) applied to both.

**Architecture:** Each MCP server becomes a single JSON file at `mcp/<name>.json` using the ecosystem-standard `command`/`args`/`env` (stdio) or `url` (HTTP) format — identical to Claude Desktop and Cursor configs. Each WASM plugin becomes a directory `plugins/<name>/PLUGIN.md` (YAML frontmatter + model-visible docs) alongside a `.wasm` binary, mirroring the existing `skills/<name>/SKILL.md` pattern. Both are scanned at profile build time; `gait.jsonc` `mcp_servers` array stays as a deprecated-but-working fallback. The three-tier CLI vocabulary is preserved: `gait register` creates the file (Tier 1), `gait enable/disable` sets the `enabled` flag in the file (Tier 2), `gait allow/deny` controls execution patterns (Tier 3, existing mechanism unchanged).

**Tech Stack:** Rust, serde_json, serde_yaml, existing `split_frontmatter` from `src/agent_frontmatter.rs`

---

## File Map

- **Modify**: `src/config.rs` — add `command`, `args`, `env`, `url` fields to `McpServerDef`; keep `spawn`/`connect` as compat shims
- **Modify**: `src/mcp_client.rs` — update `McpClient::connect` to handle new fields
- **Create**: `src/mcp_scanner.rs` — scans `mcp/<name>.json` files
- **Create**: `src/wasm_plugins.rs` — scans `plugins/<name>/PLUGIN.md` files
- **Modify**: `src/profile.rs` — add MCP scanner + plugin scanner, add `wasm_plugins` to `Profile`
- **Modify**: `src/bin/gait.rs` — update `cmd_register`, `cmd_enable_disable`, `cmd_unregister` for disk files; add `plugin` kind

---

## Task 1: Update `McpServerDef` and `McpClient::connect` for ecosystem format

The current `McpServerDef` uses `spawn: Option<String>` (full command string) and `connect: Option<String>` (URL). The ecosystem standard (Claude Desktop, Cursor, VS Code) uses `command`/`args`/`env` for stdio and `url` for HTTP. Add the new fields while keeping `spawn`/`connect` as silent backward-compat shims.

**Files:**
- Modify: `src/config.rs`
- Modify: `src/mcp_client.rs`

- [ ] **Step 1: Add new fields to `McpServerDef` in `src/config.rs`**

Replace the existing `McpServerDef` struct (around line 20):

```rust
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
```

- [ ] **Step 2: Update `McpClient::connect` in `src/mcp_client.rs`**

Replace the entire `connect` method body so it checks `command` before `spawn`, and `url` before `connect`:

```rust
pub async fn connect(def: &McpServerDef) -> anyhow::Result<Self> {
    let handle: RunningService<RoleClient, ()> = if let Some(ref cmd) = def.command {
        // Ecosystem-standard stdio: command + args + env
        let args: &[String] = def.args.as_deref().unwrap_or(&[]);
        let transport = TokioChildProcess::new(
            tokio::process::Command::new(cmd).configure(|c| {
                c.args(args);
                if let Some(env) = &def.env {
                    for (k, v) in env { c.env(k, v); }
                }
            })
        ).map_err(|e| anyhow::anyhow!("MCP spawn {} failed: {e}", def.name))?;
        ().serve(transport).await
            .map_err(|e| anyhow::anyhow!("MCP handshake failed (command {}): {e}", def.name))?
    } else if let Some(ref cmd_str) = def.spawn {
        // Compat: split "prog arg1 arg2" into command + args
        let mut parts = cmd_str.split_whitespace();
        let prog = parts.next()
            .ok_or_else(|| anyhow::anyhow!("empty spawn command for MCP server {}", def.name))?;
        let rest: Vec<&str> = parts.collect();
        let transport = TokioChildProcess::new(
            tokio::process::Command::new(prog).configure(|cmd| { cmd.args(&rest); })
        ).map_err(|e| anyhow::anyhow!("MCP spawn {} failed: {e}", def.name))?;
        ().serve(transport).await
            .map_err(|e| anyhow::anyhow!("MCP handshake failed (spawn {}): {e}", def.name))?
    } else if let Some(ref url) = def.url {
        // Ecosystem-standard HTTP transport
        let transport = StreamableHttpClientTransport::from_uri(url.as_str());
        ().serve(transport).await
            .map_err(|e| anyhow::anyhow!("MCP handshake failed (url {}): {e}", def.name))?
    } else if let Some(ref url) = def.connect {
        // Compat: connect → url
        let transport = StreamableHttpClientTransport::from_uri(url.as_str());
        ().serve(transport).await
            .map_err(|e| anyhow::anyhow!("MCP handshake failed (connect {}): {e}", def.name))?
    } else {
        anyhow::bail!("MCP server {} has neither command, spawn, url, nor connect", def.name);
    };
    Ok(Self { server_name: def.name.clone(), handle })
}
```

- [ ] **Step 3: Add tests for new connect paths in `src/mcp_client.rs`**

Append to the existing `#[cfg(test)]` block:

```rust
#[tokio::test]
async fn connect_fails_gracefully_with_command_missing_binary() {
    let def = McpServerDef {
        name: "test".into(),
        command: Some("no-such-binary-xyz-abc".into()),
        args: Some(vec!["--foo".into()]),
        env: None,
        url: None,
        spawn: None,
        connect: None,
        enabled: true,
        description: None,
    };
    assert!(McpClient::connect(&def).await.is_err());
}

#[tokio::test]
async fn connect_fails_neither_command_nor_url() {
    let def = McpServerDef {
        name: "test".into(), command: None, args: None, env: None,
        url: None, spawn: None, connect: None, enabled: true, description: None,
    };
    let err = McpClient::connect(&def).await.unwrap_err();
    assert!(err.to_string().contains("neither command"), "got: {err}");
}
```

- [ ] **Step 4: Run tests**

```bash
cargo test 2>&1 > /tmp/gait-test-output.txt; echo "exit: $?"
grep "^error" /tmp/gait-test-output.txt | head -20
grep "FAILED\|test result" /tmp/gait-test-output.txt
```

Expected: all tests pass (existing `connect_fails_when_neither_spawn_nor_connect` test will need its error message updated to match the new "neither command, spawn, url, nor connect" wording).

- [ ] **Step 5: Fix the existing error-message test if it fails**

In `src/mcp_client.rs` tests, find the test `connect_fails_when_neither_spawn_nor_connect` and update the assertion:

```rust
// before
assert!(e.to_string().contains("neither spawn nor connect"), "got: {e}"),
// after
assert!(e.to_string().contains("neither command"), "got: {e}"),
```

- [ ] **Step 6: Commit**

```bash
git add src/config.rs src/mcp_client.rs
git commit -m "feat: McpServerDef command/args/env/url fields; keep spawn/connect as compat shims"
```

---

## Task 2: Create `src/mcp_scanner.rs`

Scan `<gait_dir>/mcp/<name>.json` flat files. Each file is a JSON object; `name` is derived from the filename. Follows the same pattern as `src/skills.rs`.

**Files:**
- Create: `src/mcp_scanner.rs`
- Modify: `src/lib.rs` (or `src/main.rs` — wherever modules are declared; look for `mod skills;`)

- [ ] **Step 1: Write the failing test first**

Create `src/mcp_scanner.rs` with only the test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_mcp(dir: &TempDir, name: &str, content: &str) {
        let mcp_dir = dir.path().join("mcp");
        std::fs::create_dir_all(&mcp_dir).unwrap();
        std::fs::write(mcp_dir.join(format!("{}.json", name)), content).unwrap();
    }

    #[test]
    fn scanner_returns_empty_when_no_mcp_dir() {
        let dir = TempDir::new().unwrap();
        assert!(scan_mcp_servers(dir.path()).is_empty());
    }

    #[test]
    fn scanner_reads_command_args_env() {
        let dir = TempDir::new().unwrap();
        write_mcp(&dir, "slack", r#"{
            "command": "npx",
            "args": ["-y", "@modelcontextprotocol/server-slack"],
            "env": {"SLACK_TOKEN": "xoxb-test"},
            "enabled": false,
            "description": "Slack tools"
        }"#);
        let servers = scan_mcp_servers(dir.path());
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].name, "slack");
        assert_eq!(servers[0].command.as_deref(), Some("npx"));
        assert_eq!(servers[0].args.as_deref(), Some(&["-y".to_string(), "@modelcontextprotocol/server-slack".to_string()][..]));
        assert_eq!(servers[0].env.as_ref().unwrap()["SLACK_TOKEN"], "xoxb-test");
        assert!(!servers[0].enabled);
        assert_eq!(servers[0].description.as_deref(), Some("Slack tools"));
    }

    #[test]
    fn scanner_reads_url_server() {
        let dir = TempDir::new().unwrap();
        write_mcp(&dir, "remote", r#"{"url": "http://localhost:3001/mcp", "enabled": true}"#);
        let servers = scan_mcp_servers(dir.path());
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].name, "remote");
        assert_eq!(servers[0].url.as_deref(), Some("http://localhost:3001/mcp"));
        assert!(servers[0].enabled);
    }

    #[test]
    fn scanner_enabled_defaults_to_true_when_absent() {
        let dir = TempDir::new().unwrap();
        write_mcp(&dir, "noenabled", r#"{"command": "npx", "args": []}"#);
        let servers = scan_mcp_servers(dir.path());
        assert!(servers[0].enabled);
    }

    #[test]
    fn scanner_skips_file_with_no_connection() {
        let dir = TempDir::new().unwrap();
        write_mcp(&dir, "bad", r#"{"description": "no command or url"}"#);
        assert!(scan_mcp_servers(dir.path()).is_empty());
    }

    #[test]
    fn scanner_skips_invalid_json() {
        let dir = TempDir::new().unwrap();
        write_mcp(&dir, "broken", "{ not valid json");
        assert!(scan_mcp_servers(dir.path()).is_empty());
    }

    #[test]
    fn scanner_skips_non_json_files() {
        let dir = TempDir::new().unwrap();
        let mcp_dir = dir.path().join("mcp");
        std::fs::create_dir_all(&mcp_dir).unwrap();
        std::fs::write(mcp_dir.join("README.md"), "# docs").unwrap();
        assert!(scan_mcp_servers(dir.path()).is_empty());
    }

    #[test]
    fn scanner_accepts_legacy_spawn_field() {
        let dir = TempDir::new().unwrap();
        write_mcp(&dir, "legacy", r#"{"spawn": "node mcp.js", "enabled": false}"#);
        let servers = scan_mcp_servers(dir.path());
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].name, "legacy");
        assert_eq!(servers[0].spawn.as_deref(), Some("node mcp.js"));
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

```bash
cargo test mcp_scanner 2>&1 > /tmp/gait-test-output.txt; echo "exit: $?"
grep "^error" /tmp/gait-test-output.txt | head -20
```

Expected: compile error — `scan_mcp_servers` not found.

- [ ] **Step 3: Implement `scan_mcp_servers`**

Add the full implementation above the test module in `src/mcp_scanner.rs`:

```rust
use std::path::Path;
use crate::config::McpServerDef;

/// Scan `<gait_dir>/mcp/<name>.json` and return one `McpServerDef` per valid file.
///
/// A file is valid if it is parseable JSON and has at least one of: `command`,
/// `spawn`, `url`, or `connect`. The server name is the filename stem (e.g.
/// `mcp/slack.json` → name "slack"). Non-.json files are silently skipped.
pub fn scan_mcp_servers(gait_dir: &Path) -> Vec<McpServerDef> {
    let mcp_dir = gait_dir.join("mcp");
    if !mcp_dir.is_dir() { return vec![]; }

    let mut out = Vec::new();
    let entries = match std::fs::read_dir(&mcp_dir) {
        Ok(e) => e,
        Err(err) => {
            eprintln!("[gait] warning: could not read mcp dir {}: {err}", mcp_dir.display());
            return vec![];
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") { continue; }
        let name = match path.file_stem().and_then(|s| s.to_str()) {
            Some(n) if !n.is_empty() => n.to_string(),
            _ => continue,
        };
        match parse_mcp_file(&path, &name) {
            Some(def) => out.push(def),
            None => eprintln!("[gait] warning: skipping invalid mcp file {}", path.display()),
        }
    }
    out
}

fn parse_mcp_file(path: &Path, name: &str) -> Option<McpServerDef> {
    let content = std::fs::read_to_string(path).ok()?;
    let val: serde_json::Value = serde_json::from_str(&content).ok()?;
    let obj = val.as_object()?;

    let command = obj.get("command").and_then(|v| v.as_str()).map(|s| s.to_string());
    let args = obj.get("args").and_then(|v| v.as_array()).map(|a| {
        a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect()
    });
    let env = obj.get("env").and_then(|v| v.as_object()).map(|m| {
        m.iter()
            .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
            .collect()
    });
    let url = obj.get("url").and_then(|v| v.as_str()).map(|s| s.to_string());
    let spawn = obj.get("spawn").and_then(|v| v.as_str()).map(|s| s.to_string());
    let connect = obj.get("connect").and_then(|v| v.as_str()).map(|s| s.to_string());

    // Must have at least one connection method
    if command.is_none() && spawn.is_none() && url.is_none() && connect.is_none() {
        return None;
    }

    Some(McpServerDef {
        name: name.to_string(),
        command,
        args,
        env,
        url,
        spawn,
        connect,
        enabled: obj.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true),
        description: obj.get("description").and_then(|v| v.as_str()).map(|s| s.to_string()),
    })
}
```

- [ ] **Step 4: Declare the module**

Find where `mod skills;` is declared (check `src/lib.rs`). Add `pub mod mcp_scanner;` on the next line.

```bash
grep -n "mod skills" /Users/aicoder/Documents/ideation/gait/gait-rs/src/lib.rs
```

Add after `pub mod skills;`:
```rust
pub mod mcp_scanner;
```

- [ ] **Step 5: Run tests**

```bash
cargo test mcp_scanner 2>&1 > /tmp/gait-test-output.txt; echo "exit: $?"
grep "FAILED\|test result" /tmp/gait-test-output.txt
```

Expected: all 8 scanner tests pass.

- [ ] **Step 6: Commit**

```bash
git add src/mcp_scanner.rs src/lib.rs
git commit -m "feat: add mcp_scanner — scans mcp/<name>.json flat files"
```

---

## Task 3: Wire MCP scanner into `profile.rs`

`Profile::from_config_and_agent` currently reads MCP servers only from `config.mcp_servers`. Add disk scanning: disk files take precedence over `gait.jsonc` entries with the same name (same merge strategy as skills).

**Files:**
- Modify: `src/profile.rs`

- [ ] **Step 1: Add a failing test in `src/profile.rs`**

Add to the `#[cfg(test)]` block at the bottom of `src/profile.rs`:

```rust
#[test]
fn disk_mcp_populates_profile_mcp_servers() {
    use tempfile::TempDir;
    let dir = TempDir::new().unwrap();
    let mcp_dir = dir.path().join("mcp");
    std::fs::create_dir_all(&mcp_dir).unwrap();
    std::fs::write(
        mcp_dir.join("slack.json"),
        r#"{"command":"npx","args":["-y","@pkg/slack"],"enabled":true}"#,
    ).unwrap();
    let config = GaitConfig::default();
    let profile = Profile::from_config_and_agent(&config, dir.path(), None, None, None);
    assert_eq!(profile.mcp_servers.len(), 1);
    assert_eq!(profile.mcp_servers[0].name, "slack");
    assert_eq!(profile.mcp_servers[0].command.as_deref(), Some("npx"));
}

#[test]
fn disk_mcp_wins_over_config_mcp_same_name() {
    use tempfile::TempDir;
    let dir = TempDir::new().unwrap();
    let mcp_dir = dir.path().join("mcp");
    std::fs::create_dir_all(&mcp_dir).unwrap();
    std::fs::write(
        mcp_dir.join("slack.json"),
        r#"{"command":"npx","args":["disk-version"],"enabled":true}"#,
    ).unwrap();
    let config = GaitConfig {
        mcp_servers: vec![crate::config::McpServerDef {
            name: "slack".into(),
            command: None, args: None, env: None, url: None,
            spawn: Some("node config-version.js".into()),
            connect: None, enabled: true, description: None,
        }],
        ..GaitConfig::default()
    };
    let profile = Profile::from_config_and_agent(&config, dir.path(), None, None, None);
    assert_eq!(profile.mcp_servers.len(), 1, "should deduplicate");
    assert_eq!(profile.mcp_servers[0].command.as_deref(), Some("npx"), "disk wins");
}

#[test]
fn config_mcp_kept_when_no_disk_mcp() {
    use tempfile::TempDir;
    let dir = TempDir::new().unwrap();
    let config = GaitConfig {
        mcp_servers: vec![crate::config::McpServerDef {
            name: "remote".into(),
            command: None, args: None, env: None,
            url: Some("http://localhost/mcp".into()),
            spawn: None, connect: None, enabled: true, description: None,
        }],
        ..GaitConfig::default()
    };
    let profile = Profile::from_config_and_agent(&config, dir.path(), None, None, None);
    assert_eq!(profile.mcp_servers.len(), 1);
    assert_eq!(profile.mcp_servers[0].name, "remote");
}
```

- [ ] **Step 2: Run test to verify it fails**

```bash
cargo test disk_mcp 2>&1 > /tmp/gait-test-output.txt; echo "exit: $?"
grep "FAILED\|test result" /tmp/gait-test-output.txt
```

Expected: `disk_mcp_populates_profile_mcp_servers` fails — no disk scanning yet.

- [ ] **Step 3: Update `Profile::from_config_and_agent` to merge disk MCPs**

In `src/profile.rs`, find the line:

```rust
let config_enabled: HashSet<String> = config.mcp_servers.iter()
```

Insert the disk MCP merge block immediately above it:

```rust
// MCP: disk files first; gait.jsonc entries kept if not already on disk (by name).
let mut disk_mcp = crate::mcp_scanner::scan_mcp_servers(gait_dir);
let disk_mcp_names: std::collections::HashSet<String> =
    disk_mcp.iter().map(|s| s.name.clone()).collect();
for s in &config.mcp_servers {
    if !disk_mcp_names.contains(&s.name) {
        disk_mcp.push(s.clone());
    }
}
let merged_mcp = disk_mcp;
```

Then update the `config_enabled` line and the `Profile { ... }` construction to use `merged_mcp` instead of `config.mcp_servers`:

```rust
// Replace:
let config_enabled: HashSet<String> = config.mcp_servers.iter()
    .filter(|s| s.enabled).map(|s| s.name.clone())
    .chain(merged_skills.iter().filter(|s| s.enabled).map(|s| s.name.clone()))
    .collect();
// With:
let config_enabled: HashSet<String> = merged_mcp.iter()
    .filter(|s| s.enabled).map(|s| s.name.clone())
    .chain(merged_skills.iter().filter(|s| s.enabled).map(|s| s.name.clone()))
    .collect();
```

And in `Profile { ... }`:
```rust
// Replace:
mcp_servers: config.mcp_servers.clone(),
// With:
mcp_servers: merged_mcp,
```

- [ ] **Step 4: Run tests**

```bash
cargo test 2>&1 > /tmp/gait-test-output.txt; echo "exit: $?"
grep "FAILED\|test result" /tmp/gait-test-output.txt
```

Expected: all tests pass including the three new disk_mcp tests.

- [ ] **Step 5: Commit**

```bash
git add src/profile.rs
git commit -m "feat: profile merges disk mcp/<name>.json; gait.jsonc mcp_servers kept as fallback"
```

---

## Task 4: Update CLI — `gait register mcp` writes disk files; `enable/disable/unregister` prefer disk

`gait register mcp <name>` now creates `mcp/<name>.json` instead of editing `gait.jsonc`. `gait enable/disable mcp <name>` and `gait unregister mcp <name>` check for the disk file first and fall back to `gait.jsonc`.

The three-tier model for MCP:
- **Tier 1**: `gait register mcp <name>` → creates `mcp/<name>.json` with `enabled: false`
- **Tier 2**: `gait enable mcp <name>` → sets `enabled: true` in `mcp/<name>.json`
- **Tier 3**: `gait allow mcp:<name>:*` → adds execution pattern (existing mechanism, unchanged)

**Files:**
- Modify: `src/bin/gait.rs`

- [ ] **Step 1: Update `cmd_register` — MCP branch writes disk file**

In `src/bin/gait.rs`, find the `"mcp" =>` arm inside `cmd_register` (around line 1251). Replace it:

```rust
"mcp" => {
    // Tier 1: create mcp/<name>.json (disk file)
    let mcp_dir = gait_dir.join("mcp");
    std::fs::create_dir_all(&mcp_dir)
        .map_err(|e| anyhow::anyhow!("could not create mcp/ dir: {e}"))?;
    let disk_path = mcp_dir.join(format!("{}.json", name));
    if disk_path.exists() {
        return Err(anyhow::anyhow!(
            "mcp '{}' already registered at {}. Use 'gait unregister mcp {}' first.",
            name, disk_path.display(), name
        ));
    }
    // Build JSON content from flags
    let entry = if let Some(cmd) = flags.get("command") {
        // New ecosystem format: --command <prog> [--args "<arg1> <arg2>"]
        let args: Vec<serde_json::Value> = flags.get("args")
            .map(|s| s.split_whitespace().map(|a| serde_json::Value::String(a.to_string())).collect())
            .unwrap_or_default();
        serde_json::json!({ "command": cmd, "args": args, "enabled": false })
    } else if let Some(spawn) = flags.get("spawn") {
        // Compat: --spawn "npx -y @pkg" → convert to command+args on write
        let mut parts = spawn.split_whitespace();
        let prog = parts.next().unwrap_or("").to_string();
        let rest: Vec<serde_json::Value> = parts.map(|s| serde_json::Value::String(s.to_string())).collect();
        serde_json::json!({ "command": prog, "args": rest, "enabled": false })
    } else if let Some(url) = flags.get("url").or_else(|| flags.get("connect")) {
        serde_json::json!({ "url": url, "enabled": false })
    } else {
        return Err(anyhow::anyhow!(
            "gait register mcp: requires --command <prog> [--args \"<args>\"], --spawn <cmd>, or --url <url>"
        ));
    };
    if let Some(desc) = flags.get("description") {
        let mut obj = entry.as_object().unwrap().clone();
        obj.insert("description".into(), serde_json::Value::String(desc.clone()));
        std::fs::write(&disk_path, serde_json::to_string_pretty(&serde_json::Value::Object(obj))?)?;
    } else {
        std::fs::write(&disk_path, serde_json::to_string_pretty(&entry)?)?;
    }
    println!("Registered mcp '{}'.", name);
    println!("→ Run 'gait enable mcp {}' to activate it.", name);
    return Ok(());
}
```

- [ ] **Step 2: Update `cmd_enable_disable` to prefer disk MCP file**

In `cmd_enable_disable`, add disk-file handling for `"mcp"` before the existing `gait.jsonc` logic. Replace the whole function body:

```rust
fn cmd_enable_disable(gait_dir: &std::path::Path, args: &[String], enable: bool) -> anyhow::Result<()> {
    let kind = args.get(0).map(|s| s.as_str())
        .ok_or_else(|| anyhow::anyhow!("usage: gait enable/disable <mcp|skill|plugin> <name>"))?;
    let name = args.get(1).map(|s| s.as_str())
        .ok_or_else(|| anyhow::anyhow!("usage: gait enable/disable <mcp|skill|plugin> <name>"))?;

    // Disk-file path for mcp and plugin kinds
    let disk_path: Option<std::path::PathBuf> = match kind {
        "mcp" => {
            let p = gait_dir.join("mcp").join(format!("{}.json", name));
            if p.exists() { Some(p) } else { None }
        }
        "plugin" => {
            let p = gait_dir.join("plugins").join(name).join("PLUGIN.md");
            if p.exists() { Some(p) } else { None }
        }
        _ => None,
    };

    if let Some(path) = disk_path {
        match kind {
            "mcp" => {
                let mut val: serde_json::Value = serde_json::from_str(
                    &std::fs::read_to_string(&path)?)?;
                val["enabled"] = serde_json::Value::Bool(enable);
                std::fs::write(&path, serde_json::to_string_pretty(&val)?)?;
            }
            "plugin" => {
                let content = std::fs::read_to_string(&path)?;
                let new_content = set_frontmatter_bool(&content, "enabled", enable)?;
                std::fs::write(&path, new_content)?;
            }
            _ => unreachable!(),
        }
        eprintln!("{} {} '{}'.", if enable { "Enabled" } else { "Disabled" }, kind, name);
        return Ok(());
    }

    // Fall back to gait.jsonc for legacy mcp/skill entries
    let array_key = match kind {
        "mcp" => "mcp_servers",
        "skill" => "skills",
        other => return Err(anyhow::anyhow!("unknown kind '{}': use mcp, skill, or plugin", other)),
    };
    let config_path = gait_dir.join("gait.jsonc");
    let mut val = read_config_as_value(&config_path)?;
    let arr = val[array_key].as_array_mut()
        .ok_or_else(|| anyhow::anyhow!("{} is not an array", array_key))?;
    let entry = arr.iter_mut()
        .find(|v| v.get("name").and_then(|n| n.as_str()) == Some(name))
        .ok_or_else(|| anyhow::anyhow!("{} '{}' not found; run 'gait register {} {}' first", kind, name, kind, name))?;
    entry["enabled"] = serde_json::Value::Bool(enable);
    eprintln!("Note: gait.jsonc comments are stripped on rewrite.");
    std::fs::write(&config_path, serde_json::to_string_pretty(&val)?)?;
    eprintln!("{} {} '{}'.", if enable { "Enabled" } else { "Disabled" }, kind, name);
    Ok(())
}
```

- [ ] **Step 3: Add `set_frontmatter_bool` helper (needed by `cmd_enable_disable`)**

This helper is also used in Task 7 for plugins. Add it near the other helpers at the bottom of `src/bin/gait.rs` (before `#[cfg(test)]`):

```rust
/// Update a boolean key in YAML frontmatter of a markdown file, preserving the body.
fn set_frontmatter_bool(content: &str, key: &str, value: bool) -> anyhow::Result<String> {
    const FENCE: &str = "---";
    if !content.starts_with(FENCE) {
        return Err(anyhow::anyhow!("no frontmatter found"));
    }
    let rest = &content[FENCE.len()..];
    let rest = rest.strip_prefix('\n').unwrap_or(rest);
    let end = rest.find(&format!("\n{}", FENCE))
        .ok_or_else(|| anyhow::anyhow!("unclosed frontmatter"))?;
    let fm_str = &rest[..end];
    let body_start = end + 1 + FENCE.len();
    let body = rest[body_start..].strip_prefix('\n').unwrap_or(&rest[body_start..]);

    // Parse YAML, set the key, serialize back
    let mut yaml: serde_yaml::Value = serde_yaml::from_str(fm_str)
        .map_err(|e| anyhow::anyhow!("frontmatter parse error: {e}"))?;
    if let serde_yaml::Value::Mapping(ref mut map) = yaml {
        map.insert(
            serde_yaml::Value::String(key.to_string()),
            serde_yaml::Value::Bool(value),
        );
    }
    let new_fm = serde_yaml::to_string(&yaml)
        .map_err(|e| anyhow::anyhow!("frontmatter serialize error: {e}"))?;
    Ok(format!("---\n{}---\n{}", new_fm, body))
}
```

- [ ] **Step 4: Update `cmd_unregister` to prefer disk file for mcp**

Replace the `cmd_unregister` function body:

```rust
fn cmd_unregister(gait_dir: &std::path::Path, args: &[String]) -> anyhow::Result<()> {
    let kind = args.get(0).map(|s| s.as_str())
        .ok_or_else(|| anyhow::anyhow!("usage: gait unregister <mcp|skill|plugin> <name>"))?;
    let name = args.get(1).map(|s| s.as_str())
        .ok_or_else(|| anyhow::anyhow!("usage: gait unregister <mcp|skill|plugin> <name>"))?;

    // Disk-first for mcp and plugin
    if kind == "mcp" {
        let disk_path = gait_dir.join("mcp").join(format!("{}.json", name));
        if disk_path.exists() {
            std::fs::remove_file(&disk_path)?;
            eprintln!("Removed mcp '{}'.", name);
            return Ok(());
        }
    }
    if kind == "plugin" {
        let plugin_dir = gait_dir.join("plugins").join(name);
        if plugin_dir.exists() {
            std::fs::remove_dir_all(&plugin_dir)?;
            eprintln!("Removed plugin '{}'.", name);
            return Ok(());
        }
        return Err(anyhow::anyhow!("plugin '{}' not found", name));
    }

    // Fall back to gait.jsonc
    let array_key = match kind {
        "mcp" => "mcp_servers",
        "skill" => "skills",
        other => return Err(anyhow::anyhow!("unknown kind '{}': use mcp, skill, or plugin", other)),
    };
    let config_path = gait_dir.join("gait.jsonc");
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

- [ ] **Step 5: Add tests for new MCP disk-file register/enable/unregister behavior**

Add to the `#[cfg(test)]` block in `src/bin/gait.rs`:

```rust
#[test]
fn cmd_register_mcp_writes_disk_file() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("gait.jsonc"),
        r#"{"model":"anthropic/claude-sonnet-4-6"}"#).unwrap();
    let args: Vec<String> = ["mcp", "slack", "--command", "npx", "--args", "-y @pkg/slack"]
        .iter().map(|s| s.to_string()).collect();
    cmd_register(dir.path(), &args).unwrap();
    let disk_path = dir.path().join("mcp").join("slack.json");
    assert!(disk_path.exists(), "mcp/slack.json should be created");
    let val: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(&disk_path).unwrap()).unwrap();
    assert_eq!(val["command"], "npx");
    assert_eq!(val["enabled"], false);
}

#[test]
fn cmd_register_mcp_spawn_converts_to_command_args() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("gait.jsonc"),
        r#"{"model":"anthropic/claude-sonnet-4-6"}"#).unwrap();
    let args: Vec<String> = ["mcp", "slack2", "--spawn", "node mcp-slack.js"]
        .iter().map(|s| s.to_string()).collect();
    cmd_register(dir.path(), &args).unwrap();
    let val: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(dir.path().join("mcp").join("slack2.json")).unwrap()).unwrap();
    assert_eq!(val["command"], "node");
    assert_eq!(val["args"][0], "mcp-slack.js");
}

#[test]
fn cmd_enable_mcp_prefers_disk_file() {
    let dir = tempfile::tempdir().unwrap();
    let mcp_dir = dir.path().join("mcp");
    std::fs::create_dir_all(&mcp_dir).unwrap();
    std::fs::write(mcp_dir.join("slack.json"),
        r#"{"command":"npx","args":[],"enabled":false}"#).unwrap();
    let args: Vec<String> = ["mcp", "slack"].iter().map(|s| s.to_string()).collect();
    cmd_enable_disable(dir.path(), &args, true).unwrap();
    let val: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(mcp_dir.join("slack.json")).unwrap()).unwrap();
    assert_eq!(val["enabled"], true);
}

#[test]
fn cmd_unregister_mcp_removes_disk_file() {
    let dir = tempfile::tempdir().unwrap();
    let mcp_dir = dir.path().join("mcp");
    std::fs::create_dir_all(&mcp_dir).unwrap();
    let disk_path = mcp_dir.join("slack.json");
    std::fs::write(&disk_path, r#"{"command":"npx","args":[],"enabled":false}"#).unwrap();
    let args: Vec<String> = ["mcp", "slack"].iter().map(|s| s.to_string()).collect();
    cmd_unregister(dir.path(), &args).unwrap();
    assert!(!disk_path.exists(), "mcp/slack.json should be deleted");
}
```

- [ ] **Step 6: Add `serde_yaml` to Cargo.toml if not already present**

```bash
grep "serde_yaml" /Users/aicoder/Documents/ideation/gait/gait-rs/Cargo.toml
```

If not present:
```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo add serde_yaml
```

(If already present, skip.)

- [ ] **Step 7: Run all tests**

```bash
cargo test 2>&1 > /tmp/gait-test-output.txt; echo "exit: $?"
grep "FAILED\|test result" /tmp/gait-test-output.txt
```

Expected: all tests pass.

- [ ] **Step 8: Commit**

```bash
git add src/bin/gait.rs Cargo.toml Cargo.lock
git commit -m "feat: gait register/enable/disable/unregister mcp uses mcp/<name>.json disk files"
```

---

## Task 5: Create `src/wasm_plugins.rs`

Scan `<gait_dir>/plugins/<name>/PLUGIN.md` files. Each directory is one plugin; `PLUGIN.md` has YAML frontmatter with `name`, `wasm`, `enabled`, `allow_exec`, `allow_fs` and an optional markdown body (model-visible docs). Mirrors `src/skills.rs`.

**Files:**
- Create: `src/wasm_plugins.rs`
- Modify: `src/lib.rs`

- [ ] **Step 1: Write failing tests first**

Create `src/wasm_plugins.rs` with only the test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_plugin(dir: &TempDir, name: &str, fm: &str, body: &str) {
        let plugin_dir = dir.path().join("plugins").join(name);
        std::fs::create_dir_all(&plugin_dir).unwrap();
        // Create a dummy .wasm file so path resolution works
        std::fs::write(plugin_dir.join(format!("{}.wasm", name)), b"").unwrap();
        std::fs::write(
            plugin_dir.join("PLUGIN.md"),
            format!("---\n{}---\n{}", fm, body),
        ).unwrap();
    }

    #[test]
    fn scanner_returns_empty_when_no_plugins_dir() {
        let dir = TempDir::new().unwrap();
        assert!(scan_plugins(dir.path()).is_empty());
    }

    #[test]
    fn scanner_reads_one_plugin() {
        let dir = TempDir::new().unwrap();
        write_plugin(&dir, "formatter",
            "name: formatter\nwasm: formatter.wasm\nenabled: false\nallow_exec: []\nallow_fs: [\"read:src/\"]\n",
            "# Formatter\nFormats code.\n");
        let plugins = scan_plugins(dir.path());
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].name, "formatter");
        assert!(plugins[0].path.ends_with("formatter.wasm"), "path: {}", plugins[0].path);
        assert!(!plugins[0].enabled);
        assert_eq!(plugins[0].allow_fs, vec!["read:src/"]);
        assert!(plugins[0].allow_exec.is_empty());
    }

    #[test]
    fn scanner_skips_dir_without_plugin_md() {
        let dir = TempDir::new().unwrap();
        let plugin_dir = dir.path().join("plugins").join("orphan");
        std::fs::create_dir_all(&plugin_dir).unwrap();
        std::fs::write(plugin_dir.join("README.md"), "hi").unwrap();
        assert!(scan_plugins(dir.path()).is_empty());
    }

    #[test]
    fn scanner_skips_plugin_with_no_name() {
        let dir = TempDir::new().unwrap();
        write_plugin(&dir, "bad",
            "wasm: bad.wasm\nenabled: false\n",
            "body\n");
        assert!(scan_plugins(dir.path()).is_empty());
    }

    #[test]
    fn scanner_skips_plugin_with_no_wasm_field() {
        let dir = TempDir::new().unwrap();
        write_plugin(&dir, "nowasm",
            "name: nowasm\nenabled: false\n",
            "body\n");
        assert!(scan_plugins(dir.path()).is_empty());
    }

    #[test]
    fn enabled_defaults_to_false_when_absent() {
        let dir = TempDir::new().unwrap();
        write_plugin(&dir, "plug",
            "name: plug\nwasm: plug.wasm\n",
            "body\n");
        let plugins = scan_plugins(dir.path());
        assert!(!plugins[0].enabled, "plugins default to disabled");
    }

    #[test]
    fn allow_exec_and_fs_default_to_empty() {
        let dir = TempDir::new().unwrap();
        write_plugin(&dir, "minimal",
            "name: minimal\nwasm: minimal.wasm\nenabled: false\n",
            "# Minimal\n");
        let plugins = scan_plugins(dir.path());
        assert!(plugins[0].allow_exec.is_empty());
        assert!(plugins[0].allow_fs.is_empty());
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

```bash
cargo test wasm_plugins 2>&1 > /tmp/gait-test-output.txt; echo "exit: $?"
grep "^error" /tmp/gait-test-output.txt | head -20
```

Expected: compile error — `scan_plugins` not found.

- [ ] **Step 3: Implement `scan_plugins`**

Add full implementation above the test module in `src/wasm_plugins.rs`:

```rust
use std::path::{Path, PathBuf};
use crate::config::WasmPluginDef;
use crate::agent_frontmatter::split_frontmatter;

/// Scan `<gait_dir>/plugins/<name>/PLUGIN.md` and return one `WasmPluginDef` per valid file.
///
/// A file is valid if it has a non-empty `name` and `wasm` key in its YAML frontmatter.
/// The `wasm` field is a path relative to the plugin directory; the scanner resolves it
/// to an absolute path and stores that in `WasmPluginDef.path`.
/// `enabled` defaults to `false` (plugins start disabled until explicitly enabled).
pub fn scan_plugins(gait_dir: &Path) -> Vec<WasmPluginDef> {
    let plugins_dir = gait_dir.join("plugins");
    if !plugins_dir.is_dir() { return vec![]; }

    let mut out = Vec::new();
    let entries = match std::fs::read_dir(&plugins_dir) {
        Ok(e) => e,
        Err(err) => {
            eprintln!("[gait] warning: could not read plugins dir {}: {err}", plugins_dir.display());
            return vec![];
        }
    };
    for entry in entries.flatten() {
        let plugin_dir = entry.path();
        if !plugin_dir.is_dir() { continue; }
        let plugin_md = plugin_dir.join("PLUGIN.md");
        if !plugin_md.is_file() { continue; }
        match parse_plugin_file(&plugin_md, &plugin_dir) {
            Some(def) => out.push(def),
            None => eprintln!("[gait] warning: skipping invalid plugin file {}", plugin_md.display()),
        }
    }
    out
}

fn parse_plugin_file(path: &Path, plugin_dir: &PathBuf) -> Option<WasmPluginDef> {
    let content = std::fs::read_to_string(path).ok()?;
    let (fm, _body) = split_frontmatter(&content);
    if fm.is_empty() { return None; }
    let yaml: serde_yaml::Value = serde_yaml::from_str(fm).ok()?;
    let name = yaml.get("name").and_then(|v| v.as_str()).filter(|s| !s.is_empty())?.to_string();
    let wasm_rel = yaml.get("wasm").and_then(|v| v.as_str()).filter(|s| !s.is_empty())?;
    let wasm_abs = plugin_dir.join(wasm_rel);
    let allow_exec = yaml.get("allow_exec")
        .and_then(|v| v.as_sequence())
        .map(|seq| seq.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
        .unwrap_or_default();
    let allow_fs = yaml.get("allow_fs")
        .and_then(|v| v.as_sequence())
        .map(|seq| seq.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
        .unwrap_or_default();
    Some(WasmPluginDef {
        name,
        path: wasm_abs.to_string_lossy().to_string(),
        enabled: yaml.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false),
        allow_exec,
        allow_fs,
    })
}
```

- [ ] **Step 4: Declare the module in `src/lib.rs`**

Add after `pub mod mcp_scanner;`:
```rust
pub mod wasm_plugins;
```

- [ ] **Step 5: Run tests**

```bash
cargo test wasm_plugins 2>&1 > /tmp/gait-test-output.txt; echo "exit: $?"
grep "FAILED\|test result" /tmp/gait-test-output.txt
```

Expected: all 7 plugin scanner tests pass.

- [ ] **Step 6: Commit**

```bash
git add src/wasm_plugins.rs src/lib.rs
git commit -m "feat: add wasm_plugins scanner — scans plugins/<name>/PLUGIN.md"
```

---

## Task 6: Wire plugin scanner into `profile.rs`

Add `wasm_plugins: Vec<WasmPluginDef>` to `Profile`, scan them in `from_config_and_agent`, and include enabled plugins in `config_enabled`. WASM execution remains stubbed — the scanner just populates the registry so plugins appear in `config_enabled` and the loadable index.

**Files:**
- Modify: `src/profile.rs`

- [ ] **Step 1: Add `wasm_plugins` field to `Profile` struct**

In `src/profile.rs`, find the `pub struct Profile` definition and add:

```rust
pub wasm_plugins: Vec<crate::config::WasmPluginDef>,
```

after the `hooks` field.

- [ ] **Step 2: Write failing test**

Add to `#[cfg(test)]` in `src/profile.rs`:

```rust
#[test]
fn disk_plugins_populate_profile() {
    use tempfile::TempDir;
    let dir = TempDir::new().unwrap();
    let plugin_dir = dir.path().join("plugins").join("formatter");
    std::fs::create_dir_all(&plugin_dir).unwrap();
    std::fs::write(plugin_dir.join("formatter.wasm"), b"").unwrap();
    std::fs::write(
        plugin_dir.join("PLUGIN.md"),
        "---\nname: formatter\nwasm: formatter.wasm\nenabled: true\n---\n# Formatter\n",
    ).unwrap();
    let config = GaitConfig::default();
    let profile = Profile::from_config_and_agent(&config, dir.path(), None, None, None);
    assert_eq!(profile.wasm_plugins.len(), 1);
    assert_eq!(profile.wasm_plugins[0].name, "formatter");
    assert!(profile.config_enabled.contains("formatter"));
}
```

- [ ] **Step 3: Run test to verify it fails**

```bash
cargo test disk_plugins_populate 2>&1 > /tmp/gait-test-output.txt; echo "exit: $?"
grep "FAILED\|test result" /tmp/gait-test-output.txt
```

Expected: compile error or test failure — `wasm_plugins` field not populated.

- [ ] **Step 4: Wire plugin scanner in `from_config_and_agent`**

In `src/profile.rs`, inside `from_config_and_agent`, add plugin scanning before the `config_enabled` line:

```rust
// Plugins: scan disk
let disk_plugins = crate::wasm_plugins::scan_plugins(gait_dir);
```

Update `config_enabled` to include enabled plugins:

```rust
let config_enabled: HashSet<String> = merged_mcp.iter()
    .filter(|s| s.enabled).map(|s| s.name.clone())
    .chain(merged_skills.iter().filter(|s| s.enabled).map(|s| s.name.clone()))
    .chain(disk_plugins.iter().filter(|p| p.enabled).map(|p| p.name.clone()))
    .collect();
```

Add `wasm_plugins: disk_plugins,` to the `Profile { ... }` constructor.

- [ ] **Step 5: Fix compile errors from missing field**

Any place that constructs `Profile { ... }` in tests (e.g., the `resolve_with_no_mcp_builds_provider_and_builtin_tools` test) will need `wasm_plugins: vec![]` added.

Find all Profile struct literals:
```bash
grep -n "Profile {" /Users/aicoder/Documents/ideation/gait/gait-rs/src/profile.rs
```

Add `wasm_plugins: vec![],` to each.

- [ ] **Step 6: Run all tests**

```bash
cargo test 2>&1 > /tmp/gait-test-output.txt; echo "exit: $?"
grep "FAILED\|test result" /tmp/gait-test-output.txt
```

Expected: all tests pass.

- [ ] **Step 7: Commit**

```bash
git add src/profile.rs
git commit -m "feat: profile scans plugins/<name>/PLUGIN.md; enabled plugins in config_enabled"
```

---

## Task 7: Add `gait register plugin` + `enable/disable/unregister plugin`

Complete the three-tier CLI for plugins:
- **Tier 1**: `gait register plugin <name> --wasm <path>` → creates `plugins/<name>/PLUGIN.md` scaffold and copies the `.wasm` file
- **Tier 2**: `gait enable plugin <name>` / `gait disable plugin <name>` → sets `enabled` in `PLUGIN.md` frontmatter (already handled by `cmd_enable_disable` from Task 4)
- **Tier 3**: `gait allow plugin:<name>:*` → existing mechanism, unchanged

**Files:**
- Modify: `src/bin/gait.rs`

- [ ] **Step 1: Add `"plugin"` branch to `cmd_register`**

In `cmd_register`, after the `"skill" =>` arm and before the `other =>` arm, add:

```rust
"plugin" => {
    // Tier 1: create plugins/<name>/ directory + PLUGIN.md scaffold
    let wasm_src = flags.get("wasm")
        .ok_or_else(|| anyhow::anyhow!("gait register plugin: requires --wasm <path>"))?;
    let wasm_src_path = std::path::Path::new(wasm_src);
    if !wasm_src_path.exists() {
        return Err(anyhow::anyhow!("wasm file not found: {}", wasm_src));
    }
    let plugin_dir = gait_dir.join("plugins").join(name);
    if plugin_dir.exists() {
        return Err(anyhow::anyhow!(
            "plugin '{}' already registered at {}. Use 'gait unregister plugin {}' first.",
            name, plugin_dir.display(), name
        ));
    }
    std::fs::create_dir_all(&plugin_dir)
        .map_err(|e| anyhow::anyhow!("could not create plugins/{} dir: {e}", name))?;
    // Copy the .wasm binary into the plugin directory
    let wasm_filename = wasm_src_path.file_name()
        .ok_or_else(|| anyhow::anyhow!("invalid wasm path: {}", wasm_src))?;
    let wasm_dest = plugin_dir.join(wasm_filename);
    std::fs::copy(wasm_src_path, &wasm_dest)
        .map_err(|e| anyhow::anyhow!("could not copy wasm file: {e}"))?;
    let wasm_rel = wasm_filename.to_string_lossy();
    let description = flags.get("description").map(|s| s.as_str()).unwrap_or("No description.");
    let plugin_md = plugin_dir.join("PLUGIN.md");
    std::fs::write(&plugin_md, format!(
        "---\nname: {}\nwasm: {}\nenabled: false\nallow_exec: []\nallow_fs: []\n---\n\n# {}\n\n{}\n",
        name, wasm_rel, name, description
    ))?;
    println!("Registered plugin '{}'.", name);
    println!("→ Run 'gait enable plugin {}' to activate it.", name);
    return Ok(());
}
```

- [ ] **Step 2: Add tests for plugin register/enable/unregister**

Add to the `#[cfg(test)]` block in `src/bin/gait.rs`:

```rust
#[test]
fn cmd_register_plugin_creates_directory_and_md() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("gait.jsonc"),
        r#"{"model":"anthropic/claude-sonnet-4-6"}"#).unwrap();
    // Create a dummy .wasm file to register
    let wasm_path = dir.path().join("myfmt.wasm");
    std::fs::write(&wasm_path, b"").unwrap();
    let args: Vec<String> = ["plugin", "myfmt", "--wasm",
        wasm_path.to_str().unwrap(), "--description", "A formatter"]
        .iter().map(|s| s.to_string()).collect();
    cmd_register(dir.path(), &args).unwrap();
    let plugin_dir = dir.path().join("plugins").join("myfmt");
    assert!(plugin_dir.is_dir());
    assert!(plugin_dir.join("myfmt.wasm").exists(), "wasm should be copied");
    let md = std::fs::read_to_string(plugin_dir.join("PLUGIN.md")).unwrap();
    assert!(md.contains("name: myfmt"));
    assert!(md.contains("enabled: false"));
    assert!(md.contains("A formatter"));
}

#[test]
fn cmd_enable_plugin_sets_flag_in_plugin_md() {
    let dir = tempfile::tempdir().unwrap();
    let plugin_dir = dir.path().join("plugins").join("myfmt");
    std::fs::create_dir_all(&plugin_dir).unwrap();
    std::fs::write(plugin_dir.join("PLUGIN.md"),
        "---\nname: myfmt\nwasm: myfmt.wasm\nenabled: false\n---\n# docs\n").unwrap();
    let args: Vec<String> = ["plugin", "myfmt"].iter().map(|s| s.to_string()).collect();
    cmd_enable_disable(dir.path(), &args, true).unwrap();
    let md = std::fs::read_to_string(plugin_dir.join("PLUGIN.md")).unwrap();
    assert!(md.contains("enabled: true"), "got: {md}");
    assert!(!md.contains("enabled: false"), "old value should be gone: {md}");
}

#[test]
fn cmd_unregister_plugin_removes_directory() {
    let dir = tempfile::tempdir().unwrap();
    let plugin_dir = dir.path().join("plugins").join("myfmt");
    std::fs::create_dir_all(&plugin_dir).unwrap();
    std::fs::write(plugin_dir.join("PLUGIN.md"),
        "---\nname: myfmt\nwasm: myfmt.wasm\nenabled: false\n---\n").unwrap();
    let args: Vec<String> = ["plugin", "myfmt"].iter().map(|s| s.to_string()).collect();
    cmd_unregister(dir.path(), &args).unwrap();
    assert!(!plugin_dir.exists(), "plugins/myfmt/ should be deleted");
}

#[test]
fn set_frontmatter_bool_updates_enabled() {
    let content = "---\nname: foo\nenabled: false\n---\n# body\ncontent\n";
    let result = set_frontmatter_bool(content, "enabled", true).unwrap();
    assert!(result.contains("enabled: true"), "got: {result}");
    assert!(result.contains("# body"), "body should be preserved");
}
```

- [ ] **Step 3: Run all tests**

```bash
cargo test 2>&1 > /tmp/gait-test-output.txt; echo "exit: $?"
grep "FAILED\|test result" /tmp/gait-test-output.txt
```

Expected: all tests pass.

- [ ] **Step 4: Make install and smoke test**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && make install
gait register mcp testserver --command npx --args "-y @modelcontextprotocol/server-filesystem ." --gait-dir /Users/aicoder/Documents/gait-trace
ls /Users/aicoder/Documents/gait-trace/mcp/
cat /Users/aicoder/Documents/gait-trace/mcp/testserver.json
gait enable mcp testserver --gait-dir /Users/aicoder/Documents/gait-trace
cat /Users/aicoder/Documents/gait-trace/mcp/testserver.json
gait unregister mcp testserver --gait-dir /Users/aicoder/Documents/gait-trace
ls /Users/aicoder/Documents/gait-trace/mcp/
```

Expected: register creates the file, enable sets `"enabled": true`, unregister removes the file.

- [ ] **Step 5: Commit**

```bash
git add src/bin/gait.rs
git commit -m "feat: gait register/enable/disable/unregister plugin; three-tier CLI for WASM plugins"
```

---

## Open questions

1. **WASM execution** — `WasmPluginDef` is still a stub; no runtime executes the `.wasm`. This plan only adds the registry/scanner/CLI layer. WASM execution is a separate future task.
2. **MCP `headers`** — The `McpServerDef` struct does not include a `headers` field for HTTP authentication; `StreamableHttpClientTransport` would need to support it. Deferred.
3. **Deprecation warning for `gait.jsonc` `mcp_servers`** — Not added in this plan. Once most users migrate to disk files, add a warning at `load_config` time (same as the `skills` deprecation warning on line 163 of `src/config.rs`).

---
status: done
---

# gait-rs Phase 4a: CLI Commands Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add the five missing CLI commands (`init`, `finish`, `sessions`, `auth`, `models`) and config-defined tool wrappers to the `gait` Rust binary, bringing it to feature parity with the TypeScript CLI for the core workspace management workflow.

**Architecture:** Each command is a pure function `cmd_<name>(argv: &[String])` added to `src/bin/gait.rs`. New git operations (`finish_session`, `init_workspace`) go in `src/git.rs`. Auth write/remove operations go in `src/auth.rs`. The `models` command fetches from `https://models.dev/api.json` with a 1-hour file cache. Config-defined tools are instantiated in `src/bin/gait.rs` from the already-parsed `GaitConfig.tools` vec.

**Tech Stack:** Rust, git2, reqwest (already in Cargo.toml), serde_json, dirs-next, tokio, tempfile (tests)

---

## File Map

| File | Change |
|------|--------|
| `gait-rs/src/bin/gait.rs` | Add `init`, `finish`, `sessions`, `auth`, `models` dispatch + config-tool wiring |
| `gait-rs/src/git.rs` | Add `finish_session`, `init_workspace` |
| `gait-rs/src/auth.rs` | Add `write_api_key`, `remove_api_key`, `list_credentials` |
| `gait-rs/src/models.rs` | New file: `fetch_models`, `get_models` with 1-hour cache |
| `gait-rs/src/lib.rs` | Export `models` module |
| `gait-rs/tests/integration_commands.rs` | New file: integration tests for all commands |

---

## Task 1: `finish_session` + `init_workspace` in git.rs

The `finish` command needs to merge the session branch into main. The `init` command needs to set up a git repo. Neither exists in `src/git.rs` yet.

**Files:**
- Modify: `gait-rs/src/git.rs`
- Test: `gait-rs/src/git.rs` (inline `#[cfg(test)]` block — already exists)

- [x] **Step 1: Write the failing tests**

Add to the `#[cfg(test)]` block at the bottom of `gait-rs/src/git.rs`:

```rust
#[test]
fn finish_session_merges_into_main() {
    let ws = TempDir::new().unwrap();
    init_repo(ws.path());
    create_session_worktree(ws.path(), "sfin001").unwrap();
    // Write a file in the session so there's something to merge
    std::fs::write(ws.path().join("sessions/sfin001/result.txt"), "done").unwrap();
    commit_session(ws.path(), "sfin001", "session(sfin001): turn 1").unwrap();
    finish_session(ws.path(), "sfin001").unwrap();
    // After finish: session branch should be merged into main
    let repo = git2::Repository::open(ws.path()).unwrap();
    let sessions = list_active_sessions(ws.path()).unwrap();
    assert!(!sessions.contains(&"sfin001".to_string()), "should be merged: {:?}", sessions);
    // The merge commit should mention the session id
    let msg = repo.head().unwrap().peel_to_commit().unwrap().message().unwrap().to_string();
    assert!(msg.contains("sfin001"), "expected merge commit msg to contain session id, got: {msg}");
}

#[test]
fn init_workspace_creates_git_repo() {
    let dir = TempDir::new().unwrap();
    init_workspace(dir.path()).unwrap();
    assert!(dir.path().join(".git").exists());
    // Idempotent — calling again does not error
    init_workspace(dir.path()).unwrap();
}

#[test]
fn finish_session_noop_when_not_found() {
    let ws = TempDir::new().unwrap();
    init_repo(ws.path());
    // Should not error for a non-existent session
    finish_session(ws.path(), "snonexistent").unwrap();
}
```

- [x] **Step 2: Run to verify they fail**

```bash
cargo test -p gait-ai git::tests::finish_session_merges_into_main git::tests::init_workspace_creates_git_repo git::tests::finish_session_noop_when_not_found 2>&1 | tail -15
```

Expected: `error[E0425]: cannot find function 'finish_session'` (or similar compile error)

- [x] **Step 3: Implement `finish_session` and `init_workspace`**

Add to `gait-rs/src/git.rs` (after the existing `list_active_sessions` function, before `#[cfg(test)]`):

```rust
pub fn finish_session(workspace: &Path, session_id: &str) -> anyhow::Result<()> {
    let repo = Repository::open(workspace)?;
    let branch_name = format!("session/{}", session_id);

    // If no such branch, nothing to do
    if repo.find_branch(&branch_name, BranchType::Local).is_err() {
        return Ok(());
    }

    // Remove the worktree directory if it exists (required before branch ops).
    // Prune worktree registration first (while dir still exists), then remove the dir.
    let session_dir = workspace.join("sessions").join(session_id);
    if session_dir.exists() {
        if let Ok(wt) = repo.find_worktree(session_id) {
            let mut o = git2::WorktreePruneOptions::new();
            o.valid(true);
            let _ = wt.prune(Some(&mut o));
        }
        let _ = std::fs::remove_dir_all(&session_dir);
    }

    // Merge session branch into HEAD
    let branch_ref = repo.find_branch(&branch_name, BranchType::Local)?
        .into_reference();
    let branch_commit = branch_ref.peel_to_commit()?;
    let annotated = repo.find_annotated_commit(branch_commit.id())?;

    let mut merge_opts = git2::MergeOptions::new();
    let mut checkout_opts = git2::build::CheckoutBuilder::new();
    repo.merge(&[&annotated], Some(&mut merge_opts), Some(&mut checkout_opts))?;

    // Commit the merge
    let sig = repo.signature()
        .or_else(|_| Signature::now("gait", "gait@localhost"))?;
    let mut index = repo.index()?;
    if index.has_conflicts() {
        repo.cleanup_state()?;
        anyhow::bail!("session {} has merge conflicts with main; resolve manually", session_id);
    }
    index.write()?;
    let tree_id = index.write_tree()?;
    let tree = repo.find_tree(tree_id)?;
    let head_commit = repo.head()?.peel_to_commit()?;
    repo.commit(
        Some("HEAD"),
        &sig, &sig,
        &format!("session({}): finish", session_id),
        &tree,
        &[&head_commit, &branch_commit],
    )?;
    repo.cleanup_state()?;

    // Delete the session branch
    let mut b = repo.find_branch(&branch_name, BranchType::Local)?;
    b.delete()?;

    Ok(())
}

pub fn init_workspace(workspace: &Path) -> anyhow::Result<()> {
    // git init — no-op if already a repo
    match Repository::init(workspace) {
        Ok(_) | Err(_) => {}
    }
    Ok(())
}
```

- [x] **Step 4: Run tests to verify they pass**

```bash
cargo test -p gait-ai git::tests::finish_session_merges_into_main git::tests::init_workspace_creates_git_repo git::tests::finish_session_noop_when_not_found 2>&1 | tail -10
```

Expected: `test result: ok. 3 passed`

- [x] **Step 5: Run all tests to check no regressions**

```bash
cargo test 2>&1 | grep -E "^(test result|FAILED)"
```

Expected: all lines show `ok`, no `FAILED`

- [x] **Step 6: Commit**

```bash
git add gait-rs/src/git.rs
git commit -m "feat(rs): finish_session + init_workspace git ops"
```

---

## Task 2: Auth write/remove/list in auth.rs

The `auth add`, `auth remove`, and `auth list` commands need to write to and read from `~/.local/share/gait/auth.json`. The existing `auth.rs` only has `resolve_api_key` (read-only). We need `write_api_key`, `remove_api_key`, and `list_credentials`.

**Files:**
- Modify: `gait-rs/src/auth.rs`

- [x] **Step 1: Write the failing tests**

Add to the `#[cfg(test)]` block in `gait-rs/src/auth.rs`:

```rust
#[test]
fn write_and_read_api_key() {
    let dir = tempfile::TempDir::new().unwrap();
    let auth_path = dir.path().join("auth.json");
    write_api_key_to(&auth_path, "anthropic", "sk-test-123").unwrap();
    let list = list_credentials_from(&auth_path);
    assert_eq!(list.get("anthropic").map(|s| s.as_str()), Some("sk-test-..."));
}

#[test]
fn remove_api_key_removes_entry() {
    let dir = tempfile::TempDir::new().unwrap();
    let auth_path = dir.path().join("auth.json");
    write_api_key_to(&auth_path, "openai", "sk-abc").unwrap();
    write_api_key_to(&auth_path, "anthropic", "sk-def").unwrap();
    remove_api_key_from(&auth_path, "openai").unwrap();
    let list = list_credentials_from(&auth_path);
    assert!(!list.contains_key("openai"), "openai should be removed");
    assert!(list.contains_key("anthropic"), "anthropic should remain");
}

#[test]
fn list_credentials_masks_keys() {
    let dir = tempfile::TempDir::new().unwrap();
    let auth_path = dir.path().join("auth.json");
    write_api_key_to(&auth_path, "minimax", "sk-abcdefghijklmnop").unwrap();
    let list = list_credentials_from(&auth_path);
    let masked = list.get("minimax").unwrap();
    assert!(masked.ends_with("..."), "should be masked: {masked}");
    assert!(!masked.contains("abcdefghijklmnop"), "full key should not appear");
}
```

- [x] **Step 2: Run to verify they fail**

```bash
cargo test -p gait-ai auth::tests 2>&1 | tail -10
```

Expected: compile error — `write_api_key_to`, `list_credentials_from`, `remove_api_key_from` not found.

- [x] **Step 3: Implement the auth write/remove/list functions**

Add to `gait-rs/src/auth.rs` (after the existing `resolve_api_key` function, before `#[cfg(test)]`):

```rust
fn auth_path() -> std::path::PathBuf {
    dirs_next::data_local_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("~/.local/share"))
        .join("gait")
        .join("auth.json")
}

fn read_raw_auth(path: &std::path::Path) -> HashMap<String, serde_json::Value> {
    if !path.exists() { return HashMap::new(); }
    let text = std::fs::read_to_string(path).unwrap_or_default();
    serde_json::from_str(&text).unwrap_or_default()
}

pub fn write_api_key_to(path: &std::path::Path, provider: &str, key: &str) -> anyhow::Result<()> {
    std::fs::create_dir_all(path.parent().unwrap())?;
    let mut data = read_raw_auth(path);
    data.insert(provider.to_string(), serde_json::json!({ "type": "api", "key": key }));
    std::fs::write(path, serde_json::to_string_pretty(&data)?)?;
    Ok(())
}

pub fn write_api_key(provider: &str, key: &str) -> anyhow::Result<()> {
    write_api_key_to(&auth_path(), provider, key)
}

pub fn remove_api_key_from(path: &std::path::Path, provider: &str) -> anyhow::Result<()> {
    let mut data = read_raw_auth(path);
    data.remove(provider);
    std::fs::write(path, serde_json::to_string_pretty(&data)?)?;
    Ok(())
}

pub fn remove_api_key(provider: &str) -> anyhow::Result<()> {
    remove_api_key_from(&auth_path(), provider)
}

/// Returns provider → masked_display string for `auth list`.
pub fn list_credentials_from(path: &std::path::Path) -> HashMap<String, String> {
    read_raw_auth(path).into_iter().filter_map(|(k, v)| {
        let display = match v.get("type").and_then(|t| t.as_str()) {
            Some("api") => {
                let key = v.get("key").and_then(|k| k.as_str()).unwrap_or("");
                // chars().take(8) avoids panics on non-ASCII (API keys are ASCII in practice,
                // but this is safer than &key[..n] byte-slicing)
                let prefix: String = key.chars().take(8).collect();
                format!("{}...", prefix)
            }
            Some("oauth") => "oauth".to_string(),
            Some("wellknown") => "wellknown".to_string(),
            other => other.unwrap_or("unknown").to_string(),
        };
        Some((k, display))
    }).collect()
}

pub fn list_credentials() -> HashMap<String, String> {
    list_credentials_from(&auth_path())
}
```

- [x] **Step 4: Run the auth tests**

```bash
cargo test -p gait-ai auth::tests 2>&1 | tail -10
```

Expected: `test result: ok. 4 passed` (3 new + 1 existing)

- [x] **Step 5: Run all tests**

```bash
cargo test 2>&1 | grep -E "^(test result|FAILED)"
```

Expected: all `ok`

- [x] **Step 6: Commit**

```bash
git add gait-rs/src/auth.rs
git commit -m "feat(rs): auth write/remove/list operations"
```

---

## Task 3: `models.rs` — fetch + 1-hour cache

The `models` command fetches from `https://models.dev/api.json`, caches to `~/.cache/gait/models.json` for 1 hour, and falls back to stale cache if offline.

**Files:**
- Create: `gait-rs/src/models.rs`
- Modify: `gait-rs/src/lib.rs` (add `pub mod models;`)

- [x] **Step 1: Write the failing tests**

Create `gait-rs/src/models.rs` with just the test module first:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn get_models_from_cache() {
        let dir = TempDir::new().unwrap();
        let cache = dir.path().join("models.json");
        let data = serde_json::json!([
            { "id": "claude-3-5-sonnet", "name": "Claude 3.5 Sonnet", "provider": "anthropic" },
            { "id": "gpt-4o", "name": "GPT-4o", "provider": "openai" }
        ]);
        std::fs::write(&cache, serde_json::to_string(&data).unwrap()).unwrap();
        // Set mtime to now (fresh cache)
        let models = load_from_cache(&cache).unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "claude-3-5-sonnet");
        assert_eq!(models[0].provider, "anthropic");
    }

    #[test]
    fn filter_by_provider() {
        let models = vec![
            ModelInfo { id: "gpt-4o".into(), name: "GPT-4o".into(), provider: "openai".into(), context_length: None },
            ModelInfo { id: "claude-3-5-sonnet".into(), name: "Claude".into(), provider: "anthropic".into(), context_length: None },
        ];
        let filtered = filter_models(&models, Some("openai"));
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].id, "gpt-4o");
    }

    #[test]
    fn filter_none_returns_all() {
        let models = vec![
            ModelInfo { id: "a".into(), name: "A".into(), provider: "pa".into(), context_length: None },
            ModelInfo { id: "b".into(), name: "B".into(), provider: "pb".into(), context_length: None },
        ];
        assert_eq!(filter_models(&models, None).len(), 2);
    }
}
```

- [x] **Step 2: Run to verify they fail**

```bash
cargo test -p gait-ai models::tests 2>&1 | tail -5
```

Expected: compile errors — `ModelInfo`, `load_from_cache`, `filter_models` not found.

- [x] **Step 3: Implement `models.rs`**

Replace `gait-rs/src/models.rs` with the full implementation:

```rust
use serde::{Deserialize, Serialize};
use std::path::Path;

const MODELS_URL: &str = "https://models.dev/api.json";
const CACHE_TTL_SECS: u64 = 3600; // 1 hour

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
    pub provider: String,
    pub context_length: Option<u64>,
}

fn cache_path() -> std::path::PathBuf {
    dirs_next::cache_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("~/.cache"))
        .join("gait")
        .join("models.json")
}

pub fn load_from_cache(path: &Path) -> Option<Vec<ModelInfo>> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

fn cache_is_fresh(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else { return false };
    let Ok(modified) = meta.modified() else { return false };
    let Ok(age) = modified.elapsed() else { return false };
    age.as_secs() < CACHE_TTL_SECS
}

pub async fn get_models() -> anyhow::Result<Vec<ModelInfo>> {
    let cache = cache_path();

    if cache.exists() && cache_is_fresh(&cache) {
        if let Some(models) = load_from_cache(&cache) {
            return Ok(models);
        }
    }

    match fetch_models().await {
        Ok(models) => {
            if let Some(parent) = cache.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(&cache, serde_json::to_string_pretty(&models)?);
            Ok(models)
        }
        Err(e) => {
            // Offline fallback: use stale cache
            if let Some(models) = load_from_cache(&cache) {
                eprintln!("[warn] models.dev unreachable, using cached model list");
                return Ok(models);
            }
            anyhow::bail!("Cannot fetch model list and no cache exists. Specify \"model\" directly in gait.jsonc. ({})", e)
        }
    }
}

async fn fetch_models() -> anyhow::Result<Vec<ModelInfo>> {
    let resp = reqwest::get(MODELS_URL).await?;
    if !resp.status().is_success() {
        anyhow::bail!("models.dev returned {}", resp.status());
    }
    let data: serde_json::Value = resp.json().await?;
    let arr = data.as_array()
        .ok_or_else(|| anyhow::anyhow!("Unexpected models.dev response shape"))?;
    Ok(arr.iter().filter_map(|v| {
        Some(ModelInfo {
            id: v.get("id")?.as_str()?.to_string(),
            name: v.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string(),
            provider: v.get("provider").and_then(|p| p.as_str()).unwrap_or("").to_string(),
            context_length: v.get("contextLength").and_then(|c| c.as_u64()),
        })
    }).collect())
}

pub fn filter_models<'a>(models: &'a [ModelInfo], filter: Option<&str>) -> Vec<&'a ModelInfo> {
    match filter {
        None => models.iter().collect(),
        Some(f) => {
            let f_lower = f.to_lowercase();
            models.iter().filter(|m|
                m.provider.to_lowercase().contains(&f_lower) ||
                m.id.to_lowercase().contains(&f_lower) ||
                m.name.to_lowercase().contains(&f_lower)
            ).collect()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn get_models_from_cache() {
        let dir = TempDir::new().unwrap();
        let cache = dir.path().join("models.json");
        let data = serde_json::json!([
            { "id": "claude-3-5-sonnet", "name": "Claude 3.5 Sonnet", "provider": "anthropic" },
            { "id": "gpt-4o", "name": "GPT-4o", "provider": "openai" }
        ]);
        std::fs::write(&cache, serde_json::to_string(&data).unwrap()).unwrap();
        let models = load_from_cache(&cache).unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "claude-3-5-sonnet");
        assert_eq!(models[0].provider, "anthropic");
    }

    #[test]
    fn filter_by_provider() {
        let models = vec![
            ModelInfo { id: "gpt-4o".into(), name: "GPT-4o".into(), provider: "openai".into(), context_length: None },
            ModelInfo { id: "claude-3-5-sonnet".into(), name: "Claude".into(), provider: "anthropic".into(), context_length: None },
        ];
        let filtered = filter_models(&models, Some("openai"));
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].id, "gpt-4o");
    }

    #[test]
    fn filter_none_returns_all() {
        let models = vec![
            ModelInfo { id: "a".into(), name: "A".into(), provider: "pa".into(), context_length: None },
            ModelInfo { id: "b".into(), name: "B".into(), provider: "pb".into(), context_length: None },
        ];
        assert_eq!(filter_models(&models, None).len(), 2);
    }
}
```

- [x] **Step 4: Add `pub mod models;` to `gait-rs/src/lib.rs`**

```rust
pub mod models;
```

(Add after `pub mod factory;`)

- [x] **Step 5: Run the tests**

```bash
cargo test -p gait-ai models::tests 2>&1 | tail -10
```

Expected: `test result: ok. 3 passed`

- [x] **Step 6: Run all tests**

```bash
cargo test 2>&1 | grep -E "^(test result|FAILED)"
```

Expected: all `ok`

- [x] **Step 7: Commit**

```bash
git add gait-rs/src/models.rs gait-rs/src/lib.rs
git commit -m "feat(rs): models.rs — fetch + 1-hour cache from models.dev"
```

---

## Task 4: Config-defined tool wrappers

The `gait.jsonc` config allows users to define shell-command tools:
```jsonc
"tools": [
  { "name": "run_tests", "description": "Run the test suite", "cmd": "cargo test", "cwd": "codebase" }
]
```

These need to be instantiated as `Box<dyn Tool>` and passed to `run_turn`. The `cwd` field is either `"codebase"` or `"workspace"`, resolving to the respective path.

**Files:**
- Modify: `gait-rs/src/bin/gait.rs`
- Modify: `gait-rs/src/config.rs` (add 2 config tests for config tools)

- [x] **Step 1: Add a test for config tool parsing in `config.rs`**

Add to the `#[cfg(test)]` block in `gait-rs/src/config.rs`:

```rust
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
```

- [x] **Step 2: Run to verify it passes (config parsing already works)**

```bash
cargo test -p gait-ai config::tests::parses_config_defined_tools 2>&1 | tail -5
```

Expected: `test result: ok. 1 passed`

- [x] **Step 3: Wire config tools into `cmd_run` in `src/bin/gait.rs`**

In `cmd_run`, after `let tools = builtin_tools(codebase_path.clone());`, add:

```rust
// Append config-defined tools
let mut tools = builtin_tools(codebase_path.clone());
for ct in &config.tools {
    let cmd = ct.cmd.clone();
    let cwd = if ct.cwd == "workspace" {
        workspace.clone()
    } else {
        codebase_path.clone()
    };
    let name = ct.name.clone();
    let description = ct.description.clone();
    // Note: config-defined tools receive no arguments from the model — they are fixed shell
    // commands. The empty parameters schema tells the model to call them with no input.
    tools.push(Box::new(gait_ai::types::ToolDef {
        name,
        description,
        parameters: serde_json::json!({
            "type": "object",
            "properties": {},
            "required": []
        }),
        execute: Box::new(move |_args| {
            let cmd = cmd.clone();
            let cwd = cwd.clone();
            Box::pin(async move {
                let output = tokio::process::Command::new("sh")
                    .arg("-c")
                    .arg(&cmd)
                    .current_dir(&cwd)
                    .output()
                    .await
                    .map_err(|e| anyhow::anyhow!("{}", e))?;
                let stdout = String::from_utf8_lossy(&output.stdout).to_string();
                let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                let combined = if stderr.is_empty() { stdout } else { format!("{}\nstderr:\n{}", stdout, stderr) };
                Ok(serde_json::Value::String(combined))
            })
        }),
    }) as Box<dyn gait_ai::types::Tool>);
}
```

- [x] **Step 4: Build to verify it compiles**

```bash
cargo build --bin gait 2>&1 | grep -E "^(error|warning\[)" | head -20
```

Expected: no errors (warnings about unused imports are fine)

- [x] **Step 5: Run all tests**

```bash
cargo test 2>&1 | grep -E "^(test result|FAILED)"
```

Expected: all `ok`

- [x] **Step 6: Commit**

```bash
git add gait-rs/src/bin/gait.rs gait-rs/src/config.rs
git commit -m "feat(rs): wire config-defined tools into gait run"
```

---

## Task 5: `init`, `finish`, `sessions` commands in `gait.rs` binary

These three commands are thin wrappers over git ops we already implemented.

**Files:**
- Modify: `gait-rs/src/bin/gait.rs`

The default `gait.jsonc` content to write on `init`:

```
{
  // gait configuration
  "model": "anthropic/claude-sonnet-4-6",
  // Allow list: tools that run without approval. Default empty = everything gated.
  "allow": [],
  // Tier 1 config-defined tools: shell commands the LLM can call by name
  "tools": []
}
```

The default `AGENTS.md` content:

```markdown
# gait agent

You are a coding agent running inside gait, a stateless turn-based agent framework.

**How it works:**
- Each `gait run` invocation is one turn. Your full conversation history is replayed from a transcript each turn.
- The codebase is the external repo you work on. The workspace holds gait config and session history.

**Tools:**
- `read`, `write`, `edit`, `glob`, `grep`, `ls`, `shell` — work on the codebase
- Tools not in the allow list require user approval before running

**Turn model:**
- Complete your task in as few turns as possible
- Read files rather than asking when you need information
```

- [x] **Step 1: Add `init`, `finish`, `sessions` to the match in `main()`**

Replace the match block in `main()`:

```rust
match args[1].as_str() {
    "run"      => cmd_run(&args[2..]).await,
    "init"     => cmd_init(&args[2..]),
    "finish"   => cmd_finish(&args[2..]),
    "sessions" => cmd_sessions(&args[2..]),
    "auth"     => cmd_auth(&args[2..]),
    "models"   => cmd_models(&args[2..]).await,
    _ => {
        eprintln!("[gait] unknown command: {}", args[1]);
        usage();
    }
}
```

Also update `usage()`:

```rust
fn usage() -> ! {
    eprintln!("usage: gait <command> [options]");
    eprintln!("commands:");
    eprintln!("  run      [--session <id>] [--task <text>] [--workspace <path>] [--codebase <path>] [--auto] [--model <provider/model>]");
    eprintln!("  init     [<repo-url>]");
    eprintln!("  finish   <session-id> [--workspace <path>]");
    eprintln!("  sessions [--workspace <path>]");
    eprintln!("  auth     <list|add|remove> [<provider>]");
    eprintln!("  models   [<filter>]");
    std::process::exit(1);
}
```

- [x] **Step 2: Implement `cmd_init`**

```rust
fn cmd_init(argv: &[String]) {
    let repo_url = argv.first().filter(|a| !a.starts_with('-')).cloned();
    let workspace = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));

    if let Err(e) = gait_ai::git::init_workspace(&workspace) {
        eprintln!("[gait] git init failed: {}", e);
        std::process::exit(1);
    }

    let _ = std::fs::create_dir_all(workspace.join("sessions"));

    // .gitignore
    let gi_path = workspace.join(".gitignore");
    let gi_entry = "sessions/\n.gait-ws.lock\n";
    if !gi_path.exists() {
        let _ = std::fs::write(&gi_path, gi_entry);
    } else {
        let existing = std::fs::read_to_string(&gi_path).unwrap_or_default();
        if !existing.contains("sessions/") {
            let _ = std::fs::write(&gi_path, format!("{}\n{}", existing, gi_entry));
        }
    }

    // gait.jsonc
    let config_path = workspace.join("gait.jsonc");
    if !config_path.exists() {
        let _ = std::fs::write(&config_path, "{\n  // gait configuration\n  \"model\": \"anthropic/claude-sonnet-4-6\",\n  // Allow list: tools that run without approval. Default empty = everything gated.\n  \"allow\": [],\n  // Tier 1 config-defined tools: shell commands the LLM can call by name\n  \"tools\": []\n}\n");
    }

    // AGENTS.md
    let agents_path = workspace.join("AGENTS.md");
    if !agents_path.exists() {
        let _ = std::fs::write(&agents_path, "# gait agent\n\nYou are a coding agent running inside gait, a stateless turn-based agent framework.\n\n**How it works:**\n- Each `gait run` invocation is one turn. Your full conversation history is replayed from a transcript each turn.\n- The codebase is the external repo you work on. The workspace holds gait config and session history.\n\n**Tools:**\n- `read`, `write`, `edit`, `glob`, `grep`, `ls`, `shell` — work on the codebase\n- Tools not in the allow list require user approval before running\n\n**Turn model:**\n- Complete your task in as few turns as possible\n- Read files rather than asking when you need information\n");
    }

    // Stage and initial commit
    let repo = match git2::Repository::open(&workspace) {
        Ok(r) => r,
        Err(e) => { eprintln!("[gait] cannot open repo: {e}"); std::process::exit(1); }
    };
    let sig = repo.signature()
        .or_else(|_| git2::Signature::now("gait", "gait@localhost"))
        .unwrap();
    let mut index = repo.index().unwrap();

    let to_stage = ["gait.jsonc", ".gitignore", "AGENTS.md"];
    for f in &to_stage {
        if workspace.join(f).exists() {
            let _ = index.add_path(std::path::Path::new(f));
        }
    }
    index.write().unwrap();
    let tree_id = index.write_tree().unwrap();
    let tree = repo.find_tree(tree_id).unwrap();

    let parent_commits: Vec<git2::Commit> = repo.head()
        .ok().and_then(|h| h.peel_to_commit().ok()).into_iter().collect();
    let parents: Vec<&git2::Commit> = parent_commits.iter().collect();
    repo.commit(Some("HEAD"), &sig, &sig, "gait: init workspace", &tree, &parents).unwrap();

    if let Some(url) = repo_url {
        // Add submodule via git CLI (git2 submodule API is complex)
        let status = std::process::Command::new("git")
            .args(["submodule", "add", &url, "codebase"])
            .current_dir(&workspace)
            .status();
        match status {
            Ok(s) if s.success() => {
                eprintln!("[gait] workspace initialized with codebase submodule at {url}");
            }
            _ => eprintln!("[gait] warning: could not add submodule {url}"),
        }
    } else {
        eprintln!("[gait] workspace initialized");
        eprintln!("[gait] start an agent turn: gait run --task \"...\"");
    }
}
```

- [x] **Step 3: Implement `cmd_finish`**

```rust
fn cmd_finish(argv: &[String]) {
    let mut session_id: Option<String> = None;
    let mut workspace = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "--workspace" => { i += 1; if let Some(p) = argv.get(i) { workspace = std::path::PathBuf::from(p); } }
            other if !other.starts_with('-') => { session_id = Some(other.to_string()); }
            _ => {}
        }
        i += 1;
    }
    let session_id = match session_id {
        Some(id) => id,
        None => {
            eprintln!("usage: gait finish <session-id> [--workspace <path>]");
            std::process::exit(1);
        }
    };
    if let Err(e) = gait_ai::git::finish_session(&workspace, &session_id) {
        eprintln!("[gait] error: {}", e);
        std::process::exit(1);
    }
    eprintln!("[gait] session {} merged into main", session_id);
}
```

- [x] **Step 4: Implement `cmd_sessions`**

```rust
fn cmd_sessions(argv: &[String]) {
    let mut workspace = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let mut i = 0;
    while i < argv.len() {
        if argv[i] == "--workspace" { i += 1; if let Some(p) = argv.get(i) { workspace = std::path::PathBuf::from(p); } }
        i += 1;
    }
    match gait_ai::git::list_active_sessions(&workspace) {
        Ok(sessions) if sessions.is_empty() => println!("No active sessions."),
        Ok(sessions) => { for id in sessions { println!("{}", id); } }
        Err(e) => { eprintln!("[gait] error: {}", e); std::process::exit(1); }
    }
}
```

- [x] **Step 5: Build to verify it compiles**

```bash
cargo build --bin gait 2>&1 | grep "^error" | head -20
```

Expected: no errors

- [x] **Step 6: Smoke test**

```bash
/Users/aicoder/develop/cargo-target/debug/gait init --help 2>&1 || true
/Users/aicoder/develop/cargo-target/debug/gait finish 2>&1 | head -3
/Users/aicoder/develop/cargo-target/debug/gait sessions 2>&1 | head -3
```

Expected: usage/error messages (no panics)

- [x] **Step 7: Run all tests**

```bash
cargo test 2>&1 | grep -E "^(test result|FAILED)"
```

Expected: all `ok`

- [x] **Step 8: Commit**

```bash
git add gait-rs/src/bin/gait.rs
git commit -m "feat(rs): init, finish, sessions CLI commands"
```

---

## Task 6: `auth` and `models` commands

**Files:**
- Modify: `gait-rs/src/bin/gait.rs`

- [x] **Step 1: Implement `cmd_auth`**

Add to `gait-rs/src/bin/gait.rs`:

```rust
fn cmd_auth(argv: &[String]) {
    // Match on Option<&str> so the None arm handles `gait auth` (no subcommand).
    // Guards on or-patterns are not allowed in Rust — use None | Some("list") instead.
    match argv.first().map(|s| s.as_str()) {
        None | Some("list") | Some("ls") => {
            let creds = gait_ai::auth::list_credentials();
            if creds.is_empty() {
                println!("No credentials configured. Run: gait auth add <provider>");
            } else {
                let mut sorted: Vec<_> = creds.into_iter().collect();
                sorted.sort_by(|a, b| a.0.cmp(&b.0));
                for (k, v) in sorted { println!("  {}: {}", k, v); }
            }
        }
        Some("add") => {
            let provider = match argv.get(1) {
                Some(p) => p.as_str(),
                None => { eprintln!("usage: gait auth add <provider>"); std::process::exit(1); }
            };
            eprint!("{} API key: ", provider);
            use std::io::BufRead;
            let key = std::io::BufReader::new(std::io::stdin())
                .lines().next()
                .and_then(|l| l.ok())
                .unwrap_or_default()
                .trim()
                .to_string();
            if key.is_empty() { eprintln!("empty key, aborted"); std::process::exit(1); }
            if let Err(e) = gait_ai::auth::write_api_key(provider, &key) {
                eprintln!("[gait] error: {}", e); std::process::exit(1);
            }
            println!("Saved {} credentials", provider);
        }
        Some("remove") | Some("rm") => {
            let provider = match argv.get(1) {
                Some(p) => p.as_str(),
                None => { eprintln!("usage: gait auth remove <provider>"); std::process::exit(1); }
            };
            if let Err(e) = gait_ai::auth::remove_api_key(provider) {
                eprintln!("[gait] error: {}", e); std::process::exit(1);
            }
            println!("Removed {} credentials", provider);
        }
        Some(other) => {
            eprintln!("unknown auth subcommand: {}", other);
            eprintln!("usage: gait auth <list|add|remove> [<provider>]");
            std::process::exit(1);
        }
    }
}
```

- [x] **Step 2: Implement `cmd_models`**

```rust
async fn cmd_models(argv: &[String]) {
    let filter = argv.first().filter(|a| !a.starts_with('-')).map(|s| s.as_str());
    match gait_ai::models::get_models().await {
        Ok(models) => {
            let filtered = gait_ai::models::filter_models(&models, filter);
            for m in &filtered {
                println!("  {}/{}", m.provider, m.id);
            }
            println!("\n{} models{}", filtered.len(),
                filter.map(|f| format!(" matching \"{}\"", f)).unwrap_or_default());
        }
        Err(e) => { eprintln!("Error fetching models: {}", e); std::process::exit(1); }
    }
}
```

- [x] **Step 3: Build to verify it compiles**

```bash
cargo build --bin gait 2>&1 | grep "^error" | head -20
```

Expected: no errors

- [x] **Step 4: Smoke test `auth list`**

```bash
/Users/aicoder/develop/cargo-target/debug/gait auth list
```

Expected: either "No credentials configured." or a list of masked keys from `~/.local/share/gait/auth.json`

- [x] **Step 5: Run all tests**

```bash
cargo test 2>&1 | grep -E "^(test result|FAILED)"
```

Expected: all `ok`

- [x] **Step 6: Commit**

```bash
git add gait-rs/src/bin/gait.rs
git commit -m "feat(rs): auth and models CLI commands"
```

---

## Task 7: Integration tests for commands

**Files:**
- Create: `gait-rs/tests/integration_commands.rs`

- [x] **Step 1: Write the integration tests**

Create `gait-rs/tests/integration_commands.rs`:

```rust
use std::path::Path;
use tempfile::TempDir;

fn init_git_repo(dir: &Path) {
    let repo = git2::Repository::init(dir).unwrap();
    let mut cfg = repo.config().unwrap();
    cfg.set_str("user.name", "test").unwrap();
    cfg.set_str("user.email", "test@test.com").unwrap();
    let sig = git2::Signature::now("test", "test@test.com").unwrap();
    let tree_id = repo.index().unwrap().write_tree().unwrap();
    let tree = repo.find_tree(tree_id).unwrap();
    repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[]).unwrap();
    repo.find_branch("master", git2::BranchType::Local)
        .and_then(|mut b| { b.rename("main", false)?; Ok(()) }).ok();
}

#[test]
fn finish_session_removes_from_active() {
    let ws = TempDir::new().unwrap();
    init_git_repo(ws.path());
    gait_ai::git::create_session_worktree(ws.path(), "sfin001").unwrap();

    // Verify active before finish
    let before = gait_ai::git::list_active_sessions(ws.path()).unwrap();
    assert!(before.contains(&"sfin001".to_string()));

    gait_ai::git::finish_session(ws.path(), "sfin001").unwrap();

    // Verify gone after finish
    let after = gait_ai::git::list_active_sessions(ws.path()).unwrap();
    assert!(!after.contains(&"sfin001".to_string()), "should be merged: {:?}", after);
}

#[test]
fn sessions_lists_active_only() {
    let ws = TempDir::new().unwrap();
    init_git_repo(ws.path());
    gait_ai::git::create_session_worktree(ws.path(), "sactive").unwrap();
    gait_ai::git::create_session_worktree(ws.path(), "sfinished").unwrap();
    gait_ai::git::finish_session(ws.path(), "sfinished").unwrap();

    let active = gait_ai::git::list_active_sessions(ws.path()).unwrap();
    assert!(active.contains(&"sactive".to_string()), "active not listed: {:?}", active);
    assert!(!active.contains(&"sfinished".to_string()), "finished should not be listed: {:?}", active);
}

#[test]
fn auth_write_list_remove_roundtrip() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("auth.json");
    gait_ai::auth::write_api_key_to(&path, "anthropic", "sk-test-abc123").unwrap();
    gait_ai::auth::write_api_key_to(&path, "openai", "sk-openai-xyz").unwrap();

    let list = gait_ai::auth::list_credentials_from(&path);
    assert_eq!(list.len(), 2);
    assert!(list["anthropic"].starts_with("sk-test-"));
    assert!(list["anthropic"].ends_with("..."));

    gait_ai::auth::remove_api_key_from(&path, "openai").unwrap();
    let list2 = gait_ai::auth::list_credentials_from(&path);
    assert_eq!(list2.len(), 1);
    assert!(!list2.contains_key("openai"));
}

#[test]
fn models_filter_works() {
    let models = vec![
        gait_ai::models::ModelInfo { id: "claude-3-5-sonnet".into(), name: "Claude 3.5".into(), provider: "anthropic".into(), context_length: Some(200_000) },
        gait_ai::models::ModelInfo { id: "gpt-4o".into(), name: "GPT-4o".into(), provider: "openai".into(), context_length: Some(128_000) },
        gait_ai::models::ModelInfo { id: "gpt-4o-mini".into(), name: "GPT-4o Mini".into(), provider: "openai".into(), context_length: Some(128_000) },
    ];
    let openai = gait_ai::models::filter_models(&models, Some("openai"));
    assert_eq!(openai.len(), 2);
    let claude = gait_ai::models::filter_models(&models, Some("claude"));
    assert_eq!(claude.len(), 1);
    assert_eq!(claude[0].provider, "anthropic");
}
```

- [x] **Step 2: Run the integration tests**

```bash
cargo test --test integration_commands 2>&1 | tail -10
```

Expected: `test result: ok. 4 passed`

- [x] **Step 3: Run the full test suite**

```bash
cargo test 2>&1 | grep -E "^(test result|FAILED|running)"
```

Expected: all `ok`, no `FAILED`. Total test count should be 85+.

- [x] **Step 4: Commit**

```bash
git add gait-rs/tests/integration_commands.rs
git commit -m "test(rs): integration tests for init/finish/sessions/auth/models commands"
```

---

## Self-Review

**1. Spec coverage:**
- `gait init [repo-url]` → Task 5 (`cmd_init`)
- `gait finish <session-id>` → Task 1 (git op) + Task 5 (`cmd_finish`)
- `gait sessions` → Task 5 (`cmd_sessions`, uses existing `list_active_sessions`)
- `gait auth <add|list|remove>` → Task 2 (auth ops) + Task 6 (`cmd_auth`)
- `gait models [filter]` → Task 3 (`models.rs`) + Task 6 (`cmd_models`)
- Config-defined tools → Task 4

**2. Placeholder scan:** No TBD, no "implement later", all code blocks are complete.

**3. Type consistency:**
- `write_api_key_to(path, provider, key)` used in tests matches implementation signature
- `list_credentials_from(path)` → `HashMap<String, String>` — consistent across Task 2 tests and Task 7 tests
- `ModelInfo { id, name, provider, context_length }` — consistent across Task 3 and Task 7 tests
- `finish_session(workspace, session_id)` → `anyhow::Result<()>` — consistent with existing `git.rs` style
- `gait_ai::git::finish_session` / `gait_ai::auth::write_api_key_to` / `gait_ai::models::filter_models` — all pub, all accessible from integration tests

**4. Opus review fixes applied:**
- `finish_session` merge call: fixed `CheckoutBuilder.clone()` (doesn't implement Clone) → use `let mut checkout_opts = CheckoutBuilder::new()` and pass `&mut checkout_opts` directly
- `finish_session` worktree pruning: removed bogus `repo.worktree(...)` create call; correct order is `find_worktree` → `prune` → `remove_dir_all`
- `finish_session` conflict guard: added `index.has_conflicts()` check before committing merge; calls `cleanup_state()` before bailing
- Auth mask: changed `&key[..key.len().min(8)]` (byte-slice, panics on multibyte UTF-8) → `key.chars().take(8).collect::<String>()`
- Config-defined tools: added `as Box<dyn gait_ai::types::Tool>` cast — `Vec::push` does not auto-coerce `Box<ToolDef>` to `Box<dyn Tool>`
- `cmd_auth`: fixed invalid or-pattern-with-guard (`"list" | "ls" | _ if argv.is_empty()`) → match on `Option<&str>` using `None | Some("list") | Some("ls")`

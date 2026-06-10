# Profile + ResolvedProfile (Resolution Layer) — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Introduce a declarative `Profile` (the serializable description of an agent) and a live `ResolvedProfile` (provider + tools + clients), built by ONE `Profile::resolve(...)`, so the four composition roots stop duplicating the MCP-discovery loop, the `builtin_tools` call, and the allow/deny resolution.

**Architecture:** Today assembling a runnable agent requires 9 declarative inputs (model, instructions, allow, deny, mcp_servers, skills, config_tools, config_enabled, output_schema_retries) combined into 3 live objects (provider, tools vec, mcp clients) — and this is done inline, separately, in `bin/gait.rs` and `workflow/exec_prompt.rs` (verbatim-duplicated MCP loop + `builtin_tools`), plus implicitly in the harness/spawn subprocess path and the `curate` filter. This plan moves that into `Profile` (the 9 inputs) + `Profile::resolve()` (produces `ResolvedProfile`). `run_turn` still takes `TurnOpts` (unchanged this plan); the call sites build `TurnOpts` *from* the `ResolvedProfile`. Plan 4 then dissolves `TurnOpts` into `Session` + `ResolvedProfile` + `TurnInput`.

**Tech Stack:** Rust, `async`, `cargo test`. No new dependencies.

**Scope:** Plan 3 of the sequence. Depends on nothing from Plans 1–2. `run_turn`'s signature is unchanged here (the bridge to `TurnOpts` is deliberate, so this plan ships independently). Out of scope: the `curate` filter and spawn-subprocess paths keep working as-is (they re-resolve from config), and are migrated in Plan 4 where it's natural.

---

## File Structure

| File | Responsibility after this plan |
|---|---|
| `gait-rs/src/prompt.rs` | **(new)** `compose_system_prompt` + `content_hash_short`, moved out of `bin/gait.rs` so the library can compose instructions. |
| `gait-rs/src/profile.rs` | **(new)** `Profile` (declarative, 9 inputs) + `Profile::from_config_and_agent` builder + `ResolvedProfile` (live) + `Profile::resolve()` (the single resolver: provider + MCP loop + tools). |
| `gait-rs/src/bin/gait.rs` | `cmd_run` builds `Profile` → `resolve()` → `TurnOpts`. Inline MCP loop / `builtin_tools` / allow-deny resolution deleted. |
| `gait-rs/src/workflow/exec_prompt.rs` | Same: builds `Profile` → `resolve()`. Duplicated MCP loop / `builtin_tools` deleted. |
| `gait-rs/src/lib.rs` | `pub mod prompt; pub mod profile;` + re-exports. |

Target types (`profile.rs`):

```rust
pub struct Profile {
    pub model: String,                 // "provider/model-id"
    pub instructions: String,          // composed system prompt (preamble + agent body)
    pub allow: Vec<String>,
    pub deny: Vec<String>,
    pub mcp_servers: Vec<crate::config::McpServerDef>,
    pub skills: Vec<crate::config::SkillDef>,
    pub config_tools: Vec<crate::config::ConfigTool>,   // shell-command tool descriptors from gait.jsonc (config.tools: Vec<ConfigTool>)
    pub config_enabled: std::collections::HashSet<String>,
    pub output_schema_retries: u8,
}

pub struct ResolvedProfile {
    pub provider: std::sync::Arc<dyn crate::provider::Provider>,
    pub tools: Vec<Box<dyn crate::types::Tool>>,
    pub model: String,
    pub instructions: String,
    pub allow: Vec<String>,
    pub deny: Vec<String>,
    pub mcp_registry: Vec<crate::config::McpToolDef>,
    pub mcp_clients: std::collections::HashMap<String, std::sync::Arc<crate::mcp_client::McpClient>>,
    pub skill_registry: Vec<crate::config::SkillDef>,
    pub config_enabled: std::collections::HashSet<String>,
    pub output_schema_retries: u8,
}
```

> **Pinned (re-ground):** `config.tools` is `Vec<crate::config::ConfigTool>` (struct `ConfigTool` at `config.rs:7`, fields `name/description/cmd/cwd`). Use `ConfigTool` for `Profile::config_tools` and for the `build_config_tool` param. Do NOT use `types::ToolDef` — that is the *boxed tool* the loop constructs from a `ConfigTool`.

---

### Task 1: Move `compose_system_prompt` + `content_hash_short` into a library module

**Files:**
- Create: `gait-rs/src/prompt.rs`
- Modify: `gait-rs/src/bin/gait.rs` (delete the two fns; call `gait_ai::prompt::compose_system_prompt`)
- Modify: `gait-rs/src/lib.rs` (`pub mod prompt;`)

- [ ] **Step 1: Create `prompt.rs` with the two functions moved verbatim**

Create `gait-rs/src/prompt.rs`. Move `compose_system_prompt` (currently `bin/gait.rs:165-188`) and its helper `content_hash_short` (`bin/gait.rs:190-196`) verbatim, changing the crate-internal references from `gait_ai::` to `crate::`:

```rust
//! Prompt composition: the harness preamble + an optional agent body become the
//! agent's `instructions` (the system prompt passed to the model).

use std::path::Path;

pub fn compose_system_prompt(agent_path: Option<&Path>) -> String {
    let gait_hash = &crate::GAIT_BUILD_GIT_HASH[..crate::GAIT_BUILD_GIT_HASH.len().min(7)];
    let mut prompt = format!(
        "<!-- gait-agent.md @ {} -->\n{}",
        gait_hash,
        crate::GAIT_AGENT_PROMPT.trim_end(),
    );
    if let Some(path) = agent_path {
        let body = crate::agent_frontmatter::read_agent_body(path);
        if !body.is_empty() {
            let label = path.file_name().and_then(|n| n.to_str()).unwrap_or("agent.md");
            let hash = content_hash_short(&body);
            prompt.push_str(&format!("\n\n<!-- {} @ {} -->\n{}", label, hash, body));
        }
    }
    prompt
}

pub fn content_hash_short(s: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    format!("{:016x}", std::hash::Hasher::finish(&h))[..7].to_string()
}
```

> **Re-grounded:** this is the exact current body from `bin/gait.rs:191-196`. The `{:016x}` width matters — `[..7]` would panic on a short hash from `{:x}`. Keep it verbatim.

Add `pub mod prompt;` to `gait-rs/src/lib.rs`.

- [ ] **Step 2: Delete the two functions from `bin/gait.rs` and update the call site**

In `gait-rs/src/bin/gait.rs`: delete `compose_system_prompt` (165-188) and `content_hash_short` (190-196); change the call site at line **261** `let system_prompt = compose_system_prompt(agent_path.as_deref());` → `let system_prompt = gait_ai::prompt::compose_system_prompt(agent_path.as_deref());`. (No separate import to remove — the `use std::hash::{Hash, Hasher};` is *function-local* inside `content_hash_short`, so it leaves with the deleted function.)

- [ ] **Step 3: Write a test for `compose_system_prompt`**

Add to `gait-rs/src/prompt.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn compose_without_agent_is_just_preamble() {
        let p = compose_system_prompt(None);
        assert!(p.contains("gait-agent.md @"));
        assert!(p.starts_with("<!-- gait-agent.md @"));
    }
}
```

- [ ] **Step 4: Build and test**

Run (from `gait-rs/`):
```bash
cargo test --lib prompt
cargo build
```
Expected: PASS / builds (the binary uses the moved fn).

- [ ] **Step 5: Commit**

```bash
git add gait-rs/src/prompt.rs gait-rs/src/lib.rs gait-rs/src/bin/gait.rs
git commit -m "refactor: move compose_system_prompt + content_hash_short into prompt module"
```

---

### Task 2: Add `Profile` (declarative) + `from_config_and_agent` builder

**Files:**
- Create: `gait-rs/src/profile.rs`
- Modify: `gait-rs/src/lib.rs` (`pub mod profile;` + re-export `Profile`, `ResolvedProfile`)

- [ ] **Step 1: Write the failing test**

Create `gait-rs/src/profile.rs` with the `Profile` struct (from File Structure above) and this builder + test:

```rust
use std::collections::HashSet;
use std::path::Path;
use crate::config::GaitConfig;

// ... Profile struct (see File Structure) ...

impl Profile {
    /// Build the declarative profile from workspace config + an optional agent .md.
    /// Model precedence: explicit override > agent frontmatter > config.
    /// allow/deny: agent frontmatter (when non-empty) supersedes config.
    pub fn from_config_and_agent(
        config: &GaitConfig,
        agent_path: Option<&Path>,
        model_override: Option<String>,
    ) -> Self {
        let model = model_override
            .or_else(|| agent_path.and_then(crate::agent_frontmatter::read_agent_model))
            .unwrap_or_else(|| config.model.clone());

        let instructions = crate::prompt::compose_system_prompt(agent_path);

        let allow = agent_path
            .and_then(|p| crate::agent_frontmatter::read_agent_allow(p.to_path_buf()).ok())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| config.allow.clone());
        let deny = agent_path
            .and_then(|p| crate::agent_frontmatter::read_agent_deny(p.to_path_buf()).ok())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| config.deny.clone());

        let config_enabled: HashSet<String> = config.mcp_servers.iter()
            .filter(|s| s.enabled).map(|s| s.name.clone())
            .chain(config.skills.iter().filter(|s| s.enabled).map(|s| s.name.clone()))
            .collect();

        Profile {
            model,
            instructions,
            allow,
            deny,
            mcp_servers: config.mcp_servers.clone(),
            skills: config.skills.clone(),
            config_tools: config.tools.clone(),
            config_enabled,
            output_schema_retries: config.output_schema_retries,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builder_uses_config_model_when_no_override_or_agent() {
        let config = GaitConfig { model: "anthropic/x".into(), ..GaitConfig::default() };
        let p = Profile::from_config_and_agent(&config, None, None);
        assert_eq!(p.model, "anthropic/x");
        assert_eq!(p.allow, config.allow);
    }

    #[test]
    fn explicit_model_override_wins() {
        let config = GaitConfig { model: "anthropic/x".into(), ..GaitConfig::default() };
        let p = Profile::from_config_and_agent(&config, None, Some("openai/y".into()));
        assert_eq!(p.model, "openai/y");
    }
}
```

> Confirm `GaitConfig` implements `Default` (or construct it via the real test helper used elsewhere in the codebase). If not, build it via `load_config` against a temp workspace, matching the pattern in existing config tests.

Add to `gait-rs/src/lib.rs`: `pub mod profile;` and `pub use profile::{Profile, ResolvedProfile};`.

- [ ] **Step 2: Run the tests**

Run (from `gait-rs/`):
```bash
cargo test --lib profile::tests
```
Expected: PASS (2 tests). (`ResolvedProfile` is declared but unused so far — allowed.)

- [ ] **Step 3: Commit**

```bash
git add gait-rs/src/profile.rs gait-rs/src/lib.rs
git commit -m "feat: add Profile (declarative agent definition) + from_config_and_agent builder"
```

---

### Task 3: Add `ResolvedProfile` + `Profile::resolve()` (the single resolver)

**Files:**
- Modify: `gait-rs/src/profile.rs` (add `resolve`)

- [ ] **Step 1: Write the failing test (no-network resolve)**

Add to `profile.rs` tests:
```rust
    #[tokio::test]
    async fn resolve_with_no_mcp_builds_provider_and_builtin_tools() {
        use std::collections::HashSet;
        let profile = Profile {
            model: "anthropic/claude-x".into(),
            instructions: "sys".into(),
            allow: vec!["read".into()],
            deny: vec![],
            mcp_servers: vec![],          // no network
            skills: vec![],
            config_tools: vec![],
            config_enabled: HashSet::new(),
            output_schema_retries: 3,
        };
        let resolved = profile.resolve(
            std::path::PathBuf::from("."),
            "s1".into(),
            std::path::PathBuf::from("."),
            crate::git::Repos::production(),
            Some("test-key".into()),
        ).await.unwrap();
        assert_eq!(resolved.provider.model_id(), "claude-x");
        assert!(resolved.tools.len() >= 15, "expected the builtin tools");
        assert_eq!(resolved.allow, vec!["read".to_string()]);
    }
```

- [ ] **Step 2: Run it to verify it fails (no `resolve` method)**

Run (from `gait-rs/`):
```bash
cargo test --lib profile::tests::resolve_with_no_mcp_builds_provider_and_builtin_tools
```
Expected: FAIL — `no method named 'resolve'`.

- [ ] **Step 3: Implement `resolve`**

Add the `ResolvedProfile` struct (from File Structure) and this method to `profile.rs`. **Re-grounded line numbers:** the MCP loop is moved verbatim from `bin/gait.rs:263-280` (include its three `eprintln!("[gait] mcp …")` / warning log lines for behavior parity); the `builtin_tools` call from `bin/gait.rs:285-292`; the config-tool loop from `bin/gait.rs:293-330`:

```rust
use std::sync::Arc;
use std::collections::HashMap;
use std::path::PathBuf;

impl Profile {
    pub async fn resolve(
        self,
        codebase_path: PathBuf,
        session_id: String,
        workspace: PathBuf,
        repos: crate::git::Repos,
        api_key: Option<String>,
    ) -> anyhow::Result<ResolvedProfile> {
        // Provider (no network until .stream()).
        let provider: Arc<dyn crate::provider::Provider> =
            Arc::from(crate::factory::load_provider(&self.model, api_key)
                .map_err(|e| anyhow::anyhow!("provider: {e}"))?);

        // MCP connect + discover (moved verbatim from bin/gait.rs:263-279).
        let mut mcp_registry: Vec<crate::config::McpToolDef> = vec![];
        let mut mcp_clients: HashMap<String, Arc<crate::mcp_client::McpClient>> = HashMap::new();
        for server_def in &self.mcp_servers {
            if let Ok(client) = crate::mcp_client::McpClient::connect(server_def).await {
                if let Ok(tools) = client.discover_tools().await {
                    mcp_registry.extend(tools);
                    mcp_clients.insert(server_def.name.clone(), Arc::new(client));
                }
            }
        }

        // Tools: builtins (moved from bin/gait.rs:284) + config tools (moved from bin/gait.rs:~290-329).
        let mut tools = crate::tools::builtin_tools(
            codebase_path.clone(),
            session_id.clone(),
            workspace.clone(),
            mcp_registry.clone(),
            self.skills.clone(),
            repos,
        );
        for ct in &self.config_tools {
            // MOVE the exact config-tool → Box<dyn Tool> construction currently at
            // bin/gait.rs:293-330 here, verbatim. (It builds a types::ToolDef whose
            // `execute` shells out the configured command.)
            tools.push(build_config_tool(ct));
        }

        Ok(ResolvedProfile {
            provider,
            tools,
            model: self.model,
            instructions: self.instructions,
            allow: self.allow,
            deny: self.deny,
            mcp_registry,
            mcp_clients,
            skill_registry: self.skills,
            config_enabled: self.config_enabled,
            output_schema_retries: self.output_schema_retries,
        })
    }
}
```

Implement `build_config_tool(ct: &crate::config::ConfigTool) -> Box<dyn crate::types::Tool>` by moving the exact body of the `for ct in &config.tools { ... }` loop from `bin/gait.rs:293-330` into this helper. Note the param type is `ConfigTool` (the input descriptor); the body constructs a `crate::types::ToolDef` (the boxed tool). Do not paraphrase — copy the real construction so behavior is identical.

- [ ] **Step 4: Run the test to verify it passes**

Run (from `gait-rs/`):
```bash
cargo test --lib profile::tests::resolve_with_no_mcp_builds_provider_and_builtin_tools
```
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add gait-rs/src/profile.rs
git commit -m "feat: add ResolvedProfile + Profile::resolve (provider + MCP + tools, one place)"
```

---

### Task 4: Route `bin/gait.rs::cmd_run` through `Profile::resolve`

**Files:**
- Modify: `gait-rs/src/bin/gait.rs` (replace the inline assembly at ~239-340 with Profile build + resolve)

- [ ] **Step 1: Replace the inline assembly**

In `cmd_run`, after `config` is loaded and `agent_path` resolved, replace the inline assembly — **re-grounded line ranges:** model resolution (240-242), provider construction (244-259), MCP loop (263-280), `let repos = Repos::production()` stays (it's at 282, between the MCP loop and tools — keep/reuse it), tools build (285-330), and allow/deny resolution (332-341) — with:

```rust
let api_key = resolve_api_key(model.split_once('/').map(|(p,_)| p).unwrap_or(""));
let profile = gait_ai::profile::Profile::from_config_and_agent(&config, agent_path.as_deref(), model_override.clone());
let model = profile.model.clone();
let resolved = match profile.resolve(
    codebase_path.clone(), session_id.clone(), workspace.clone(), repos.clone(),
    api_key,
).await {
    Ok(r) => r,
    Err(e) => { eprintln!("[gait] error: {}", e); std::process::exit(1); }
};
```

(Resolve the `api_key` using the existing `auth::resolve_api_key(provider_name)` helper, keyed on the provider prefix of `profile.model`. **Behavior parity:** the current code at 244-250 validates `model.split_once('/')` and exits with a "model must be 'provider/model'" error if there's no `/` — preserve that explicit validation/exit before resolving, rather than silently `unwrap_or("")`. Keep `system_prompt = profile.instructions` available via `resolved.instructions`.)

- [ ] **Step 2: Build `TurnOpts` from the `ResolvedProfile` (bridge)**

Update the `run_turn(TurnOpts { ... }, provider, tools)` call to draw from `resolved`:

```rust
let turn_result = match run_turn(
    TurnOpts {
        session_id: session_id.clone(),
        task,
        auto,
        workspace: workspace.clone(),
        codebase_path,
        allow: resolved.allow.clone(),
        deny: resolved.deny.clone(),
        system_prompt: resolved.instructions.clone(),
        model: resolved.model.clone(),
        mcp_registry: resolved.mcp_registry.clone(),
        mcp_clients: resolved.mcp_clients.clone(),
        skill_registry: resolved.skill_registry.clone(),
        config_enabled: resolved.config_enabled.clone(),
        max_iter: None,
        hooks: vec![],
        repos,
        output_schema_retries: resolved.output_schema_retries,
    },
    resolved.provider.clone(),
    resolved.tools,
).await { /* unchanged Ok/Err arms */ };
```

Delete the now-dead inline MCP/tools/provider/allow-deny code. Keep the harness/scheduler block (Plan 2) intact.

- [ ] **Step 3: Build and smoke-test**

Run (from `gait-rs/`):
```bash
cargo build
cargo test
```
Expected: builds; full suite PASS (the change is internal to `cmd_run`, which has no unit test; the integration tests exercise `run_turn` directly and are unaffected).

- [ ] **Step 4: Commit**

```bash
git add gait-rs/src/bin/gait.rs
git commit -m "refactor: cmd_run assembles the agent via Profile::resolve (kills inline duplication)"
```

---

### Task 5: Route `workflow/exec_prompt.rs` through `Profile::resolve`

**Files:**
- Modify: `gait-rs/src/workflow/exec_prompt.rs` (replace duplicated MCP loop + `builtin_tools` at ~33-82)

- [ ] **Step 1: Replace the duplicated assembly**

In `exec_prompt_node`, replace the duplicated MCP discovery loop (lines 36-52) and `builtin_tools` call (55-58) with a `Profile` built from the workflow node's settings, then `resolve()`. Preserve the workflow-specific differences: system prompt from `resolve_system_prompt(node, workspace)` (not the harness-preamble composer), `deny: vec![]`, model precedence `node.model > workflow_def.model > "minimax/MiniMax-M1-80k"`, and the **passed-in** provider:

```rust
let model = node.model.clone()
    .or_else(|| workflow_def.model.clone())
    .unwrap_or_else(|| "minimax/MiniMax-M1-80k".to_string());
let mut profile = gait_ai::profile::Profile::from_config_and_agent(&config, None, Some(model));
profile.instructions = resolve_system_prompt(node, ctx.workspace);   // workflow body, no preamble
profile.deny = vec![];                                               // workflow nodes: no deny
profile.allow = allow;                                               // existing workflow allow
let resolved = profile.resolve(
    codebase.clone(), ctx.session_id.to_string(), ctx.workspace.to_path_buf(),
    repos.clone(), api_key,
).await?;
```

Then build the workflow's `TurnOpts` from `resolved` exactly as in Task 4 Step 2, but pass the workflow's existing `provider` (the runner supplies it) instead of `resolved.provider` if the workflow must reuse the injected provider — otherwise use `resolved.provider`. Confirm which during implementation; the workflow runner currently constructs and threads its own provider, so prefer that one for behavior parity:

```rust
run_turn(TurnOpts { /* fields from resolved, as Task 4 */ }, provider, resolved.tools).await?
```

- [ ] **Step 2: Build and run workflow tests**

Run (from `gait-rs/`):
```bash
cargo build
cargo test workflow
cargo test
```
Expected: builds; workflow + full suites PASS.

- [ ] **Step 3: Commit**

```bash
git add gait-rs/src/workflow/exec_prompt.rs
git commit -m "refactor: workflow prompt node assembles via Profile::resolve (dedup MCP + tools)"
```

---

## Self-Review

**Spec coverage** (against the `Profile` / `ResolvedProfile` sections):
- *"Profile — the declarative description… Model ref, instructions, Tool/Loadable set, allow/deny"* → `Profile` holds exactly the 9 declarative inputs; `from_config_and_agent` is the builder. ✓
- *"ResolvedProfile — live Arc<dyn Provider>, Vec<Box<dyn Tool>>, resolved Loadables + clients"* → `ResolvedProfile`; `resolve()` produces them in one place. ✓
- *"collapses the 4 composition roots into one resolver"* → Tasks 4 & 5 route `bin/gait.rs` and `exec_prompt.rs` through `resolve()`, deleting the duplicated MCP loop + `builtin_tools`. (Spawn-subprocess + curate paths are migrated in Plan 4.) ✓
- *"resolution requires async I/O (key lookup, MCP connects)"* → `resolve` is `async`, takes the api_key, runs the MCP loop. ✓

**Placeholder scan:** The two "MOVE the exact existing loop from bin/gait.rs:NNN" instructions reference real, line-cited code to relocate verbatim — they are precise relocation instructions, not invented placeholders. Everything else is complete code. The plan flags two facts to pin at implementation time (the `config.tools` element type; whether the workflow reuses its injected provider) — both are checked, not guessed.

**Type/name consistency:** `Profile` fields and `ResolvedProfile` fields match between the File Structure, the builder, `resolve`, and the `TurnOpts` bridge in Tasks 4–5. `resolve(codebase_path, session_id, workspace, repos, api_key)` signature is identical in the test, the impl, and both call sites.

**Risk:** Behavior parity is the goal. The resolve test pins provider + tool-count with no network. The two production call sites have no unit tests; the integration suite exercises `run_turn` and is unaffected. The deliberate bridge (build `TurnOpts` from `ResolvedProfile`) keeps `run_turn` unchanged so this plan is independently shippable.

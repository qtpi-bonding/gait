---
status: done
---

# Config-Driven Post-Tool Hooks Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Allow users to declare shell-command hooks in `gait.jsonc` that fire after tool calls, enabling per-project checks like `rustfmt --check` that give the agent immediate syntax feedback without hardcoding any language logic in gait.

**Architecture:** `HookDef` structs in `GaitConfig` flow through `Profile` → `ResolvedProfile.hooks` as `ConfigHook` instances implementing the existing `Hook` trait. `Hook::post_tool` becomes async (so it can shell out via the existing `run_hook` executor). On non-zero exit, the hook appends the command output to the tool result the model sees. Every hook execution emits a `HookRun` proto transcript event for worldline traceability.

**Tech Stack:** Rust, `async_trait`, existing `workflow::hooks::run_hook`, `proto/wire::emit`, `serde_json`

---

## File Map

| File | Change |
|------|--------|
| `proto/log.proto` | Add `HookRun` message + `HookPhase` enum at field 51; update reserved-range comment |
| `gait-rs/src/config.rs` | Add `HookDef` struct; add `hooks: Vec<HookDef>` to `GaitConfig` |
| `gait-rs/src/profile.rs` | Add `hooks: Vec<HookDef>` to `Profile`; build `ConfigHook`s in `resolve` |
| `gait-rs/src/types.rs` | Add `#[async_trait]` to `Hook` trait; make `post_tool` async with `call_id` + `args` params |
| `gait-rs/src/turn.rs` | Update 3 `post_tool` call sites to pass `call_id`, `args`, and `.await` |
| `gait-rs/src/proto/events.rs` | Add `hook_run()` constructor + `hook_phase()` helper |
| `gait-rs/src/config_hook.rs` | **New.** `ConfigHook` struct + `impl Hook` |
| `gait-rs/src/lib.rs` | Add `pub mod config_hook` |
| `gait.jsonc` | Add example hook entry |

---

### Task 1: Add `HookRun` + `HookPhase` to proto

**Files:**
- Modify: `proto/log.proto`

- [ ] **Step 1: Add `HookRun` to `Payload.kind` and the message + enum**

In `proto/log.proto`, add `HookRun hook_run = 51;` to `Payload.kind` (after field 50), update the reserved-range comment, and append the new message and enum after the `SandboxEnforcement` enum:

```proto
// In Payload.kind oneof — add after sandbox_applied = 50:
HookRun         hook_run            = 51;
```

```proto
// Update the reserved comment line:
reserved 20 to 49;  // workflow family (WorkflowHook* / WorkflowNode* / WorkflowStart …) — deferred to the workflow-subsystem redo
```

```proto
// ── Hooks ────────────────────────────────────────────────────────────────────
// Emitted by config-driven hooks (gait.jsonc "hooks": [...]).
// Workflow-subsystem hook events are WorkflowHook* in the reserved 20-49 range.
message HookRun {
  HookPhase phase      = 1;  // lifecycle point where this hook fired
  string    command    = 2;  // the shell command string
  int32     exit_code  = 3;  // 0 = pass; non-zero = hook reported failure
  string    output     = 4;  // combined stdout+stderr from the command
  optional string call_id = 5;  // ToolCall correlation (post_tool / pre_tool only)
}
enum HookPhase {
  HOOK_PHASE_UNSPECIFIED = 0;
  HOOK_PHASE_POST_TOOL   = 1;
  HOOK_PHASE_PRE_TOOL    = 2;
  HOOK_PHASE_POST_TURN   = 3;
  HOOK_PHASE_PRE_TURN    = 4;
}
```

- [ ] **Step 2: Verify proto compiles**

```bash
cd gait-rs && cargo build 2>&1 | grep -E "^error"
```

Expected: no errors. The prost build step regenerates Rust from the proto on every `cargo build`.

- [ ] **Step 3: Commit**

```bash
cd gait-rs && git add ../proto/log.proto
git commit -m "feat: add HookRun + HookPhase to proto log schema"
```

---

### Task 2: Add `HookDef` to config + wire through `Profile`

**Files:**
- Modify: `gait-rs/src/config.rs`
- Modify: `gait-rs/src/profile.rs`
- Test: `gait-rs/src/config.rs` (inline tests)

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)]` block at the bottom of `config.rs`:

```rust
#[test]
fn hook_def_deserializes() {
    let json = r#"{
        "model": "anthropic/claude-sonnet-4-6",
        "hooks": [
            { "event": "post_tool", "tool": "edit", "run": "rustfmt --check $GAIT_ARG_FILE_PATH" },
            { "event": "post_tool", "run": "echo done" }
        ]
    }"#;
    let cfg: GaitConfig = serde_json::from_str(json).unwrap();
    assert_eq!(cfg.hooks.len(), 2);
    assert_eq!(cfg.hooks[0].event, "post_tool");
    assert_eq!(cfg.hooks[0].tool, Some("edit".to_string()));
    assert_eq!(cfg.hooks[0].run, "rustfmt --check $GAIT_ARG_FILE_PATH");
    assert_eq!(cfg.hooks[1].tool, None);
}

#[test]
fn hooks_default_empty() {
    let cfg = GaitConfig::default();
    assert!(cfg.hooks.is_empty());
}
```

- [ ] **Step 2: Run tests to confirm they fail**

```bash
cd gait-rs && cargo test --lib config 2>&1 | tail -20
```

Expected: `error[E0609]: no field 'hooks' on type 'GaitConfig'`

- [ ] **Step 3: Add `HookDef` struct and `hooks` field**

In `config.rs`, add `HookDef` after `WasmPluginDef`:

```rust
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
```

Add `#[serde(default)] pub hooks: Vec<HookDef>,` to `GaitConfig` struct and `hooks: vec![]` to `GaitConfig::default()`.

- [ ] **Step 4: Add `hooks` to `Profile`**

In `profile.rs`, add `pub hooks: Vec<crate::config::HookDef>,` to `Profile`.

In `Profile::from_config_and_agent`, add `hooks: config.hooks.clone(),` to the returned `Profile`.

- [ ] **Step 5: Run tests to confirm they pass**

```bash
cd gait-rs && cargo test --lib config 2>&1 | tail -10
```

Expected: `test config::tests::hook_def_deserializes ... ok` and `test config::tests::hooks_default_empty ... ok`

- [ ] **Step 6: Commit**

```bash
cd gait-rs && git add src/config.rs src/profile.rs
git commit -m "feat: add HookDef config struct + hooks field to GaitConfig and Profile"
```

---

### Task 3: Make `Hook::post_tool` async with `call_id` + `args` params

The current `post_tool` is synchronous. It needs to be async (to shell out) and needs the tool's `call_id` and `args` (to populate `GAIT_ARG_*` env vars and correlate the transcript event).

**Files:**
- Modify: `gait-rs/src/types.rs`
- Modify: `gait-rs/src/turn.rs`

- [ ] **Step 1: Update `Hook` trait in `types.rs`**

Find the `Hook` trait (around line 169) and change it to:

```rust
/// Lifecycle hooks for the agent turn loop. All methods have no-op defaults —
/// implement only the hooks you need.
#[async_trait::async_trait]
pub trait Hook: Send + Sync {
    /// Once per outer turn, before the step loop. Return Some to replace the system prompt.
    fn pre_turn(&self, _system_prompt: &str) -> Option<String> { None }
    /// Before each inner LLM call. Return Some to replace the outgoing message history.
    fn pre_step(&self, _messages: &[Message]) -> Option<Vec<Message>> { None }
    /// After each inner LLM response, before tools execute.
    fn post_step(&self, _text: &str, _tool_calls: &[ToolCall]) {}
    /// Before a tool executes. Return Block to prevent execution.
    fn pre_tool(&self, _tc: &ToolCall) -> HookAction { HookAction::Continue }
    /// After a tool returns. Return Some to replace the result seen by the LLM.
    /// `call_id` correlates to the ToolCall event; `args` are the tool's input args.
    async fn post_tool(
        &self,
        _name: &str,
        _call_id: &str,
        _args: &serde_json::Value,
        _result: &str,
    ) -> Option<String> { None }
    /// Once per outer turn, at every exit point (Done, Clarify, Inform, Permissions).
    fn post_turn(&self) {}
}
```

Note: `async_trait` is already a dependency (used by the `Tool` trait).

- [ ] **Step 2: Update the 3 `post_tool` call sites in `turn.rs`**

Search for all three occurrences of the pattern:
```rust
for h in &hooks {
    if let Some(new) = h.post_tool(&tc.tool_name, &result_str) {
        result_str = new;
    }
}
```

Replace each with:
```rust
for h in &hooks {
    if let Some(new) = h.post_tool(&tc.tool_name, &tc.tool_call_id, &tc.args, &result_str).await {
        result_str = new;
    }
}
```

There are exactly 3 such blocks (around lines 924, 1013, 1092). Use grep to confirm:
```bash
cd gait-rs && grep -n "h.post_tool" src/turn.rs
```

- [ ] **Step 3: Run all lib tests**

```bash
cd gait-rs && cargo test --lib 2>&1 | tail -15
```

Expected: all tests pass (the default async `post_tool` still returns `None`; no behavior change).

- [ ] **Step 4: Commit**

```bash
cd gait-rs && git add src/types.rs src/turn.rs
git commit -m "refactor: make Hook::post_tool async with call_id + args params"
```

---

### Task 4: Add `hook_run` event constructor to `proto/events.rs`

**Files:**
- Modify: `gait-rs/src/proto/events.rs`

- [ ] **Step 1: Add `hook_run()` constructor and `hook_phase()` helper**

Append to `proto/events.rs` after `sandbox_applied` (around line 280):

```rust
/// `HookRun` — config hook execution record.
/// `phase`: "post_tool" | "pre_tool" | "post_turn" | "pre_turn"
pub fn hook_run(
    phase: &str,
    command: &str,
    exit_code: i32,
    output: &str,
    call_id: Option<String>,
) -> pb::event::Body {
    wire::body(pb::payload::Kind::HookRun(pb::HookRun {
        phase: hook_phase(phase),
        command: command.to_string(),
        exit_code,
        output: output.to_string(),
        call_id,
    }))
}

fn hook_phase(s: &str) -> i32 {
    let p = match s {
        "post_tool"  => pb::HookPhase::PostTool,
        "pre_tool"   => pb::HookPhase::PreTool,
        "post_turn"  => pb::HookPhase::PostTurn,
        "pre_turn"   => pb::HookPhase::PreTurn,
        _            => pb::HookPhase::Unspecified,
    };
    p as i32
}
```

- [ ] **Step 2: Verify it compiles**

```bash
cd gait-rs && cargo build 2>&1 | grep -E "^error"
```

Expected: no errors.

- [ ] **Step 3: Commit**

```bash
cd gait-rs && git add src/proto/events.rs
git commit -m "feat: add hook_run event constructor to proto/events"
```

---

### Task 5: Implement `ConfigHook`

**Files:**
- Create: `gait-rs/src/config_hook.rs`

- [ ] **Step 1: Write the failing tests**

Create `gait-rs/src/config_hook.rs` with only the test module (no impl yet):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn hook_def(event: &str, tool: Option<&str>, run: &str) -> crate::config::HookDef {
        crate::config::HookDef {
            event: event.to_string(),
            tool: tool.map(|s| s.to_string()),
            run: run.to_string(),
        }
    }

    #[tokio::test]
    async fn matching_tool_success_returns_none() {
        let dir = TempDir::new().unwrap();
        let tx = dir.path().join("transcript.jsonl");
        let hook = ConfigHook::new(hook_def("post_tool", Some("edit"), "exit 0"), tx);
        let result = hook.post_tool("edit", "call-1", &serde_json::json!({}), "original").await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn matching_tool_failure_appends_output() {
        let dir = TempDir::new().unwrap();
        let tx = dir.path().join("transcript.jsonl");
        let hook = ConfigHook::new(
            hook_def("post_tool", Some("edit"), "echo 'syntax error'; exit 1"),
            tx,
        );
        let result = hook.post_tool("edit", "call-1", &serde_json::json!({}), "original").await;
        let new = result.expect("failure must return Some");
        assert!(new.contains("original"), "must preserve original: {new}");
        assert!(new.contains("syntax error"), "must include hook output: {new}");
        assert!(new.contains("exit 1"), "must include exit code: {new}");
    }

    #[tokio::test]
    async fn non_matching_tool_skips() {
        let dir = TempDir::new().unwrap();
        let tx = dir.path().join("transcript.jsonl");
        let hook = ConfigHook::new(hook_def("post_tool", Some("edit"), "exit 1"), tx);
        let result = hook.post_tool("shell", "call-1", &serde_json::json!({}), "original").await;
        assert!(result.is_none(), "non-matching tool must return None");
    }

    #[tokio::test]
    async fn no_tool_filter_runs_for_all_tools() {
        let dir = TempDir::new().unwrap();
        let tx = dir.path().join("transcript.jsonl");
        let hook = ConfigHook::new(hook_def("post_tool", None, "exit 0"), tx);
        let result = hook.post_tool("shell", "call-1", &serde_json::json!({}), "original").await;
        assert!(result.is_none()); // exit 0 → no modification
    }

    #[tokio::test]
    async fn args_exposed_as_env_vars() {
        let dir = TempDir::new().unwrap();
        let tx = dir.path().join("transcript.jsonl");
        // The command echoes the env var; if it's not set, it would print empty and still exit 0
        // so we check the transcript event for the output
        let hook = ConfigHook::new(
            hook_def("post_tool", Some("edit"), "[ -n \"$GAIT_ARG_FILE_PATH\" ] || exit 1"),
            tx.clone(),
        );
        let args = serde_json::json!({"file_path": "src/main.rs"});
        let result = hook.post_tool("edit", "call-1", &args, "original").await;
        assert!(result.is_none(), "GAIT_ARG_FILE_PATH must be set: {result:?}");
    }

    #[tokio::test]
    async fn wrong_event_type_skips() {
        let dir = TempDir::new().unwrap();
        let tx = dir.path().join("transcript.jsonl");
        let hook = ConfigHook::new(hook_def("pre_tool", Some("edit"), "exit 1"), tx);
        let result = hook.post_tool("edit", "call-1", &serde_json::json!({}), "original").await;
        assert!(result.is_none(), "pre_tool hook must not fire in post_tool");
    }
}
```

- [ ] **Step 2: Run tests to confirm they fail**

```bash
cd gait-rs && cargo test --lib config_hook 2>&1 | tail -10
```

Expected: compile error — `ConfigHook` not defined yet.

- [ ] **Step 3: Implement `ConfigHook`**

Replace the file contents with the full implementation:

```rust
use std::collections::HashMap;
use std::path::PathBuf;

use crate::config::HookDef;
use crate::proto::{events, wire};
use crate::types::{HookAction, Message, ToolCall};
use crate::workflow::hooks::run_hook;

pub struct ConfigHook {
    def: HookDef,
    tx_path: PathBuf,
}

impl ConfigHook {
    pub fn new(def: HookDef, tx_path: PathBuf) -> Self {
        Self { def, tx_path }
    }
}

#[async_trait::async_trait]
impl crate::types::Hook for ConfigHook {
    async fn post_tool(
        &self,
        name: &str,
        call_id: &str,
        args: &serde_json::Value,
        result: &str,
    ) -> Option<String> {
        if self.def.event != "post_tool" {
            return None;
        }
        if let Some(ref filter) = self.def.tool {
            if filter != name {
                return None;
            }
        }

        let mut env: HashMap<String, String> = HashMap::new();
        env.insert("GAIT_TOOL_NAME".into(), name.to_string());
        env.insert("GAIT_TOOL_RESULT".into(), result.to_string());
        if let Some(obj) = args.as_object() {
            for (k, v) in obj {
                let key = format!("GAIT_ARG_{}", k.to_uppercase().replace('-', "_"));
                let val = match v {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                env.insert(key, val);
            }
        }

        let (exit_code, output) = run_hook(&self.def.run, &env).await;

        let _ = wire::emit(
            &self.tx_path,
            events::hook_run("post_tool", &self.def.run, exit_code, &output, Some(call_id.to_string())),
        );

        if exit_code != 0 {
            Some(format!(
                "{result}\n[hook '{}' failed (exit {exit_code})]\n{output}",
                self.def.run
            ))
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    // ... (tests from Step 1 above)
}
```

- [ ] **Step 4: Run tests to confirm they pass**

```bash
cd gait-rs && cargo test --lib config_hook 2>&1 | tail -15
```

Expected: all 6 tests pass.

- [ ] **Step 5: Commit**

```bash
cd gait-rs && git add src/config_hook.rs
git commit -m "feat: add ConfigHook impl — config-driven post-tool shell hooks"
```

---

### Task 6: Wire `ConfigHook` into `ResolvedProfile` + expose module

**Files:**
- Modify: `gait-rs/src/lib.rs`
- Modify: `gait-rs/src/profile.rs`

- [ ] **Step 1: Expose `config_hook` module in `lib.rs`**

In `gait-rs/src/lib.rs`, add `pub mod config_hook;` after `pub mod sandbox;`:

```rust
pub mod sandbox;
pub mod config_hook;
```

- [ ] **Step 2: Build `ConfigHook`s in `Profile::resolve`**

In `profile.rs::resolve`, replace `hooks: vec![]` (line ~151) with:

```rust
let tx_path = workspace.join("sessions").join(&session_id).join("transcript.jsonl");
let hooks: Vec<Box<dyn crate::types::Hook>> = self.hooks
    .iter()
    .map(|def| -> Box<dyn crate::types::Hook> {
        Box::new(crate::config_hook::ConfigHook::new(def.clone(), tx_path.clone()))
    })
    .collect();
```

And update the returned struct field from `hooks: vec![]` to `hooks`.

- [ ] **Step 3: Write a profile wiring test**

Add to `profile.rs` tests (or a new test in `config_hook.rs`):

```rust
#[test]
fn resolved_profile_hooks_populated_from_config() {
    use crate::config::{GaitConfig, HookDef};
    let mut cfg = GaitConfig::default();
    cfg.hooks.push(HookDef {
        event: "post_tool".into(),
        tool: Some("edit".into()),
        run: "exit 0".into(),
    });
    let profile = crate::profile::Profile::from_config_and_agent(&cfg, None, None);
    assert_eq!(profile.hooks.len(), 1);
    assert_eq!(profile.hooks[0].run, "exit 0");
}
```

- [ ] **Step 4: Run all lib tests**

```bash
cd gait-rs && cargo test --lib 2>&1 | tail -15
```

Expected: all tests pass (including new profile wiring test).

- [ ] **Step 5: Commit**

```bash
cd gait-rs && git add src/lib.rs src/profile.rs
git commit -m "feat: wire ConfigHook instances into ResolvedProfile from config"
```

---

### Task 7: Dogfood — add hook to `gait.jsonc` and test

**Files:**
- Modify: `gait.jsonc` (repo root)

- [ ] **Step 1: Add a `hooks` entry to `gait.jsonc`**

Open `/Users/aicoder/Documents/ideation/gait/gait.jsonc` and add the hooks field:

```jsonc
{
  "model": "minimax/MiniMax-M3",
  "allow": ["read", "glob", "grep", "ls", "codesearch", "webfetch", "output", "load", "spawn_agent", "list_agents", "clarify", "inform", "write"],
  "tools": [],
  "hooks": [
    // After each edit, check the file parses as valid Rust. exit 0 = silent; non-zero output
    // is appended to the edit result so the model sees the syntax error immediately.
    { "event": "post_tool", "tool": "edit", "run": "rustfmt --check \"$GAIT_ARG_FILE_PATH\" 2>&1 || true; rustfmt --check \"$GAIT_ARG_FILE_PATH\" 2>&1; [ $? -eq 0 ]" }
  ]
}
```

Actually, simpler — `rustfmt --check` exits 1 if the file isn't parseable. Use it directly:

```jsonc
{
  "model": "minimax/MiniMax-M3",
  "allow": ["read", "glob", "grep", "ls", "codesearch", "webfetch", "output", "load", "spawn_agent", "list_agents", "clarify", "inform", "write"],
  "tools": [],
  "hooks": [
    { "event": "post_tool", "tool": "edit", "run": "rustfmt --check \"$GAIT_ARG_FILE_PATH\"" }
  ]
}
```

- [ ] **Step 2: Install and verify**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && make install
```

Expected: binary installed at `/Users/aicoder/develop/gait`.

- [ ] **Step 3: Smoke test the hook fires**

```bash
/Users/aicoder/develop/gait run \
  --gait-dir /Users/aicoder/Documents/gait-trace \
  --codebase /Users/aicoder/Documents/ideation/gait/gait-rs \
  --auto \
  --task "Read src/sandbox.rs and then make a trivial edit to it (e.g. add a blank line inside the noop_returns_noop test). Report what the edit tool returned."
```

Expected: the agent reports either a clean edit result (if `rustfmt --check` exits 0) or an appended hook failure block (if the blank line somehow confuses rustfmt — unlikely but proves the hook fired). Check the transcript at `gait-trace/sessions/<id>/transcript.jsonl` for a `hook_run` event.

- [ ] **Step 4: Commit**

```bash
cd /Users/aicoder/Documents/ideation/gait
git add gait.jsonc
git commit -m "chore: add rustfmt post-edit hook to dogfood config"
```

---

## Self-Review

**Spec coverage:**
- `HookDef` in config ✓ (Task 2)
- `hooks` flowing through `Profile` → `ResolvedProfile` ✓ (Tasks 2, 6)
- Async `post_tool` with `call_id` + `args` ✓ (Task 3)
- `GAIT_ARG_*` env vars from tool args ✓ (Task 5)
- `HookRun` proto event emitted on every hook execution ✓ (Tasks 1, 4, 5)
- Failure output appended to tool result ✓ (Task 5)
- No language detection — fully user-configured ✓ (Tasks 2, 7)
- `tool` filter optional (None = all tools) ✓ (Task 5)

**No placeholders found.**

**Type consistency:**
- `HookDef` defined in Task 2, used in Tasks 3, 5, 6 — consistent
- `hook_run(phase, command, exit_code, output, call_id)` defined in Task 4, called in Task 5 — consistent
- `post_tool(name, call_id, args, result)` defined in Task 3, implemented in Task 5, called at 3 sites in Task 3 — consistent

---
status: superseded
---

# Unified Tool Permission System Implementation Plan

**Status: SUPERSEDED** — The tasks here are a subset of `2026-06-08-tool-permission-unification.md`, which extends this plan with deny lists, path-aware patterns, CLI subcommands, and a full REPL cleanup. All tasks in this plan are DONE (verified 2026-06-10); see the larger plan for the full implementation status.

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the hardcoded name-based permission logic in `permission.rs`/`turn.rs` with a `permission()` method on the `Tool` trait so every tool — builtin, MCP, or future WASM plugin — declares its own gating behavior.

**Architecture:** Add a `ToolPermission` enum to `types.rs` alongside the `Tool` trait, add a `permission(&self, args: &Value) -> ToolPermission` method (default: `NeedsApproval`), override it on each tool that needs non-default behavior, then update `turn.rs` to dispatch on `tool.permission()` instead of calling `is_tool_allowed`. Remove the now-dead `is_tool_allowed` and `unapproved_segments` from `permission.rs`. `is_segment_allowed` and `command_patterns` stay in `permission.rs` as pure utilities; `ShellTool` and `turn.rs` call them directly.

**Tech Stack:** Rust, `async_trait`, `serde_json::Value`. No new dependencies.

---

## File Map

| File | Action | Purpose |
|------|--------|---------|
| `gait-rs/src/types.rs` | Modify | Add `ToolPermission` enum + `permission()` to `Tool` trait + `ToolDef` default |
| `gait-rs/src/permission.rs` | Modify | Make `is_segment_allowed` pub; remove `is_tool_allowed` + `unapproved_segments` |
| `gait-rs/src/tools/output.rs` | Modify | Override `permission()` → `AlwaysAllow` |
| `gait-rs/src/tools/mcp.rs` | Modify | Override `permission()` → `AlwaysAllow` |
| `gait-rs/src/tools/skill.rs` | Modify | Override `permission()` → `AlwaysAllow` |
| `gait-rs/src/tools/clarify.rs` | Modify | Override `permission()` → `AlwaysAllow` |
| `gait-rs/src/tools/inform.rs` | Modify | Override `permission()` → `AlwaysAllow` |
| `gait-rs/src/tools/shell.rs` | Modify | Override `permission()` → `CommandSegments` |
| `gait-rs/src/turn.rs` | Modify | Replace `is_tool_allowed` call with `tool.permission()` dispatch |

---

## Task 1: Add `ToolPermission` to `types.rs`

**Files:**
- Modify: `gait-rs/src/types.rs`

- [ ] **Step 1: Add the `ToolPermission` enum and `permission()` method**

In `gait-rs/src/types.rs`, add the enum directly before the `Tool` trait definition, and add the method to the trait and its `ToolDef` impl:

```rust
/// How a tool participates in the permission gate.
/// Add this before the `Tool` trait in types.rs.
#[derive(Debug, Clone, PartialEq)]
pub enum ToolPermission {
    /// Never ask for permission — output, clarify, inform, mcp, skill.
    AlwaysAllow,
    /// Ask if tool name is not in the allow list (default for all tools).
    NeedsApproval,
    /// Shell-style: pre-split command segments; ask for each not covered by allow list patterns.
    CommandSegments(Vec<String>),
}
```

Add to the `Tool` trait (after `execution_mode`):

```rust
/// How this tool is gated by the permission system.
/// Override to opt into AlwaysAllow or CommandSegments; default is NeedsApproval.
fn permission(&self, _args: &serde_json::Value) -> ToolPermission {
    ToolPermission::NeedsApproval
}
```

Add to `ToolDef`'s `impl Tool` block (no override needed — the default covers it — but add an explicit impl to keep ToolDef honest):

```rust
// ToolDef already picks up the default NeedsApproval from the trait.
// No explicit override needed.
```

- [ ] **Step 2: Add a unit test for the new enum**

Append to the `#[cfg(test)]` block in `types.rs`:

```rust
#[test]
fn tool_def_permission_defaults_to_needs_approval() {
    let tool = ToolDef {
        name: "mytool".into(),
        description: "".into(),
        parameters: serde_json::json!({}),
        execute: Box::new(|_| Box::pin(async { Ok(serde_json::Value::Null) })),
    };
    assert!(matches!(tool.permission(&serde_json::json!({})), ToolPermission::NeedsApproval));
}

#[test]
fn tool_permission_always_allow_ne_needs_approval() {
    assert_ne!(ToolPermission::AlwaysAllow, ToolPermission::NeedsApproval);
}
```

- [ ] **Step 3: Run the new tests**

```bash
cd gait-rs && cargo test types::tests::tool_def_permission 2>&1 | tail -10
```

Expected: `test types::tests::tool_def_permission_defaults_to_needs_approval ... ok`

- [ ] **Step 4: Commit**

```bash
cd gait-rs && git add src/types.rs && git commit -m "feat: add ToolPermission enum + permission() method to Tool trait"
```

---

## Task 2: Expose `is_segment_allowed` in `permission.rs`

**Files:**
- Modify: `gait-rs/src/permission.rs`

- [ ] **Step 1: Change `is_segment_allowed` to `pub fn`**

Find line 43 in `gait-rs/src/permission.rs`:

```rust
fn is_segment_allowed(segment: &str, allow: &[String]) -> bool {
```

Change to:

```rust
pub fn is_segment_allowed(segment: &str, allow: &[String]) -> bool {
```

- [ ] **Step 2: Run existing tests to confirm nothing broke**

```bash
cd gait-rs && cargo test permission:: 2>&1 | tail -15
```

Expected: all `permission::tests::*` pass.

- [ ] **Step 3: Commit**

```bash
cd gait-rs && git add src/permission.rs && git commit -m "refactor: pub is_segment_allowed — needed by turn.rs after permission() dispatch"
```

---

## Task 3: `AlwaysAllow` on output, clarify, inform

**Files:**
- Modify: `gait-rs/src/tools/output.rs`
- Modify: `gait-rs/src/tools/clarify.rs`
- Modify: `gait-rs/src/tools/inform.rs`

All three need the same change: add `use crate::types::ToolPermission;` at the top of the file and override `permission()` in the `impl Tool` block.

- [ ] **Step 1: Add `permission()` to `OutputTool`**

In `gait-rs/src/tools/output.rs`, add the import at the top:

```rust
use crate::types::{Tool, ToolPermission};
```

Add to `impl Tool for OutputTool` (after the `execute` method):

```rust
fn permission(&self, _args: &serde_json::Value) -> ToolPermission {
    ToolPermission::AlwaysAllow
}
```

Add to the `#[cfg(test)]` block in `output.rs`:

```rust
#[test]
fn permission_is_always_allow() {
    let tool = OutputTool::new();
    assert!(matches!(tool.permission(&serde_json::json!({})), ToolPermission::AlwaysAllow));
}
```

- [ ] **Step 2: Add `permission()` to `ClarifyTool`**

In `gait-rs/src/tools/clarify.rs`, update the import:

```rust
use crate::types::{Tool, ToolPermission};
```

Add to `impl Tool for ClarifyTool`:

```rust
fn permission(&self, _args: &serde_json::Value) -> ToolPermission {
    ToolPermission::AlwaysAllow
}
```

Add test:

```rust
#[test]
fn permission_is_always_allow() {
    let tool = ClarifyTool::new();
    assert!(matches!(tool.permission(&serde_json::json!({})), ToolPermission::AlwaysAllow));
}
```

- [ ] **Step 3: Add `permission()` to `InformTool`**

In `gait-rs/src/tools/inform.rs`, update the import:

```rust
use crate::types::{Tool, ToolPermission};
```

Add to `impl Tool for InformTool`:

```rust
fn permission(&self, _args: &serde_json::Value) -> ToolPermission {
    ToolPermission::AlwaysAllow
}
```

Add test:

```rust
#[test]
fn permission_is_always_allow() {
    let tool = InformTool::new();
    assert!(matches!(tool.permission(&serde_json::json!({})), ToolPermission::AlwaysAllow));
}
```

- [ ] **Step 4: Run the new tests**

```bash
cd gait-rs && cargo test tools::output::tests::permission tools::clarify::tests::permission tools::inform::tests::permission 2>&1 | tail -10
```

Expected: 3 tests pass.

- [ ] **Step 5: Commit**

```bash
cd gait-rs && git add src/tools/output.rs src/tools/clarify.rs src/tools/inform.rs && git commit -m "feat: output/clarify/inform declare AlwaysAllow permission"
```

---

## Task 4: `AlwaysAllow` on mcp and skill

**Files:**
- Modify: `gait-rs/src/tools/mcp.rs`
- Modify: `gait-rs/src/tools/skill.rs`

- [ ] **Step 1: Add `permission()` to `McpTool`**

In `gait-rs/src/tools/mcp.rs`, update import:

```rust
use crate::types::{Tool, ToolPermission};
```

Add to `impl Tool for McpTool`:

```rust
fn permission(&self, _args: &serde_json::Value) -> ToolPermission {
    ToolPermission::AlwaysAllow
}
```

Add test to `#[cfg(test)]` block:

```rust
#[test]
fn permission_is_always_allow() {
    let tool = McpTool::new(vec![]);
    assert!(matches!(tool.permission(&serde_json::json!({})), ToolPermission::AlwaysAllow));
}
```

- [ ] **Step 2: Add `permission()` to `SkillTool`**

In `gait-rs/src/tools/skill.rs`, update import:

```rust
use crate::types::{Tool, ToolPermission};
```

Add to `impl Tool for SkillTool`:

```rust
fn permission(&self, _args: &serde_json::Value) -> ToolPermission {
    ToolPermission::AlwaysAllow
}
```

Add test:

```rust
#[test]
fn permission_is_always_allow() {
    let tool = SkillTool::new(vec![]);
    assert!(matches!(tool.permission(&serde_json::json!({})), ToolPermission::AlwaysAllow));
}
```

- [ ] **Step 3: Run the new tests**

```bash
cd gait-rs && cargo test tools::mcp::tests::permission tools::skill::tests::permission 2>&1 | tail -10
```

Expected: 2 tests pass.

- [ ] **Step 4: Commit**

```bash
cd gait-rs && git add src/tools/mcp.rs src/tools/skill.rs && git commit -m "feat: mcp/skill declare AlwaysAllow permission"
```

---

## Task 5: `CommandSegments` on ShellTool

**Files:**
- Modify: `gait-rs/src/tools/shell.rs`

- [ ] **Step 1: Add the `permission()` override**

In `gait-rs/src/tools/shell.rs`, update import to include `ToolPermission` and the `split_commands` helper:

```rust
use crate::types::{Tool, ToolPermission};
use crate::permission::split_commands;
```

Add to `impl Tool for ShellTool` (after `description`):

```rust
fn permission(&self, args: &serde_json::Value) -> ToolPermission {
    let cmd = args.get("cmd").and_then(|v| v.as_str()).unwrap_or("");
    let segs = split_commands(cmd);
    ToolPermission::CommandSegments(if segs.is_empty() {
        vec![cmd.trim().to_string()]
    } else {
        segs
    })
}
```

- [ ] **Step 2: Add tests for `ShellTool::permission()`**

Append to `#[cfg(test)]` in `shell.rs`:

```rust
#[test]
fn permission_single_cmd_returns_one_segment() {
    let tool = ShellTool::new(
        std::path::PathBuf::from("/tmp"),
        std::path::PathBuf::from("/tmp"),
    );
    let perm = tool.permission(&serde_json::json!({"cmd": "cargo test", "reason": ""}));
    match perm {
        crate::types::ToolPermission::CommandSegments(segs) => {
            assert_eq!(segs, vec!["cargo test"]);
        }
        other => panic!("expected CommandSegments, got {:?}", other),
    }
}

#[test]
fn permission_compound_cmd_returns_multiple_segments() {
    let tool = ShellTool::new(
        std::path::PathBuf::from("/tmp"),
        std::path::PathBuf::from("/tmp"),
    );
    let perm = tool.permission(&serde_json::json!({"cmd": "cargo build && cargo test", "reason": ""}));
    match perm {
        crate::types::ToolPermission::CommandSegments(segs) => {
            assert_eq!(segs, vec!["cargo build", "cargo test"]);
        }
        other => panic!("expected CommandSegments, got {:?}", other),
    }
}

#[test]
fn permission_empty_cmd_returns_one_empty_segment() {
    let tool = ShellTool::new(
        std::path::PathBuf::from("/tmp"),
        std::path::PathBuf::from("/tmp"),
    );
    let perm = tool.permission(&serde_json::json!({"cmd": "", "reason": ""}));
    match perm {
        crate::types::ToolPermission::CommandSegments(segs) => {
            assert_eq!(segs, vec![""]);
        }
        other => panic!("expected CommandSegments, got {:?}", other),
    }
}
```

- [ ] **Step 3: Run the new tests**

```bash
cd gait-rs && cargo test tools::shell::tests::permission 2>&1 | tail -10
```

Expected: 3 tests pass.

- [ ] **Step 4: Commit**

```bash
cd gait-rs && git add src/tools/shell.rs && git commit -m "feat: ShellTool declares CommandSegments permission"
```

---

## Task 6: Wire `turn.rs` to use `tool.permission()`

**Files:**
- Modify: `gait-rs/src/turn.rs`

This is the pivotal task. All existing integration tests in `turn.rs` must pass after this change.

- [ ] **Step 1: Update the imports in `turn.rs`**

Find the import at the top of `gait-rs/src/turn.rs`:

```rust
use crate::permission::{is_tool_allowed, unapproved_segments, command_patterns};
```

Replace with:

```rust
use crate::permission::{is_segment_allowed, command_patterns};
use crate::types::ToolPermission;
```

- [ ] **Step 2: Replace the permission gate loop**

Find this block in `run_turn` (around line 460):

```rust
for tc in &step_tool_calls {
    if !(opts.auto || is_tool_allowed(&tc.tool_name, &tc.args, &effective_allow)) {
        gated_ids.insert(tc.tool_call_id.clone());
        if tc.tool_name == "shell" {
            let cmd = tc.args.get("cmd").and_then(|v| v.as_str()).unwrap_or("");
            for seg in unapproved_segments(cmd, &effective_allow) {
                pending_permissions.push(PermissionItem {
                    id: tc.tool_call_id.clone(),
                    cmd: seg.clone(),
                    reason: "shell command requires permission".into(),
                    patterns: command_patterns(&seg),
                    tool_name: tc.tool_name.clone(),
                    tool_args: tc.args.clone(),
                });
            }
        } else {
            pending_permissions.push(PermissionItem {
                id: tc.tool_call_id.clone(),
                cmd: tc.tool_name.clone(),
                reason: format!("{} requires permission", tc.tool_name),
                patterns: vec![tc.tool_name.clone()],
                tool_name: tc.tool_name.clone(),
                tool_args: tc.args.clone(),
            });
        }
    }
}
```

Replace with:

```rust
for tc in &step_tool_calls {
    let perm = tools.iter()
        .find(|t| t.name() == tc.tool_name)
        .map(|t| t.permission(&tc.args))
        .unwrap_or(ToolPermission::NeedsApproval);

    let allowed = opts.auto || match &perm {
        ToolPermission::AlwaysAllow => true,
        ToolPermission::NeedsApproval => {
            effective_allow.iter().any(|e| !e.contains(':') && e == &tc.tool_name)
        }
        ToolPermission::CommandSegments(segs) => {
            segs.iter().all(|s| is_segment_allowed(s, &effective_allow))
        }
    };

    if !allowed {
        gated_ids.insert(tc.tool_call_id.clone());
        match &perm {
            ToolPermission::CommandSegments(segs) => {
                for seg in segs.iter().filter(|s| !is_segment_allowed(s, &effective_allow)) {
                    pending_permissions.push(PermissionItem {
                        id: tc.tool_call_id.clone(),
                        cmd: seg.clone(),
                        reason: "shell command requires permission".into(),
                        patterns: command_patterns(seg),
                        tool_name: tc.tool_name.clone(),
                        tool_args: tc.args.clone(),
                    });
                }
            }
            _ => {
                pending_permissions.push(PermissionItem {
                    id: tc.tool_call_id.clone(),
                    cmd: tc.tool_name.clone(),
                    reason: format!("{} requires permission", tc.tool_name),
                    patterns: vec![tc.tool_name.clone()],
                    tool_name: tc.tool_name.clone(),
                    tool_args: tc.args.clone(),
                });
            }
        }
    }
}
```

- [ ] **Step 3: Add a new integration test — `AlwaysAllow` tool skips gating**

Append to the `#[cfg(test)]` block in `turn.rs`:

```rust
#[tokio::test]
async fn always_allow_tool_never_gated_in_non_auto_mode() {
    // A custom tool that returns AlwaysAllow should never trigger a Permissions event,
    // even in non-auto mode with an empty allow list.
    use crate::types::ToolPermission;

    let ws = TempDir::new().unwrap();
    init_git_repo(ws.path());

    let provider = Arc::new(MockProvider {
        responses: Arc::new(vec![
            vec![
                StreamChunk::ToolCallComplete {
                    tool_call_id: "tc1".into(),
                    tool_name: "always_allowed".into(),
                    args: serde_json::json!({}),
                },
                StreamChunk::Done,
            ],
            vec![StreamChunk::Text("done".into()), StreamChunk::Done],
        ]),
        call_idx: Arc::new(AtomicUsize::new(0)),
    });

    struct AlwaysAllowedTool;
    #[async_trait::async_trait]
    impl crate::types::Tool for AlwaysAllowedTool {
        fn name(&self) -> &str { "always_allowed" }
        fn description(&self) -> &str { "test tool" }
        fn parameters(&self) -> &serde_json::Value { &serde_json::Value::Null }
        async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<serde_json::Value> {
            Ok(serde_json::Value::String("ok".into()))
        }
        fn permission(&self, _args: &serde_json::Value) -> ToolPermission {
            ToolPermission::AlwaysAllow
        }
    }

    run_turn(
        TurnOpts {
            session_id: "s_always_allow".into(),
            task: Some("test".into()),
            auto: false,
            workspace: ws.path().to_path_buf(),
            codebase_path: ws.path().to_path_buf(),
            allow: vec![],         // empty allow list
            system_prompt: "".into(),
            model: "mock/test".into(),
            mcp_registry: vec![],
            mcp_clients: std::collections::HashMap::new(),
            skill_registry: vec![],
        },
        provider,
        vec![Box::new(AlwaysAllowedTool)],
    ).await.unwrap();

    let tx_path = ws.path().join("sessions/s_always_allow/transcript.jsonl");
    let events = read_events(&tx_path).unwrap();
    assert!(!events.iter().any(|e| matches!(e, AnyEvent::Permissions { .. })),
        "AlwaysAllow tool must never trigger permission gating");
    assert!(events.iter().any(|e| matches!(e, AnyEvent::TurnComplete { .. })),
        "turn must complete");
}

#[tokio::test]
async fn needs_approval_tool_gated_in_non_auto_mode() {
    // A plain ToolDef (NeedsApproval default) not in the allow list must be gated.
    let ws = TempDir::new().unwrap();
    init_git_repo(ws.path());

    let provider = Arc::new(MockProvider {
        responses: Arc::new(vec![
            vec![
                StreamChunk::ToolCallComplete {
                    tool_call_id: "tc1".into(),
                    tool_name: "custom_tool".into(),
                    args: serde_json::json!({}),
                },
                StreamChunk::Done,
            ],
        ]),
        call_idx: Arc::new(AtomicUsize::new(0)),
    });

    let custom_tool = ToolDef {
        name: "custom_tool".into(),
        description: "".into(),
        parameters: serde_json::json!({}),
        execute: Box::new(|_| Box::pin(async { Ok(serde_json::Value::Null) })),
    };

    run_turn(
        TurnOpts {
            session_id: "s_needs_approval".into(),
            task: Some("test".into()),
            auto: false,
            workspace: ws.path().to_path_buf(),
            codebase_path: ws.path().to_path_buf(),
            allow: vec![],    // not in allow list
            system_prompt: "".into(),
            model: "mock/test".into(),
            mcp_registry: vec![],
            mcp_clients: std::collections::HashMap::new(),
            skill_registry: vec![],
        },
        provider,
        vec![Box::new(custom_tool)],
    ).await.unwrap();

    let tx_path = ws.path().join("sessions/s_needs_approval/transcript.jsonl");
    let events = read_events(&tx_path).unwrap();
    assert!(events.iter().any(|e| matches!(e, AnyEvent::Permissions { .. })),
        "NeedsApproval tool not in allow list must trigger permission gating");
}
```

- [ ] **Step 4: Run ALL turn tests**

```bash
cd gait-rs && cargo test turn:: 2>&1 | tail -20
```

Expected: all tests pass including the two new ones and all existing ones.

- [ ] **Step 5: Commit**

```bash
cd gait-rs && git add src/turn.rs && git commit -m "refactor: turn.rs dispatches on tool.permission() — removes hardcoded name list"
```

---

## Task 7: Remove `is_tool_allowed` and `unapproved_segments` from `permission.rs`

**Files:**
- Modify: `gait-rs/src/permission.rs`

Now that `turn.rs` no longer calls `is_tool_allowed` or `unapproved_segments`, both functions and their tests can be deleted.

- [ ] **Step 1: Delete `is_tool_allowed` and `unapproved_segments` from `permission.rs`**

Remove the `is_tool_allowed` function (lines ~63–80 in original):

```rust
// DELETE this entire function:
pub fn is_tool_allowed(
    tool_name: &str,
    tool_args: &serde_json::Value,
    allow: &[String],
) -> bool { ... }
```

Remove the `unapproved_segments` function (lines ~53–60):

```rust
// DELETE this entire function:
pub fn unapproved_segments(cmd: &str, allow: &[String]) -> Vec<String> { ... }
```

Remove their corresponding tests from the `#[cfg(test)]` block:
- `is_allowed_exact_tool`
- `is_allowed_shell_pattern`
- `is_allowed_shell_compound_all_covered`
- `is_allowed_shell_compound_partial_coverage`
- `unapproved_segments_returns_unapproved_only`
- `unapproved_segments_empty_when_all_allowed`

Keep all other tests: `split_*`, `patterns_*`, `wildcard_*`.

- [ ] **Step 2: Verify it compiles and tests pass**

```bash
cd gait-rs && cargo test 2>&1 | tail -20
```

Expected: no compile errors, all tests pass. If anything imports `is_tool_allowed` or `unapproved_segments` elsewhere, the compiler will tell you — fix those imports.

- [ ] **Step 3: Commit**

```bash
cd gait-rs && git add src/permission.rs && git commit -m "refactor: remove is_tool_allowed and unapproved_segments — replaced by Tool::permission()"
```

---

## Self-Review

**Spec coverage:**
- ✅ `ToolPermission` enum with three variants
- ✅ `permission()` on `Tool` trait, default `NeedsApproval`
- ✅ `AlwaysAllow` on output, mcp, skill, clarify, inform
- ✅ `CommandSegments` on shell
- ✅ `turn.rs` dispatches via `tool.permission()`
- ✅ `is_tool_allowed` removed
- ✅ MCP loaded tools (e.g. `slack_send`) get `NeedsApproval` by default via `ToolDef` — no change needed
- ✅ Future WASM tools: implement `Tool`, `permission()` returns `NeedsApproval` by default — nothing extra required

**Placeholder scan:** No TBDs, no "similar to Task N" shortcuts. All code is explicit.

**Type consistency:**
- `ToolPermission` defined in Task 1, used in Tasks 3–6
- `is_segment_allowed` made pub in Task 2, used in Task 6 turn.rs replacement
- `command_patterns` stays pub throughout, used in Task 6
- `split_commands` stays pub, used in Task 5 ShellTool

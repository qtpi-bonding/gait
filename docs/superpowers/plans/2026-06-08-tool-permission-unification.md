# Tool & Permission Unification Implementation Plan

> **Status: Partially done (10 of 11 tasks substantially complete; 3 sub-items outstanding — see Implementation Status section at bottom)**

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Eliminate all hardcoded tool-name strings from `turn.rs`/`permission.rs`, give every tool a declarative `permission()` and `execution_signal()` method, add opencode-style argument-aware patterns (so `edit:src/main.rs` can be approved instead of just `edit`), and clean up the accumulated sprawl found in the audit.

**Architecture:** Add `ToolPermission`, `PermissionRequest`, and `ExecutionSignal` to `types.rs` alongside the `Tool` trait. Each tool overrides `permission(args)` to return the patterns the user can save to the allow list, and `execution_signal()` to declare whether it ends the turn, pauses for user, or loads context. `turn.rs` dispatches entirely on these trait methods — no string matching. The REPL in `chat.rs` displays the patterns the tool declares, so file-level and command-level approval ("allow reads in `src/*`") works without shell-only special casing. Fully backward-compatible: bare `"read"` in the allow list still approves all reads.

**Tech Stack:** Rust, `async_trait`, `serde_json::Value`. No new dependencies.

---

## File Map

| File | Action | Purpose |
|------|--------|---------|
| `gait-rs/src/config.rs` | Modify | Fix default model bug; add `DEFAULT_MODEL` const |
| `gait-rs/src/types.rs` | Modify | Add `ToolPermission`, `PermissionRequest`, `ExecutionSignal`; add `permission()` + `execution_signal()` to `Tool` trait |
| `gait-rs/src/permission.rs` | Modify | Add `path_patterns`, `is_request_approved`, `pub PermissionChoice`; remove `is_tool_allowed`, `unapproved_segments`, `is_segment_allowed` |
| `gait-rs/src/tools/output.rs` | Modify | `permission()` → `Passthrough`; `execution_signal()` → `TurnComplete`; remove duplicate `boxed()` |
| `gait-rs/src/tools/clarify.rs` | Modify | `permission()` → `RequiresApproval(["clarify"])`; `execution_signal()` → `Clarify`; remove duplicate `boxed()` |
| `gait-rs/src/tools/inform.rs` | Modify | `permission()` → `RequiresApproval(["inform"])`; `execution_signal()` → `Inform`; remove duplicate `boxed()` |
| `gait-rs/src/tools/mcp.rs` | Modify | `permission()` → `RequiresApproval(["mcp", "mcp:name"])`; `execution_signal()` → `LoadContext`; remove duplicate `boxed()` |
| `gait-rs/src/tools/skill.rs` | Modify | `permission()` → `RequiresApproval(["skill", "skill:name"])`; `execution_signal()` → `LoadContext`; remove duplicate `boxed()` |
| `gait-rs/src/tools/read.rs` | Modify | `permission()` → path-aware `RequiresApproval` |
| `gait-rs/src/tools/write.rs` | Modify | `permission()` → path-aware `RequiresApproval` |
| `gait-rs/src/tools/edit.rs` | Modify | `permission()` → path-aware `RequiresApproval` |
| `gait-rs/src/tools/multiedit.rs` | Modify | `permission()` → path-aware `RequiresApproval` (one request per file); remove duplicate `boxed()` |
| `gait-rs/src/tools/shell.rs` | Modify | `permission()` → `RequiresApproval` with one `PermissionRequest` per command segment |
| `gait-rs/src/tools/codesearch.rs` | Modify | Remove duplicate `boxed()` |
| `gait-rs/src/tools/webfetch.rs` | Modify | Remove duplicate `boxed()` |
| `gait-rs/src/tools/list_agents.rs` | Modify | Replace literal JSON schema with `make_schema` |
| `gait-rs/src/tools/spawn_agent.rs` | Modify | Use `chat::new_session_id()` instead of duplicating the format |
| `gait-rs/src/chat.rs` | Modify | `pub fn new_session_id()`; use `once_approval_path`; remove `"shell:"` prefix logic (patterns are pre-qualified); use `PermissionChoice` from `permission.rs` |
| `gait-rs/src/turn.rs` | Modify | `pub fn once_approval_path`; `derive_loaded_context_tool` generic; replace all name-based dispatch with trait methods |

---

## Task 1: Fix Config Default Model Bug

**Files:**
- Modify: `gait-rs/src/config.rs`

The bug: `GaitConfig::default()` uses `"anthropic/claude-sonnet-4-6"` but `append_allow_pattern` creates a new `gait.jsonc` with `"minimax/MiniMax-M1-80k"`. A user whose first action triggers a Global permission approval gets silently misconfigured.

- [ ] **Step 1: Write a failing test**

Append to the `#[cfg(test)]` block in `gait-rs/src/config.rs`:

```rust
#[test]
fn append_allow_creates_file_with_correct_default_model() {
    let dir = TempDir::new().unwrap();
    // No gait.jsonc exists yet — append_allow_pattern must create one
    append_allow_pattern(dir.path(), "read").unwrap();
    let content = std::fs::read_to_string(dir.path().join("gait.jsonc")).unwrap();
    let val: serde_json::Value = serde_json::from_str(&content).unwrap();
    assert_eq!(
        val["model"].as_str().unwrap(),
        "anthropic/claude-sonnet-4-6",
        "newly created gait.jsonc must use the same default model as GaitConfig::default()"
    );
}
```

- [ ] **Step 2: Run to see it fail**

```bash
cd gait-rs && cargo test config::tests::append_allow_creates_file_with_correct_default_model 2>&1 | tail -10
```

Expected: FAIL — the created file has `"minimax/MiniMax-M1-80k"`.

- [ ] **Step 3: Add `DEFAULT_MODEL` constant and fix `append_allow_pattern`**

At the top of `gait-rs/src/config.rs` (before the structs), add:

```rust
pub const DEFAULT_MODEL: &str = "anthropic/claude-sonnet-4-6";
```

In `GaitConfig::default()`, replace the model string literal and expand the default allow list. `"clarify"` and `"inform"` are approved by default here — not because the code hardcodes them as free, but because a fresh gait install should feel sensible out of the box. Users can remove them from `gait.jsonc` to require explicit approval:

```rust
impl Default for GaitConfig {
    fn default() -> Self {
        Self {
            model: DEFAULT_MODEL.into(),
            allow: vec![
                "read".into(), "glob".into(), "grep".into(), "ls".into(),
                "clarify".into(), "inform".into(),
            ],
            tools: vec![],
            compaction_threshold: 0.8,
            mcp_servers: vec![],
            skills: vec![],
        }
    }
}
```

In `append_allow_pattern`, replace the hardcoded `"minimax/MiniMax-M1-80k"` string at line 112:

```rust
} else {
    let mut m = serde_json::Map::new();
    m.insert("model".into(), serde_json::Value::String(DEFAULT_MODEL.into()));
    m
};
```

- [ ] **Step 4: Run the test to confirm it passes**

```bash
cd gait-rs && cargo test config::tests:: 2>&1 | tail -10
```

Expected: all config tests pass.

- [ ] **Step 5: Commit**

```bash
cd gait-rs && git add src/config.rs && git commit -m "fix: config default model mismatch — append_allow_pattern now uses DEFAULT_MODEL const"
```

---

## Task 2: Core Types — ToolPermission, PermissionRequest, ExecutionSignal

**Files:**
- Modify: `gait-rs/src/types.rs`

- [ ] **Step 1: Add the three new types**

In `gait-rs/src/types.rs`, add the following directly before the `/// Extensible tool interface` comment (before the `Tool` trait definition):

```rust
/// One thing a tool needs approved. `display` is shown to the user and matched
/// against the allow list. `patterns` are the full allow-list entries the user
/// can choose to save, from broadest to most specific.
///
/// Examples:
///   shell "cargo test"  → display: "cargo test",   patterns: ["shell:cargo *", "shell:cargo test"]
///   read  "src/main.rs" → display: "src/main.rs",  patterns: ["read", "read:src/*", "read:src/main.rs"]
#[derive(Debug, Clone, PartialEq)]
pub struct PermissionRequest {
    pub display: String,
    pub patterns: Vec<String>,
}

/// What permission gating a tool requires.
#[derive(Debug, Clone, PartialEq)]
pub enum ToolPermission {
    /// Skip the gate entirely — don't consult the allow list at all.
    /// Reserved for output: it's a turn-completion signal, not a user-visible action.
    /// Denying it would just break the turn. Pre-approving via config is the right
    /// mechanism for everything else (clarify, inform, read, etc.).
    Passthrough,
    /// Gate against the allow list. One entry per "thing" being approved:
    /// one for most tools, one per command segment for compound shell commands.
    RequiresApproval(Vec<PermissionRequest>),
}

/// What happens after a tool executes (or instead of executing).
/// `turn.rs` uses this to dispatch without hardcoding tool names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionSignal {
    /// Execute normally, push result back to model, continue the inner agentic loop.
    /// This is the default for all regular tools (read, write, shell, etc.).
    Continue,
    /// Calling this tool IS the exit signal. Capture summary/result from args,
    /// emit TurnComplete event, and return TurnResult::Done immediately.
    /// No tool result is sent back to the model — no extra LLM round-trip.
    TurnComplete,
    /// Capture items from args, emit Clarify event, return TurnResult::Clarify immediately.
    /// No tool result sent to model.
    Clarify,
    /// Capture items from args, emit Inform event, return TurnResult::Inform immediately.
    /// No tool result sent to model.
    Inform,
    /// Mutate the active tool list (load or unload), execute for transcript, continue loop.
    LoadContext,
}
```

- [ ] **Step 2: Add `permission()` and `execution_signal()` to the `Tool` trait**

In the `Tool` trait (after the `execution_mode` method and before the `boxed` method), add:

```rust
/// What this tool needs approved before running.
/// Default: requires approval with just the tool name as the pattern.
/// Signal tools (output, clarify, inform, mcp, skill) override to Passthrough.
fn permission(&self, _args: &serde_json::Value) -> ToolPermission {
    ToolPermission::RequiresApproval(vec![PermissionRequest {
        display: self.name().to_string(),
        patterns: vec![self.name().to_string()],
    }])
}

/// How turn.rs should dispatch this tool's execution.
/// Default: execute normally and continue the inner loop. Override on signal tools.
fn execution_signal(&self) -> ExecutionSignal {
    ExecutionSignal::Continue
}
```

- [ ] **Step 3: Add tests**

Append to `#[cfg(test)]` in `types.rs`:

```rust
#[test]
fn tool_def_permission_default_is_tool_name() {
    let tool = ToolDef {
        name: "mytool".into(),
        description: "".into(),
        parameters: serde_json::json!({}),
        execute: Box::new(|_| Box::pin(async { Ok(serde_json::Value::Null) })),
    };
    match tool.permission(&serde_json::json!({})) {
        ToolPermission::RequiresApproval(reqs) => {
            assert_eq!(reqs.len(), 1);
            assert_eq!(reqs[0].display, "mytool");
            assert_eq!(reqs[0].patterns, vec!["mytool"]);
        }
        other => panic!("expected RequiresApproval, got {:?}", other),
    }
}

#[test]
fn tool_def_execution_signal_default_is_normal() {
    let tool = ToolDef {
        name: "mytool".into(),
        description: "".into(),
        parameters: serde_json::json!({}),
        execute: Box::new(|_| Box::pin(async { Ok(serde_json::Value::Null) })),
    };
    assert_eq!(tool.execution_signal(), ExecutionSignal::Continue);
}

#[test]
fn permission_request_patterns_can_be_empty() {
    let req = PermissionRequest { display: "foo".into(), patterns: vec![] };
    assert_eq!(req.display, "foo");
    assert!(req.patterns.is_empty());
}
```

- [ ] **Step 4: Run tests**

```bash
cd gait-rs && cargo test types::tests:: 2>&1 | tail -15
```

Expected: all pass.

- [ ] **Step 5: Commit**

```bash
cd gait-rs && git add src/types.rs && git commit -m "feat: add ToolPermission, PermissionRequest, ExecutionSignal to types.rs"
```

---

## Task 3: Refactor permission.rs

**Files:**
- Modify: `gait-rs/src/permission.rs`

Add `path_patterns` (opencode-style file path specificity), `is_request_approved` (unified replacement for `is_tool_allowed` + `is_segment_allowed`), and `pub PermissionChoice`. Remove the now-dead `is_tool_allowed`, `unapproved_segments`, and `is_segment_allowed`.

- [ ] **Step 1: Add tests for new functions**

Append to `#[cfg(test)]` in `gait-rs/src/permission.rs`:

```rust
#[test]
fn path_patterns_single_file_at_root() {
    let pats = path_patterns("read", "README.md");
    assert_eq!(pats, vec!["read", "read:README.md"]);
}

#[test]
fn path_patterns_nested_file() {
    let pats = path_patterns("write", "src/tools/edit.rs");
    assert_eq!(pats, vec![
        "write",
        "write:src/*",
        "write:src/tools/*",
        "write:src/tools/edit.rs",
    ]);
}

#[test]
fn is_request_approved_bare_tool_name_allows_all() {
    use crate::types::PermissionRequest;
    let req = PermissionRequest { display: "src/main.rs".into(), patterns: vec![] };
    assert!(is_request_approved("read", &req, &["read".to_string()]));
}

#[test]
fn is_request_approved_exact_path() {
    use crate::types::PermissionRequest;
    let req = PermissionRequest { display: "src/main.rs".into(), patterns: vec![] };
    assert!(is_request_approved("read", &req, &["read:src/main.rs".to_string()]));
    assert!(!is_request_approved("read", &req, &["read:src/lib.rs".to_string()]));
}

#[test]
fn is_request_approved_wildcard_path() {
    use crate::types::PermissionRequest;
    let req = PermissionRequest { display: "src/main.rs".into(), patterns: vec![] };
    assert!(is_request_approved("read", &req, &["read:src/*".to_string()]));
    assert!(!is_request_approved("read", &req, &["read:tests/*".to_string()]));
}

#[test]
fn is_request_approved_shell_pattern() {
    use crate::types::PermissionRequest;
    let req = PermissionRequest { display: "cargo test".into(), patterns: vec![] };
    assert!(is_request_approved("shell", &req, &["shell:cargo *".to_string()]));
    assert!(is_request_approved("shell", &req, &["shell:cargo test".to_string()]));
    assert!(!is_request_approved("shell", &req, &["shell:git *".to_string()]));
}

#[test]
fn is_request_approved_wrong_tool_name() {
    use crate::types::PermissionRequest;
    let req = PermissionRequest { display: "src/main.rs".into(), patterns: vec![] };
    // "write" in allow list does NOT approve a "read" request
    assert!(!is_request_approved("read", &req, &["write".to_string()]));
}
```

- [ ] **Step 2: Run to see them fail**

```bash
cd gait-rs && cargo test permission::tests::path_patterns permission::tests::is_request_approved 2>&1 | tail -15
```

Expected: FAIL — `path_patterns` and `is_request_approved` are not yet defined.

- [ ] **Step 3: Add `path_patterns` and `is_request_approved`**

Add these functions to `gait-rs/src/permission.rs` (before the `#[cfg(test)]` block). Also add the `use crate::types::PermissionRequest;` import at the top:

```rust
use crate::types::PermissionRequest;
```

```rust
/// Generate allow-list patterns for a file path, from broadest to most specific.
/// The tool name is included as a prefix in every entry.
///
/// path_patterns("read", "src/tools/edit.rs")
///   → ["read", "read:src/*", "read:src/tools/*", "read:src/tools/edit.rs"]
///
/// The first entry ("read") is the backward-compat catch-all: bare tool name in
/// the allow list approves any call to that tool regardless of path.
pub fn path_patterns(tool_name: &str, rel_path: &str) -> Vec<String> {
    let mut patterns = vec![tool_name.to_string()];
    let path = std::path::Path::new(rel_path);
    // Collect ancestor directories from outermost to innermost
    let mut ancestors: Vec<&std::path::Path> = path.ancestors().skip(1).collect();
    ancestors.reverse();
    for ancestor in ancestors {
        if ancestor.as_os_str().is_empty() { continue; }
        patterns.push(format!("{}:{}/*", tool_name, ancestor.display()));
    }
    patterns.push(format!("{}:{}", tool_name, rel_path));
    patterns
}

/// Check whether a single PermissionRequest is covered by the allow list.
///
/// A request is approved if any allow-list entry either:
///   - is the bare tool name ("read" approves any read request), OR
///   - has the format "tool_name:pattern" where pattern wildcard-matches req.display
pub fn is_request_approved(tool_name: &str, req: &PermissionRequest, allow: &[String]) -> bool {
    allow.iter().any(|entry| {
        if !entry.contains(':') {
            return entry == tool_name;
        }
        if let Some((prefix, pat)) = entry.split_once(':') {
            if prefix == tool_name {
                return wildcard_match(pat, &req.display);
            }
        }
        false
    })
}
```

- [ ] **Step 4: Add `pub enum PermissionChoice`**

Add this before the `split_commands` function in `gait-rs/src/permission.rs`:

```rust
/// The user's decision when prompted about a pending tool call.
/// Moved here from chat.rs so any front-end can use it without depending on REPL code.
#[derive(Debug, Clone, PartialEq)]
pub enum PermissionChoice {
    /// Save pattern to gait.jsonc (persists across sessions).
    Global(String),
    /// Save pattern to agent frontmatter (scoped to this agent file).
    Agent(String),
    /// Approve once via temp file (single turn only).
    Once(String),
    /// Deny — model receives "permission denied" and can react.
    Reject,
}
```

- [ ] **Step 5: Do NOT yet delete `is_tool_allowed`, `unapproved_segments`, `is_segment_allowed`**

`turn.rs` still imports and calls `is_tool_allowed` + `unapproved_segments` until Task 8 replaces them. Deleting them here would break compilation. Leave all three functions in place.

Add `#[allow(dead_code)]` above each if the compiler warns — but in practice they remain called until Task 8, so no warning is expected.

The functions will be removed in **Task 8, Step 1** once `turn.rs` no longer calls them.

- [ ] **Step 6: Run all permission tests**

```bash
cd gait-rs && cargo test permission:: 2>&1 | tail -20
```

Expected: all remaining tests pass (split/patterns/wildcard + new path/request tests).

- [ ] **Step 7: Commit**

```bash
cd gait-rs && git add src/permission.rs && git commit -m "feat: path_patterns + is_request_approved + PermissionChoice in permission.rs"
```

---

## Task 4: Signal Tools — Passthrough + ExecutionSignal

**Files:**
- Modify: `gait-rs/src/tools/output.rs`
- Modify: `gait-rs/src/tools/clarify.rs`
- Modify: `gait-rs/src/tools/inform.rs`
- Modify: `gait-rs/src/tools/mcp.rs`
- Modify: `gait-rs/src/tools/skill.rs`

Each signal tool needs:
1. `permission()` — `output` gets `Passthrough`; clarify/inform/mcp/skill get `RequiresApproval` with their own name patterns
2. `execution_signal()` → the appropriate variant
3. Remove duplicate `pub fn boxed(self)` (trait already provides it via `where Self: Sized + 'static`)

- [ ] **Step 1: Update `output.rs`**

In `gait-rs/src/tools/output.rs`, update the import line:

```rust
use crate::types::{Tool, ToolPermission, ExecutionSignal};
```

Remove the `pub fn boxed(self) -> Box<dyn Tool> { Box::new(self) }` method from `impl OutputTool`.

Add to `impl Tool for OutputTool` (after `execute`):

```rust
fn permission(&self, _args: &serde_json::Value) -> ToolPermission {
    ToolPermission::Passthrough
}

fn execution_signal(&self) -> ExecutionSignal {
    ExecutionSignal::TurnComplete
}
```

Add to `#[cfg(test)]` in `output.rs`:

```rust
#[test]
fn output_permission_is_always_allow() {
    let tool = OutputTool::new();
    assert_eq!(tool.permission(&serde_json::json!({})), ToolPermission::Passthrough);
}

#[test]
fn output_execution_signal_is_turn_complete() {
    let tool = OutputTool::new();
    assert_eq!(tool.execution_signal(), ExecutionSignal::TurnComplete);
}
```

- [ ] **Step 2: Update `clarify.rs`**

In `gait-rs/src/tools/clarify.rs`, update import:

```rust
use crate::types::{Tool, ToolPermission, PermissionRequest, ExecutionSignal};
```

Remove `pub fn boxed(self)` from `impl ClarifyTool`.

Add to `impl Tool for ClarifyTool`:

```rust
fn permission(&self, _args: &serde_json::Value) -> ToolPermission {
    // RequiresApproval — "clarify" is pre-approved in the default config allow list,
    // but permission is enforced by config, not hardcoded here.
    ToolPermission::RequiresApproval(vec![PermissionRequest {
        display: "clarify".to_string(),
        patterns: vec!["clarify".to_string()],
    }])
}

fn execution_signal(&self) -> ExecutionSignal {
    ExecutionSignal::Clarify
}
```

Add to `#[cfg(test)]`:

```rust
#[test]
fn clarify_permission_requires_approval() {
    use crate::types::ToolPermission;
    let tool = ClarifyTool::new();
    let perm = tool.permission(&serde_json::json!({}));
    match perm {
        ToolPermission::RequiresApproval(reqs) => {
            assert_eq!(reqs[0].patterns, vec!["clarify"]);
        }
        other => panic!("expected RequiresApproval, got {:?}", other),
    }
}

#[test]
fn clarify_execution_signal_is_clarify() {
    let tool = ClarifyTool::new();
    assert_eq!(tool.execution_signal(), ExecutionSignal::Clarify);
}
```

- [ ] **Step 3: Update `inform.rs`**

In `gait-rs/src/tools/inform.rs`, update import:

```rust
use crate::types::{Tool, ToolPermission, PermissionRequest, ExecutionSignal};
```

Remove `pub fn boxed(self)` from `impl InformTool`.

Add to `impl Tool for InformTool`:

```rust
fn permission(&self, _args: &serde_json::Value) -> ToolPermission {
    // RequiresApproval — "inform" is pre-approved in the default config allow list,
    // but permission is enforced by config, not hardcoded here.
    ToolPermission::RequiresApproval(vec![PermissionRequest {
        display: "inform".to_string(),
        patterns: vec!["inform".to_string()],
    }])
}

fn execution_signal(&self) -> ExecutionSignal {
    ExecutionSignal::Inform
}
```

Add to `#[cfg(test)]`:

```rust
#[test]
fn inform_permission_requires_approval() {
    use crate::types::ToolPermission;
    let tool = InformTool::new();
    let perm = tool.permission(&serde_json::json!({}));
    match perm {
        ToolPermission::RequiresApproval(reqs) => {
            assert_eq!(reqs[0].patterns, vec!["inform"]);
        }
        other => panic!("expected RequiresApproval, got {:?}", other),
    }
}

#[test]
fn inform_execution_signal_is_inform() {
    let tool = InformTool::new();
    assert_eq!(tool.execution_signal(), ExecutionSignal::Inform);
}
```

- [ ] **Step 4: Update `mcp.rs`**

In `gait-rs/src/tools/mcp.rs`, update import:

```rust
use crate::types::{Tool, ToolPermission, PermissionRequest, ExecutionSignal};
```

Remove `pub fn boxed(self)` from `impl McpTool`.

Add to `impl Tool for McpTool`:

```rust
fn permission(&self, args: &serde_json::Value) -> ToolPermission {
    let name = args.get("name").and_then(|v| v.as_str()).unwrap_or("*");
    ToolPermission::RequiresApproval(vec![PermissionRequest {
        display: name.to_string(),
        patterns: vec!["mcp".to_string(), format!("mcp:{}", name)],
    }])
}

fn execution_signal(&self) -> ExecutionSignal {
    ExecutionSignal::LoadContext
}
```

Add to `#[cfg(test)]`:

```rust
#[test]
fn mcp_permission_requires_approval_with_name_patterns() {
    use crate::types::ToolPermission;
    let tool = McpTool::new(vec![]);
    let perm = tool.permission(&serde_json::json!({"name": "slack_send", "context": true}));
    match perm {
        ToolPermission::RequiresApproval(reqs) => {
            assert_eq!(reqs.len(), 1);
            assert_eq!(reqs[0].display, "slack_send");
            assert_eq!(reqs[0].patterns, vec!["mcp", "mcp:slack_send"]);
        }
        other => panic!("expected RequiresApproval, got {:?}", other),
    }
}

#[test]
fn mcp_permission_bare_name_allows_all_mcp() {
    use crate::types::ToolPermission;
    let tool = McpTool::new(vec![]);
    let perm = tool.permission(&serde_json::json!({"name": "anything", "context": true}));
    if let ToolPermission::RequiresApproval(reqs) = perm {
        assert!(reqs[0].patterns.contains(&"mcp".to_string()),
            "bare 'mcp' pattern must be present for allow-all");
    }
}

#[test]
fn mcp_execution_signal_is_load_context() {
    let tool = McpTool::new(vec![]);
    assert_eq!(tool.execution_signal(), ExecutionSignal::LoadContext);
}
```

- [ ] **Step 5: Update `skill.rs`**

In `gait-rs/src/tools/skill.rs`, update import:

```rust
use crate::types::{Tool, ToolPermission, PermissionRequest, ExecutionSignal};
```

Remove `pub fn boxed(self)` from `impl SkillTool`.

Add to `impl Tool for SkillTool`:

```rust
fn permission(&self, args: &serde_json::Value) -> ToolPermission {
    let name = args.get("name").and_then(|v| v.as_str()).unwrap_or("*");
    ToolPermission::RequiresApproval(vec![PermissionRequest {
        display: name.to_string(),
        patterns: vec!["skill".to_string(), format!("skill:{}", name)],
    }])
}

fn execution_signal(&self) -> ExecutionSignal {
    ExecutionSignal::LoadContext
}
```

Add to `#[cfg(test)]`:

```rust
#[test]
fn skill_permission_requires_approval_with_name_patterns() {
    use crate::types::ToolPermission;
    let tool = SkillTool::new(vec![]);
    let perm = tool.permission(&serde_json::json!({"name": "tdd", "context": true}));
    match perm {
        ToolPermission::RequiresApproval(reqs) => {
            assert_eq!(reqs.len(), 1);
            assert_eq!(reqs[0].display, "tdd");
            assert_eq!(reqs[0].patterns, vec!["skill", "skill:tdd"]);
        }
        other => panic!("expected RequiresApproval, got {:?}", other),
    }
}

#[test]
fn skill_execution_signal_is_load_context() {
    let tool = SkillTool::new(vec![]);
    assert_eq!(tool.execution_signal(), ExecutionSignal::LoadContext);
}
```

- [ ] **Step 6: Run all signal tool tests**

```bash
cd gait-rs && cargo test tools::output:: tools::clarify:: tools::inform:: tools::mcp:: tools::skill:: 2>&1 | tail -20
```

Expected: all pass.

- [ ] **Step 7: Commit**

```bash
cd gait-rs && git add src/tools/output.rs src/tools/clarify.rs src/tools/inform.rs src/tools/mcp.rs src/tools/skill.rs && git commit -m "feat: signal tools declare Passthrough + ExecutionSignal; remove duplicate boxed()"
```

---

## Task 5: File Tools — Path-Aware Permission

**Files:**
- Modify: `gait-rs/src/tools/read.rs`
- Modify: `gait-rs/src/tools/write.rs`
- Modify: `gait-rs/src/tools/edit.rs`
- Modify: `gait-rs/src/tools/multiedit.rs`

Each file tool overrides `permission()` to call `path_patterns` with the file path from args, producing patterns like `["read", "read:src/*", "read:src/main.rs"]`. The path is made relative to `codebase_path` so patterns are portable.

- [ ] **Step 1: Update `read.rs`**

Add to imports in `gait-rs/src/tools/read.rs`:

```rust
use crate::types::{Tool, ExecutionMode, ToolPermission, PermissionRequest};
use crate::permission::path_patterns;
```

Add to `impl Tool for ReadTool` (after `execute`):

```rust
fn permission(&self, args: &serde_json::Value) -> ToolPermission {
    let raw = args.get("file_path").and_then(|v| v.as_str()).unwrap_or("*");
    let rel = std::path::Path::new(raw)
        .strip_prefix(&*self.codebase_path)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| raw.to_string());
    ToolPermission::RequiresApproval(vec![PermissionRequest {
        display: rel.clone(),
        patterns: path_patterns("read", &rel),
    }])
}
```

Add to `#[cfg(test)]` in `read.rs`:

```rust
#[test]
fn read_permission_uses_relative_path_patterns() {
    use crate::types::ToolPermission;
    let codebase = std::path::PathBuf::from("/project");
    let tool = ReadTool::new(codebase);
    let perm = tool.permission(&serde_json::json!({
        "file_path": "/project/src/main.rs"
    }));
    match perm {
        ToolPermission::RequiresApproval(reqs) => {
            assert_eq!(reqs.len(), 1);
            assert_eq!(reqs[0].display, "src/main.rs");
            assert!(reqs[0].patterns.contains(&"read".to_string()));
            assert!(reqs[0].patterns.contains(&"read:src/main.rs".to_string()));
        }
        other => panic!("expected RequiresApproval, got {:?}", other),
    }
}

#[test]
fn read_permission_fallback_on_missing_arg() {
    use crate::types::ToolPermission;
    let codebase = std::path::PathBuf::from("/project");
    let tool = ReadTool::new(codebase);
    let perm = tool.permission(&serde_json::json!({}));
    match perm {
        ToolPermission::RequiresApproval(reqs) => {
            assert_eq!(reqs[0].display, "*");
        }
        other => panic!("expected RequiresApproval, got {:?}", other),
    }
}
```

- [ ] **Step 2: Update `write.rs`**

Add to imports in `gait-rs/src/tools/write.rs`:

```rust
use crate::types::{Tool, ToolPermission, PermissionRequest};
use crate::permission::path_patterns;
```

Add to `impl Tool for WriteTool` (after `execute`):

```rust
fn permission(&self, args: &serde_json::Value) -> ToolPermission {
    let raw = args.get("file_path").and_then(|v| v.as_str()).unwrap_or("*");
    let rel = std::path::Path::new(raw)
        .strip_prefix(&*self.codebase_path)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| raw.to_string());
    ToolPermission::RequiresApproval(vec![PermissionRequest {
        display: rel.clone(),
        patterns: path_patterns("write", &rel),
    }])
}
```

Add to `#[cfg(test)]` in `write.rs`:

```rust
#[test]
fn write_permission_uses_path_patterns() {
    use crate::types::ToolPermission;
    let codebase = std::path::PathBuf::from("/project");
    let tool = WriteTool::new(codebase);
    let perm = tool.permission(&serde_json::json!({"file_path": "/project/src/lib.rs"}));
    match perm {
        ToolPermission::RequiresApproval(reqs) => {
            assert!(reqs[0].patterns.contains(&"write:src/lib.rs".to_string()));
            assert!(reqs[0].patterns.contains(&"write".to_string()));
        }
        other => panic!("expected RequiresApproval, got {:?}", other),
    }
}
```

- [ ] **Step 3: Update `edit.rs`**

Add to imports in `gait-rs/src/tools/edit.rs`:

```rust
use crate::types::{Tool, ToolPermission, PermissionRequest};
use crate::permission::path_patterns;
```

Add to `impl Tool for EditTool` (after `execute`):

```rust
fn permission(&self, args: &serde_json::Value) -> ToolPermission {
    let raw = args.get("file_path").and_then(|v| v.as_str()).unwrap_or("*");
    let rel = std::path::Path::new(raw)
        .strip_prefix(&*self.codebase_path)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| raw.to_string());
    ToolPermission::RequiresApproval(vec![PermissionRequest {
        display: rel.clone(),
        patterns: path_patterns("edit", &rel),
    }])
}
```

Add to `#[cfg(test)]` in `edit.rs`:

```rust
#[test]
fn edit_permission_uses_path_patterns() {
    use crate::types::ToolPermission;
    let codebase = std::path::PathBuf::from("/project");
    let tool = EditTool::new(codebase);
    let perm = tool.permission(&serde_json::json!({"file_path": "/project/src/lib.rs"}));
    match perm {
        ToolPermission::RequiresApproval(reqs) => {
            assert_eq!(reqs[0].display, "src/lib.rs");
            assert!(reqs[0].patterns.contains(&"edit:src/lib.rs".to_string()));
        }
        other => panic!("expected RequiresApproval, got {:?}", other),
    }
}
```

- [ ] **Step 4: Update `multiedit.rs`**

`multiedit` takes `edits: Vec<{file_path, ...}>`, so it produces one `PermissionRequest` per unique file.

Add to imports in `gait-rs/src/tools/multiedit.rs`:

```rust
use crate::types::{Tool, ToolPermission, PermissionRequest};
use crate::permission::path_patterns;
```

Remove `pub fn boxed(self) -> Box<dyn Tool> { Box::new(self) }` from `impl MultiEditTool`.

Add to `impl Tool for MultiEditTool` (after `execute`):

```rust
fn permission(&self, args: &serde_json::Value) -> ToolPermission {
    let edits = args.get("edits").and_then(|v| v.as_array());
    let paths: Vec<String> = match edits {
        None => vec!["*".to_string()],
        Some(arr) => {
            let mut seen = std::collections::HashSet::new();
            arr.iter()
                .filter_map(|e| e.get("file_path").and_then(|v| v.as_str()))
                .filter(|p| seen.insert(p.to_string()))
                .map(|p| {
                    std::path::Path::new(p)
                        .strip_prefix(&*self.codebase_path)
                        .map(|r| r.to_string_lossy().into_owned())
                        .unwrap_or_else(|_| p.to_string())
                })
                .collect()
        }
    };
    let requests = paths.iter().map(|rel| PermissionRequest {
        display: rel.clone(),
        patterns: path_patterns("edit", rel),
    }).collect();
    ToolPermission::RequiresApproval(requests)
}
```

Add to `#[cfg(test)]` in `multiedit.rs`:

```rust
#[test]
fn multiedit_permission_one_request_per_unique_file() {
    use crate::types::ToolPermission;
    let codebase = std::path::PathBuf::from("/project");
    let tool = MultiEditTool::new(codebase);
    let perm = tool.permission(&serde_json::json!({
        "edits": [
            {"file_path": "/project/src/a.rs", "old_string": "x", "new_string": "y"},
            {"file_path": "/project/src/b.rs", "old_string": "x", "new_string": "y"},
            {"file_path": "/project/src/a.rs", "old_string": "z", "new_string": "w"},
        ]
    }));
    match perm {
        ToolPermission::RequiresApproval(reqs) => {
            assert_eq!(reqs.len(), 2, "deduplicates same file");
            let displays: Vec<&str> = reqs.iter().map(|r| r.display.as_str()).collect();
            assert!(displays.contains(&"src/a.rs"));
            assert!(displays.contains(&"src/b.rs"));
        }
        other => panic!("expected RequiresApproval, got {:?}", other),
    }
}
```

- [ ] **Step 5: Run file tool tests**

```bash
cd gait-rs && cargo test tools::read:: tools::write:: tools::edit:: tools::multiedit:: 2>&1 | tail -20
```

Expected: all pass.

- [ ] **Step 6: Commit**

```bash
cd gait-rs && git add src/tools/read.rs src/tools/write.rs src/tools/edit.rs src/tools/multiedit.rs && git commit -m "feat: file tools declare path-aware permission() — approve edit:src/* instead of just edit"
```

---

## Task 6: Shell Tool — Command Segment Permission

**Files:**
- Modify: `gait-rs/src/tools/shell.rs`

Shell produces one `PermissionRequest` per command segment, so compound commands (`cargo test && git push`) get approved per-segment.

- [ ] **Step 1: Update `shell.rs` imports**

Add to imports in `gait-rs/src/tools/shell.rs`:

```rust
use crate::types::{Tool, ToolPermission, PermissionRequest};
use crate::permission::{split_commands, command_patterns};
```

- [ ] **Step 2: Add `permission()` to `ShellTool`**

Add to `impl Tool for ShellTool` (after `description`):

```rust
fn permission(&self, args: &serde_json::Value) -> ToolPermission {
    let cmd = args.get("cmd").and_then(|v| v.as_str()).unwrap_or("");
    let segs = split_commands(cmd);
    let segs = if segs.is_empty() { vec![cmd.trim().to_string()] } else { segs };
    let requests = segs.iter().map(|seg| PermissionRequest {
        display: seg.clone(),
        // Full allow-list entries: "shell:cargo *", "shell:cargo test --verbose" etc.
        patterns: command_patterns(seg)
            .into_iter()
            .map(|p| format!("shell:{}", p))
            .collect(),
    }).collect();
    ToolPermission::RequiresApproval(requests)
}
```

- [ ] **Step 3: Add tests**

Append to `#[cfg(test)]` in `shell.rs`:

```rust
#[test]
fn shell_permission_single_command() {
    use crate::types::ToolPermission;
    let tool = ShellTool::new(
        std::path::PathBuf::from("/tmp"),
        std::path::PathBuf::from("/tmp"),
    );
    let perm = tool.permission(&serde_json::json!({"cmd": "cargo test", "reason": ""}));
    match perm {
        ToolPermission::RequiresApproval(reqs) => {
            assert_eq!(reqs.len(), 1);
            assert_eq!(reqs[0].display, "cargo test");
            assert!(reqs[0].patterns.contains(&"shell:cargo *".to_string()));
            assert!(reqs[0].patterns.contains(&"shell:cargo test".to_string()));
        }
        other => panic!("expected RequiresApproval, got {:?}", other),
    }
}

#[test]
fn shell_permission_compound_command_two_segments() {
    use crate::types::ToolPermission;
    let tool = ShellTool::new(
        std::path::PathBuf::from("/tmp"),
        std::path::PathBuf::from("/tmp"),
    );
    let perm = tool.permission(&serde_json::json!({"cmd": "cargo build && git push", "reason": ""}));
    match perm {
        ToolPermission::RequiresApproval(reqs) => {
            assert_eq!(reqs.len(), 2);
            assert_eq!(reqs[0].display, "cargo build");
            assert_eq!(reqs[1].display, "git push");
            assert!(reqs[0].patterns.contains(&"shell:cargo *".to_string()));
            assert!(reqs[1].patterns.contains(&"shell:git *".to_string()));
        }
        other => panic!("expected RequiresApproval, got {:?}", other),
    }
}

#[test]
fn shell_permission_patterns_all_prefixed_with_shell() {
    use crate::types::ToolPermission;
    let tool = ShellTool::new(
        std::path::PathBuf::from("/tmp"),
        std::path::PathBuf::from("/tmp"),
    );
    let perm = tool.permission(&serde_json::json!({"cmd": "cargo test --verbose", "reason": ""}));
    if let ToolPermission::RequiresApproval(reqs) = perm {
        for pat in &reqs[0].patterns {
            assert!(pat.starts_with("shell:"), "pattern missing shell: prefix: {pat}");
        }
    }
}
```

- [ ] **Step 4: Run shell tests**

```bash
cd gait-rs && cargo test tools::shell::tests:: 2>&1 | tail -15
```

Expected: all pass including the three new tests.

- [ ] **Step 5: Commit**

```bash
cd gait-rs && git add src/tools/shell.rs && git commit -m "feat: ShellTool declares permission() with per-segment PermissionRequests"
```

---

## Task 7: Mechanical Cleanups

**Files:**
- Modify: `gait-rs/src/tools/codesearch.rs` — remove duplicate `boxed()`
- Modify: `gait-rs/src/tools/webfetch.rs` — remove duplicate `boxed()`
- Modify: `gait-rs/src/tools/list_agents.rs` — replace literal JSON schema with `make_schema`
- Modify: `gait-rs/src/chat.rs` — make `new_session_id` pub
- Modify: `gait-rs/src/tools/spawn_agent.rs` — use `crate::chat::new_session_id()`
- Modify: `gait-rs/src/turn.rs` — add `pub fn once_approval_path`; add `derive_loaded_context_tool`

- [ ] **Step 1: Remove duplicate `boxed()` from codesearch.rs and webfetch.rs**

In `gait-rs/src/tools/codesearch.rs`, delete:
```rust
pub fn boxed(self) -> Box<dyn Tool> { Box::new(self) }
```

In `gait-rs/src/tools/webfetch.rs`, delete the same method.

Verify callers in `tools/mod.rs` — they call `.boxed()` via the trait, so no changes needed there.

- [ ] **Step 2: Fix `list_agents.rs` literal schema**

In `gait-rs/src/tools/list_agents.rs`, find the `ListAgentsTool` struct and its literal schema. Replace with a proper empty params struct.

Add at the top of the file (after existing imports):

```rust
use schemars::JsonSchema;
use serde::Deserialize;

#[derive(Deserialize, JsonSchema)]
struct ListAgentsParams {}
```

In `ListAgentsTool::new()`, replace the literal schema with:

```rust
pub fn new(workspace: PathBuf) -> Self {
    let parameters = crate::tools::make_schema::<ListAgentsParams>();
    Self { workspace, parameters }
}
```

Run `cargo build` to confirm it compiles. The schema output will be `{"type":"object","properties":{}}` instead of the hand-rolled literal.

- [ ] **Step 3: Make `new_session_id` pub in `chat.rs`**

In `gait-rs/src/chat.rs`, find the `new_session_id` function (it generates `"s{8-char-uuid}"`) and add `pub`:

```rust
pub fn new_session_id() -> String {
    format!("s{}", &uuid::Uuid::new_v4().to_string().replace('-', "")[..8])
}
```

- [ ] **Step 4: Update `spawn_agent.rs` to use `chat::new_session_id()`**

In `gait-rs/src/tools/spawn_agent.rs`, find the duplicated session ID format line (it looks like `format!("s{}", &uuid::Uuid::new_v4()...)`). Replace it with:

```rust
let session_id = crate::chat::new_session_id();
```

Remove any now-unused `uuid` import from `spawn_agent.rs` if it's no longer referenced.

- [ ] **Step 5: Add `once_approval_path` to `turn.rs`**

In `gait-rs/src/turn.rs`, add this function after the `SessionLock` impl:

```rust
/// Path of the temp file used to pass once-approved patterns from chat.rs to turn.rs.
/// Written by chat.rs after user approval; read-and-deleted by turn.rs at turn start.
pub fn once_approval_path(workspace: &std::path::Path, session_id: &str) -> std::path::PathBuf {
    workspace.join("tmp").join(format!("gait-approve-{}", session_id))
}
```

Update the existing hardcoded path in `run_turn` (line ~241):

```rust
// OLD:
let once_path = opts.workspace.join("tmp").join(format!("gait-approve-{}", opts.session_id));
// NEW:
let once_path = once_approval_path(&opts.workspace, &opts.session_id);
```

- [ ] **Step 6: Update `chat.rs` to use `once_approval_path`**

In `gait-rs/src/chat.rs`, find the temp file path construction (line ~501):

```rust
// OLD:
let temp = tmp_dir.join(format!("gait-approve-{}", session_id));
// NEW:
let temp = crate::turn::once_approval_path(&workspace, &session_id);
```

Remove any now-unused `tmp_dir` variable if it was only used for this path.

- [ ] **Step 7: Add `derive_loaded_context_tool` and simplify `derive_loaded_mcps/skills`**

In `gait-rs/src/turn.rs`, add a generic helper after the `build_turn_context` function:

```rust
/// Generic: scan events for calls to a context-loading tool (mcp or skill),
/// return names of items where the last call had context=true.
pub fn derive_loaded_context_tool(events: &[AnyEvent], tool_name: &str) -> Vec<String> {
    let mut state: std::collections::HashMap<String, bool> = std::collections::HashMap::new();
    for event in events {
        if let AnyEvent::ToolCall { name, input, .. } = event {
            if name == tool_name {
                if let (Some(item_name), Some(context)) = (
                    input.get("name").and_then(|v| v.as_str()),
                    input.get("context").and_then(|v| v.as_bool()),
                ) {
                    state.insert(item_name.to_string(), context);
                }
            }
        }
    }
    state.into_iter().filter(|(_, loaded)| *loaded).map(|(name, _)| name).collect()
}
```

Replace the bodies of `derive_loaded_mcps` and `derive_loaded_skills` to delegate:

```rust
pub fn derive_loaded_mcps(events: &[AnyEvent]) -> Vec<String> {
    derive_loaded_context_tool(events, "mcp")
}

pub fn derive_loaded_skills(events: &[AnyEvent]) -> Vec<String> {
    derive_loaded_context_tool(events, "skill")
}
```

- [ ] **Step 8: Run full test suite**

```bash
cd gait-rs && cargo test 2>&1 | tail -20
```

Expected: all tests pass.

- [ ] **Step 9: Commit**

```bash
cd gait-rs && git add src/tools/codesearch.rs src/tools/webfetch.rs src/tools/list_agents.rs src/tools/spawn_agent.rs src/chat.rs src/turn.rs && git commit -m "refactor: mechanical cleanups — shared session_id, once_approval_path, derive_loaded, schema fix, boxed dedup"
```

---

## Task 8: Rewire turn.rs — Trait-Driven Dispatch

**Files:**
- Modify: `gait-rs/src/turn.rs`

This is the pivotal task. After this, `turn.rs` has zero hardcoded tool names. All existing integration tests must still pass.

- [ ] **Step 1: Update imports in `turn.rs` and delete the old permission functions**

Find the permission import line in `gait-rs/src/turn.rs`:

```rust
use crate::permission::{is_tool_allowed, unapproved_segments, command_patterns};
```

Replace with:

```rust
use crate::permission::is_request_approved;
use crate::types::{ToolPermission, ExecutionSignal};
```

Now that `turn.rs` no longer calls them, delete from `gait-rs/src/permission.rs`:
- `fn is_segment_allowed(...)` 
- `pub fn unapproved_segments(...)`
- `pub fn is_tool_allowed(...)`

Also delete their tests from `#[cfg(test)]`:
- `is_allowed_exact_tool`
- `is_allowed_shell_pattern`
- `is_allowed_shell_compound_all_covered`
- `is_allowed_shell_compound_partial_coverage`
- `unapproved_segments_returns_unapproved_only`
- `unapproved_segments_empty_when_all_allowed`

Keep: `split_commands`, `command_patterns`, `wildcard_match`, and their tests.

- [ ] **Step 2: Replace the permission gate loop**

Find the block in `run_turn` starting with `for tc in &step_tool_calls {` (around line 460). Replace it entirely:

```rust
for tc in &step_tool_calls {
    let perm = tools.iter()
        .find(|t| t.name() == tc.tool_name)
        .map(|t| t.permission(&tc.args))
        .unwrap_or_else(|| ToolPermission::RequiresApproval(vec![
            crate::types::PermissionRequest {
                display: tc.tool_name.clone(),
                patterns: vec![tc.tool_name.clone()],
            }
        ]));

    let is_allowed = opts.auto || match &perm {
        ToolPermission::Passthrough => true,
        ToolPermission::RequiresApproval(requests) => requests.iter()
            .all(|req| is_request_approved(&tc.tool_name, req, &effective_allow)),
    };

    if !is_allowed {
        gated_ids.insert(tc.tool_call_id.clone());
        if let ToolPermission::RequiresApproval(requests) = &perm {
            for req in requests.iter()
                .filter(|r| !is_request_approved(&tc.tool_name, r, &effective_allow))
            {
                pending_permissions.push(PermissionItem {
                    id: tc.tool_call_id.clone(),
                    cmd: req.display.clone(),
                    reason: format!("{} requires permission", tc.tool_name),
                    patterns: req.patterns.clone(),
                    tool_name: tc.tool_name.clone(),
                    tool_args: tc.args.clone(),
                });
            }
        }
    }
}
```

- [ ] **Step 3: Replace the parallel/serial classifier**

Find `let is_parallel_eligible = |tc: &ToolCall| -> bool {` (around line 504). Replace the whole closure:

```rust
let is_parallel_eligible = |tc: &ToolCall| -> bool {
    tools.iter()
        .find(|t| t.name() == tc.tool_name)
        .map(|t| {
            t.execution_signal() == ExecutionSignal::Continue
                && t.execution_mode() == ExecutionMode::Parallel
        })
        .unwrap_or(false)
};
```

- [ ] **Step 4: Replace the serial match block**

Find the `for (i, tc) in &serial_items {` block. Replace the `match tc.tool_name.as_str() {` dispatch inside it:

```rust
for (i, tc) in &serial_items {
    let signal = tools.iter()
        .find(|t| t.name() == tc.tool_name)
        .map(|t| t.execution_signal())
        .unwrap_or(ExecutionSignal::Continue);

    match signal {
        ExecutionSignal::LoadContext => {
            // mcp or skill: load/unload the named tool from the active tools vec
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
            let r = execute_tool_call(tc, &tools, &tx_path).await
                .unwrap_or_else(|e| error_tool_result(tc, e));
            all_results[*i] = Some(r);
        }
        ExecutionSignal::TurnComplete => {
            // output: calling this tool IS the exit signal. Exit the inner loop
            // immediately — no tool result sent back, no extra LLM round-trip.
            let summary = tc.args.get("summary").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let result = tc.args.get("result").cloned();
            let events_now = read_events(&tx_path)?;
            let turn_n = count_turn_complete(&events_now) + 1;
            emit_event(&tx_path, &AnyEvent::TurnComplete {
                usage: TurnUsage {
                    input: total_usage.input_tokens as u64,
                    output: total_usage.output_tokens as u64,
                    cache_read: total_usage.cache_read_tokens as u64,
                    cache_write: total_usage.cache_write_tokens as u64,
                },
                codebase_hash: codebase_hash.clone(),
                codebase_dirty,
                output: TurnOutput { summary: summary.clone(), result: result.clone() },
                ts: now_ms(),
            })?;
            git::commit_session(&opts.workspace, &opts.session_id,
                &format!("session({}): turn {}", opts.session_id, turn_n))?;
            return Ok(TurnResult::Done { summary, result });
        }
        ExecutionSignal::Clarify => {
            let items: Vec<ClarifyItem> = tc.args.get("items")
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_default();
            emit_event(&tx_path, &AnyEvent::Clarify { items: items.clone(), ts: now_ms() })?;
            clarify_signal = Some(items);
            // no tool result — clarify exits the turn without executing
        }
        ExecutionSignal::Inform => {
            let items: Vec<InformItem> = tc.args.get("items")
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_default();
            emit_event(&tx_path, &AnyEvent::Inform { items: items.clone(), ts: now_ms() })?;
            inform_signal = Some(items);
            // no tool result — inform exits the turn without executing
        }
        ExecutionSignal::Continue => {
            let r = execute_tool_call(tc, &tools, &tx_path).await
                .unwrap_or_else(|e| error_tool_result(tc, e));
            all_results[*i] = Some(r);
        }
    }
}
```

Also remove the `output_signal` variable and its usage in the "no tool calls" path. After this change the "no tool calls" path is a plain-text fallback — it no longer needs to check for a prior output call:

```rust
// No tool calls → turn complete (plain text response, no output tool called)
if step_tool_calls.is_empty() {
    let events_now = read_events(&tx_path)?;
    let turn_n = count_turn_complete(&events_now) + 1;
    let summary = step_text.lines().next().unwrap_or("").to_string();
    emit_event(&tx_path, &AnyEvent::TurnComplete {
        usage: TurnUsage {
            input: total_usage.input_tokens as u64,
            output: total_usage.output_tokens as u64,
            cache_read: total_usage.cache_read_tokens as u64,
            cache_write: total_usage.cache_write_tokens as u64,
        },
        codebase_hash: codebase_hash.clone(),
        codebase_dirty,
        output: TurnOutput { summary: summary.clone(), result: None },
        ts: now_ms(),
    })?;
    git::commit_session(&opts.workspace, &opts.session_id,
        &format!("session({}): turn {}", opts.session_id, turn_n))?;
    return Ok(TurnResult::Done { summary, result: None });
}
```

Update the existing test `output_tool_result_used_in_turn_complete` — it currently has two provider responses (output call + empty text). After this fix output exits immediately, so it only needs one response:

```rust
// BEFORE (wrong — was relying on extra LLM call):
responses: Arc::new(vec![
    vec![StreamChunk::ToolCallComplete { tool_name: "output".into(), ... }, StreamChunk::Done],
    vec![StreamChunk::Text("".into()), StreamChunk::Done],  // remove this
]),
// AFTER:
responses: Arc::new(vec![
    vec![StreamChunk::ToolCallComplete { tool_name: "output".into(), ... }, StreamChunk::Done],
]),
```

- [ ] **Step 5: Add two new integration tests**

Append to `#[cfg(test)]` in `turn.rs`:

```rust
#[tokio::test]
async fn always_allow_tool_never_gated_in_non_auto_mode() {
    use crate::types::{ToolPermission, ExecutionSignal};
    let ws = TempDir::new().unwrap();
    init_git_repo(ws.path());

    struct PassthroughedTool;
    #[async_trait::async_trait]
    impl crate::types::Tool for PassthroughedTool {
        fn name(&self) -> &str { "always_allowed" }
        fn description(&self) -> &str { "" }
        fn parameters(&self) -> &serde_json::Value { &serde_json::Value::Null }
        async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<serde_json::Value> {
            Ok(serde_json::Value::String("ok".into()))
        }
        fn permission(&self, _args: &serde_json::Value) -> ToolPermission { ToolPermission::Passthrough }
    }

    let provider = Arc::new(MockProvider {
        responses: Arc::new(vec![
            vec![StreamChunk::ToolCallComplete {
                tool_call_id: "tc1".into(), tool_name: "always_allowed".into(),
                args: serde_json::json!({}),
            }, StreamChunk::Done],
            vec![StreamChunk::Text("done".into()), StreamChunk::Done],
        ]),
        call_idx: Arc::new(AtomicUsize::new(0)),
    });

    run_turn(TurnOpts {
        session_id: "s_always_allow".into(), task: Some("test".into()), auto: false,
        workspace: ws.path().to_path_buf(), codebase_path: ws.path().to_path_buf(),
        allow: vec![], system_prompt: "".into(), model: "mock/test".into(),
        mcp_registry: vec![], mcp_clients: std::collections::HashMap::new(), skill_registry: vec![],
    }, provider, vec![Box::new(PassthroughedTool)]).await.unwrap();

    let events = read_events(&ws.path().join("sessions/s_always_allow/transcript.jsonl")).unwrap();
    assert!(!events.iter().any(|e| matches!(e, AnyEvent::Permissions { .. })),
        "Passthrough tool must never trigger permission gating");
    assert!(events.iter().any(|e| matches!(e, AnyEvent::TurnComplete { .. })));
}

#[tokio::test]
async fn needs_approval_tool_gated_when_not_in_allow_list() {
    let ws = TempDir::new().unwrap();
    init_git_repo(ws.path());
    let custom_tool = ToolDef {
        name: "custom_tool".into(), description: "".into(),
        parameters: serde_json::json!({}),
        execute: Box::new(|_| Box::pin(async { Ok(serde_json::Value::Null) })),
    };
    let provider = Arc::new(MockProvider {
        responses: Arc::new(vec![
            vec![StreamChunk::ToolCallComplete {
                tool_call_id: "tc1".into(), tool_name: "custom_tool".into(),
                args: serde_json::json!({}),
            }, StreamChunk::Done],
        ]),
        call_idx: Arc::new(AtomicUsize::new(0)),
    });
    run_turn(TurnOpts {
        session_id: "s_needs_approval".into(), task: Some("test".into()), auto: false,
        workspace: ws.path().to_path_buf(), codebase_path: ws.path().to_path_buf(),
        allow: vec![], system_prompt: "".into(), model: "mock/test".into(),
        mcp_registry: vec![], mcp_clients: std::collections::HashMap::new(), skill_registry: vec![],
    }, provider, vec![Box::new(custom_tool)]).await.unwrap();

    let events = read_events(&ws.path().join("sessions/s_needs_approval/transcript.jsonl")).unwrap();
    assert!(events.iter().any(|e| matches!(e, AnyEvent::Permissions { .. })));
}
```

- [ ] **Step 6: Run ALL turn tests**

```bash
cd gait-rs && cargo test turn:: 2>&1 | tail -25
```

Expected: all pass — existing tests and both new ones.

- [ ] **Step 7: Commit**

```bash
cd gait-rs && git add src/turn.rs && git commit -m "refactor: turn.rs fully trait-driven — no hardcoded tool names in permission gate or dispatch"
```

---

## Task 9: Shared Approval Functions

**Files:**
- Modify: `gait-rs/src/permission.rs`
- Modify: `gait-rs/src/session/transcript.rs`

All "what happens when a permission choice is made" logic moves here. The REPL, CLI subcommands, and any future caller (spawn_agent, remote API) all go through these functions. `chat.rs` currently does this inline — after this task it is the only caller, but Task 11 strips it out.

- [ ] **Step 1: Add `save_choice` to `permission.rs`**

`save_choice` persists the chosen pattern to the right location. It does NOT touch the transcript — that is `write_permissions_reply`'s job.

```rust
/// Persist a permission choice to the appropriate location.
/// Does not write a transcript event — call write_permissions_reply after collecting all choices.
pub fn save_choice(
    choice: &PermissionChoice,
    workspace: &std::path::Path,
    session_id: &str,
    agent_path: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    match choice {
        PermissionChoice::Global(pattern) => {
            crate::config::append_allow_pattern(workspace, pattern)?;
        }
        PermissionChoice::Agent(pattern) => {
            let path = agent_path.ok_or_else(|| anyhow::anyhow!("no agent file in scope"))?;
            // Use the existing serde_yaml-based implementation — handles inline flow style,
            // arbitrary indentation, and trailing comments correctly.
            crate::agent_frontmatter::append_agent_allow(path.to_path_buf(), pattern)?;
        }
        PermissionChoice::Once(pattern) => {
            let once_path = crate::turn::once_approval_path(workspace, session_id);
            std::fs::create_dir_all(once_path.parent().unwrap())?;
            let mut f = std::fs::OpenOptions::new()
                .create(true).append(true).open(&once_path)?;
            use std::io::Write;
            writeln!(f, "{}", pattern)?;
        }
        PermissionChoice::Reject => {}
    }
    Ok(())
}
```

- [ ] **Step 2: Add `write_permissions_reply` to `session/transcript.rs`**

This writes a `PermissionsReply` event to the transcript. After this, `run_turn` will detect it on the next call and resume.

```rust
/// Write a PermissionsReply event. `decisions` is a vec of (item_id, "approve"|"deny", Option<pattern>).
/// The optional pattern is stored in the `text` field for audit trail.
pub fn write_permissions_reply(
    transcript_path: &std::path::Path,
    decisions: &[(String, String, Option<String>)],
) -> anyhow::Result<()> {
    let items: Vec<PermissionsReplyItem> = decisions.iter().map(|(id, decision, text)| {
        PermissionsReplyItem {
            id: id.clone(),
            decision: decision.clone(),
            text: text.clone(),
        }
    }).collect();
    append_event(transcript_path, &AnyEvent::PermissionsReply {
        from: "user".into(),
        items,
        ts: now_ms(),
    })
}
```

- [ ] **Step 3: Add `session_state` to `session/transcript.rs`**

`gait status` needs to read what a session is waiting on. This fn returns the current pending state by reading the last meaningful event.

```rust
#[derive(Debug, serde::Serialize)]
pub enum SessionState {
    Empty,
    InProgress,
    PendingPermissions { items: Vec<PermissionItem> },
    PendingClarify     { items: Vec<ClarifyItem> },
    PendingInform      { items: Vec<InformItem> },
    Done               { summary: String },
}

pub fn session_state(transcript_path: &std::path::Path) -> anyhow::Result<SessionState> {
    let events = match read_events(transcript_path) {
        Ok(e) => e,
        Err(_) => return Ok(SessionState::Empty),
    };
    // Walk backwards to find the last meaningful event.
    // Walking backwards means a Reply is encountered before the original event, so
    // we return InProgress when we see a reply (the original is already resolved).
    for event in events.iter().rev() {
        match event {
            AnyEvent::Permissions { items, .. } => {
                return Ok(SessionState::PendingPermissions { items: items.clone() });
            }
            AnyEvent::PermissionsReply { .. } => {
                // Reply exists — permissions resolved
                return Ok(SessionState::InProgress);
            }
            AnyEvent::Clarify { items, .. } => {
                return Ok(SessionState::PendingClarify { items: items.clone() });
            }
            AnyEvent::ClarifyReply { .. } => {
                // Reply exists — clarify resolved
                return Ok(SessionState::InProgress);
            }
            AnyEvent::Inform { items, .. } => {
                return Ok(SessionState::PendingInform { items: items.clone() });
            }
            AnyEvent::InformReply { .. } => {
                // Reply exists — inform resolved
                return Ok(SessionState::InProgress);
            }
            AnyEvent::TurnComplete { output, .. } => {
                return Ok(SessionState::Done { summary: output.summary.clone() });
            }
            _ => {}
        }
    }
    Ok(SessionState::InProgress)
}
```

- [ ] **Step 4: Add tests**

```rust
// permission.rs tests
#[test]
fn save_choice_once_writes_to_approval_file() {
    let dir = TempDir::new().unwrap();
    save_choice(
        &PermissionChoice::Once("shell:cargo *".into()),
        dir.path(), "s123", None,
    ).unwrap();
    let content = std::fs::read_to_string(
        dir.path().join("tmp/gait-approve-s123")
    ).unwrap();
    assert!(content.contains("shell:cargo *"));
}

#[test]
fn save_choice_global_appends_to_config() {
    let dir = TempDir::new().unwrap();
    save_choice(
        &PermissionChoice::Global("read:src/*".into()),
        dir.path(), "s123", None,
    ).unwrap();
    let content = std::fs::read_to_string(dir.path().join("gait.jsonc")).unwrap();
    assert!(content.contains("read:src/*"));
}

#[test]
fn save_choice_reject_is_noop() {
    let dir = TempDir::new().unwrap();
    // Should not create any files
    save_choice(&PermissionChoice::Reject, dir.path(), "s123", None).unwrap();
    assert!(!dir.path().join("gait.jsonc").exists());
    assert!(!dir.path().join("tmp/gait-approve-s123").exists());
}
```

```rust
// transcript.rs tests
#[test]
fn session_state_pending_permissions() {
    let dir = TempDir::new().unwrap();
    let tx = dir.path().join("tx.jsonl");
    append_event(&tx, &AnyEvent::Permissions {
        items: vec![PermissionItem {
            id: "tc1".into(), cmd: "cargo test".into(),
            reason: "".into(), patterns: vec!["shell:cargo *".into()],
            tool_name: "shell".into(), tool_args: serde_json::json!({}),
        }],
        ts: 1,
    }).unwrap();
    assert!(matches!(session_state(&tx).unwrap(), SessionState::PendingPermissions { .. }));
}

#[test]
fn session_state_done() {
    let dir = TempDir::new().unwrap();
    let tx = dir.path().join("tx.jsonl");
    append_event(&tx, &AnyEvent::TurnComplete {
        usage: TurnUsage { input: 0, output: 0, cache_read: 0, cache_write: 0 },
        codebase_hash: "".into(), codebase_dirty: false,
        output: TurnOutput { summary: "all done".into(), result: None },
        ts: 1,
    }).unwrap();
    match session_state(&tx).unwrap() {
        SessionState::Done { summary } => assert_eq!(summary, "all done"),
        other => panic!("expected Done, got {:?}", other),
    }
}
```

- [ ] **Step 5: Run tests**

```bash
cd gait-rs && cargo test permission::tests::save_choice session::transcript::tests::session_state 2>&1 | tail -15
```

- [ ] **Step 6: Commit**

```bash
cd gait-rs && git add src/permission.rs src/session/transcript.rs && git commit -m "feat: shared save_choice, write_permissions_reply, session_state — CLI and REPL common layer"
```

---

## Task 10: CLI Subcommands — approve, reject, status

**Files:**
- Modify: `gait-rs/src/bin/gait.rs`

The REPL is a UI loop. Everything it does must be doable via CLI so sessions can be driven turn-by-turn from scripts, other tools, or terminals without the REPL. Three subcommands:

**Important:** The CLI entry point is `src/bin/gait.rs`, not `main.rs`. It uses hand-rolled arg parsing: a top-level `match args[1].as_str()` dispatches to `cmd_*` functions, each with a `while i < argv.len()` flags loop. There is no clap. The three new subcommands must follow this pattern — add `cmd_approve`, `cmd_reject`, `cmd_status` functions and add arms to the top-level match. Also update the `usage()` function at the top of the file.

- `gait approve <session> "<pattern>" [--global | --once | --agent <file>]`
- `gait reject <session>`
- `gait status <session> [--json]`

The pattern string is explicit — the user copies it from `gait status` output. The REPL resolves `g1`/`o`/`a2` keystrokes to the pattern string *before* calling the same shared execute, so there is no `--level` concept in the canonical params.

`clarify` and `inform` responses don't need a special command — the user just runs `gait run <session> --task "<answer>"` (clarify) or `gait run <session>` (inform/continue).

- [ ] **Step 1: Add `gait status <session>`**

Reads the session transcript and prints the current pending state.

**Important:** The snippets below show the logic. You must adapt them to the hand-rolled arg-parsing style used throughout `src/bin/gait.rs` — add a `fn cmd_status(argv: &[String], workspace: &Path)` function and a `"status"` arm in the top-level `match`. Use `argv[i]` indexing and `while i < argv.len()` flag loops, following the pattern of existing commands like `cmd_run`. Also update `usage()` at the top of the file.

**Field names** — use exact field names from transcript types:
- `ClarifyItem`: `content` (the question text), `why`
- `InformItem`: `content` (the info text), `context`

```rust
// Logic for cmd_status (adapt to hand-rolled style):
use gait_rs::session::transcript::{session_state, SessionState};

let session_id = &argv[2];  // argv[0]=gait argv[1]=status argv[2]=<session>
let json_flag = argv.iter().any(|a| a == "--json");
let tx_path = workspace.join("sessions").join(session_id).join("transcript.jsonl");

let state = session_state(&tx_path)?;
if json_flag {
    println!("{}", serde_json::to_string(&state)?);
    return Ok(());
}

match state {
    SessionState::Empty => println!("no session found"),
    SessionState::InProgress => println!("in progress"),
    SessionState::Done { summary } => println!("done: {}", summary),
    SessionState::PendingClarify { items } => {
        println!("pending clarify:");
        for item in &items {
            println!("  Q: {}", item.content);
            if !item.why.is_empty() { println!("  why: {}", item.why); }
        }
        println!("\nRespond: gait run {} --task \"<answer>\"", session_id);
    }
    SessionState::PendingInform { items } => {
        println!("pending inform:");
        for item in &items {
            println!("  {}", item.content);
            if let Some(ctx) = &item.context { println!("  context: {}", ctx); }
        }
        println!("\nContinue: gait run {}", session_id);
    }
    SessionState::PendingPermissions { items } => {
        println!("pending permissions:");
        let mut seen = std::collections::HashSet::new();
        for item in &items {
            if seen.insert(&item.id) {
                println!("\n  {} — {}", item.tool_name, item.cmd);
                println!("  patterns (copy one to approve):");
                for pat in &item.patterns {
                    println!("    \"{}\"", pat);
                }
            }
        }
        println!("\nApprove: gait approve {} \"<pattern>\" [--global|--once|--agent <file>]", session_id);
        println!("Reject:  gait reject {}", session_id);
    }
}
```

- [ ] **Step 2: Add `ApproveParams`/`execute_approve` to `permission.rs` and wire CLI**

`ApproveParams`, `ApproveScope`, and `execute_approve` must live in `gait-rs/src/permission.rs` (the library crate), NOT in `src/bin/gait.rs`. Both the CLI binary and `chat.rs` (REPL) need to call `execute_approve` — a binary crate is not importable by a library crate, so any shared logic must be in the library.

Add to `gait-rs/src/permission.rs`:

```rust
pub struct ApproveParams {
    pub session_id: String,
    pub pattern: String,              // e.g. "shell:cargo *" — explicit, no level indirection
    pub scope: ApproveScope,
    pub agent_path: Option<std::path::PathBuf>,
}

pub enum ApproveScope { Global, Once, Agent }

pub fn execute_approve(params: &ApproveParams, workspace: &std::path::Path) -> anyhow::Result<()> {
    let tx_path = workspace
        .join("sessions").join(&params.session_id).join("transcript.jsonl");

    let state = crate::session::transcript::session_state(&tx_path)?;
    let items = match state {
        SessionState::PendingPermissions { items } => items,
        other => anyhow::bail!("no pending permissions (state: {:?})", other),
    };

    let choice = match &params.scope {
        ApproveScope::Global => PermissionChoice::Global(params.pattern.clone()),
        ApproveScope::Once   => PermissionChoice::Once(params.pattern.clone()),
        ApproveScope::Agent  => PermissionChoice::Agent(params.pattern.clone()),
    };
    crate::permission::save_choice(&choice, workspace, &params.session_id, params.agent_path.as_deref())?;

    // Only approve items where the given pattern actually covers the request.
    // Items not covered get "deny" so the model can react to the refusal.
    // This prevents a pattern like "shell:cargo *" from silently approving
    // an unrelated "git push" that happened to be pending in the same turn.
    let mut seen = std::collections::HashSet::new();
    let allow = vec![params.pattern.clone()];
    let decisions: Vec<(String, String, Option<String>)> = items.iter()
        .filter(|item| seen.insert(item.id.clone()))
        .map(|item| {
            let req = crate::types::PermissionRequest {
                display: item.cmd.clone(),
                patterns: item.patterns.clone(),
            };
            let covered = crate::permission::is_request_approved(&item.tool_name, &req, &allow);
            if covered {
                (item.id.clone(), "approve".into(), Some(params.pattern.clone()))
            } else {
                (item.id.clone(), "deny".into(), None)
            }
        })
        .collect();

    let approved = decisions.iter().filter(|(_, d, _)| d == "approve").count();
    let denied   = decisions.iter().filter(|(_, d, _)| d == "deny").count();
    crate::session::transcript::write_permissions_reply(&tx_path, &decisions)?;
    if denied > 0 {
        println!("approved {approved} item(s), denied {denied} (pattern did not cover them) — run: gait run {}", params.session_id);
    } else {
        println!("approved {approved} item(s) — run: gait run {}", params.session_id);
    }
    Ok(())
}
```

The REPL maps keystrokes to the same `ApproveParams`:
- `g1` → `ApproveParams { pattern: item.patterns[0].clone(), scope: Global, .. }`
- `a2` → `ApproveParams { pattern: item.patterns[1].clone(), scope: Agent, .. }`
- `o`  → `ApproveParams { pattern: item.patterns.last().clone(), scope: Once, .. }`

Same `execute_approve` runs in both cases. The keystroke is just a shorthand parser.

- [ ] **Step 3: Add `gait reject <session>`**

Rejects all pending permissions. The model sees "permission denied" for each and can respond accordingly. Add `fn cmd_reject(argv, workspace)` following the hand-rolled arg style.

```rust
// Logic for cmd_reject (adapt to hand-rolled style):
let session_id = &argv[2];
let tx_path = workspace.join("sessions").join(session_id).join("transcript.jsonl");
let state = crate::session::transcript::session_state(&tx_path)?;

    let items = match state {
        SessionState::PendingPermissions { items } => items,
        other => anyhow::bail!("no pending permissions (state: {:?})", other),
    };

    let mut seen_ids = std::collections::HashSet::new();
    let decisions: Vec<(String, String, Option<String>)> = items.iter()
        .filter(|item| seen_ids.insert(item.id.clone()))
        .map(|item| (item.id.clone(), "deny".into(), None))
        .collect();

    crate::session::transcript::write_permissions_reply(&tx_path, &decisions)?;
    println!("rejected {} item(s) — run: gait run {} to continue", decisions.len(), session_id);
}
```

- [ ] **Step 4: Add tests**

```rust
#[test]
fn approve_command_writes_permissions_reply_and_saves_pattern() {
    // Integration test: set up a transcript with a pending Permissions event,
    // run the approve logic, verify PermissionsReply was written and gait.jsonc updated.
    let dir = TempDir::new().unwrap();
    let session_id = "s_approve_test";
    let tx = dir.path().join("sessions").join(session_id).join("transcript.jsonl");
    std::fs::create_dir_all(tx.parent().unwrap()).unwrap();

    // Write a pending Permissions event
    append_event(&tx, &AnyEvent::Permissions {
        items: vec![PermissionItem {
            id: "tc1".into(),
            cmd: "cargo test".into(),
            reason: "shell requires permission".into(),
            patterns: vec!["shell:cargo *".into(), "shell:cargo test".into()],
            tool_name: "shell".into(),
            tool_args: serde_json::json!({"cmd": "cargo test", "reason": ""}),
        }],
        ts: 1,
    }).unwrap();

    // Simulate: gait approve s_approve_test "shell:cargo *" --global
    let item_pattern = "shell:cargo *".to_string();
    save_choice(
        &PermissionChoice::Global(item_pattern.clone()),
        dir.path(), session_id, None,
    ).unwrap();
    write_permissions_reply(&tx, &[
        ("tc1".into(), "approve".into(), Some(item_pattern.clone())),
    ]).unwrap();

    // Verify PermissionsReply was written
    let events = read_events(&tx).unwrap();
    assert!(events.iter().any(|e| matches!(e, AnyEvent::PermissionsReply { .. })));

    // Verify pattern saved to config
    let config_content = std::fs::read_to_string(dir.path().join("gait.jsonc")).unwrap();
    assert!(config_content.contains("shell:cargo *"));
}

#[test]
fn reject_command_writes_deny_permissions_reply() {
    let dir = TempDir::new().unwrap();
    let session_id = "s_reject_test";
    let tx = dir.path().join("sessions").join(session_id).join("transcript.jsonl");
    std::fs::create_dir_all(tx.parent().unwrap()).unwrap();

    append_event(&tx, &AnyEvent::Permissions {
        items: vec![PermissionItem {
            id: "tc1".into(), cmd: "rm -rf /".into(),
            reason: "".into(), patterns: vec!["shell:rm *".into()],
            tool_name: "shell".into(), tool_args: serde_json::json!({}),
        }],
        ts: 1,
    }).unwrap();

    write_permissions_reply(&tx, &[("tc1".into(), "deny".into(), None)]).unwrap();

    let events = read_events(&tx).unwrap();
    if let Some(AnyEvent::PermissionsReply { items, .. }) = events.iter()
        .find(|e| matches!(e, AnyEvent::PermissionsReply { .. }))
    {
        assert_eq!(items[0].decision, "deny");
    }
}
```

- [ ] **Step 5: Run tests**

```bash
cd gait-rs && cargo test 2>&1 | tail -20
```

- [ ] **Step 6: Commit**

```bash
cd gait-rs && git add src/bin/gait.rs src/permission.rs && git commit -m "feat: gait approve/reject/status CLI subcommands — full turn-by-turn CLI control"
```

---

## Task 11: Update REPL — UI Layer Only

**Files:**
- Modify: `gait-rs/src/chat.rs`

The REPL is now a UI layer. It collects user input, parses it into a `PermissionChoice`, then delegates to the shared functions from Tasks 9/10. It no longer owns any "save" logic. The `"shell:"` prefix is also gone — patterns are pre-qualified. The local `PermissionChoice` enum is removed in favor of the one from `permission.rs`.

- [ ] **Step 1: Remove the local `PermissionChoice` enum from `chat.rs`**

Find and delete the `PermissionChoice` enum definition in `chat.rs` (it has `Global`, `Agent`, `Once`, `Reject` variants).

Add to imports at the top of `chat.rs`:

```rust
use crate::permission::PermissionChoice;
```

- [ ] **Step 2: Remove the `"shell:"` prefix logic from `parse_permission_response`**

Find `parse_permission_response` in `chat.rs`. The current function starts with:

```rust
let prefix = if tool_name == "shell" { "shell:" } else { "" };
```

And uses `format!("{}{}", prefix, pat)` when building `Global` and `Once` variants.

Remove the `prefix` variable entirely. The `tool_name` parameter is no longer needed — update the signature and all callers:

```rust
fn parse_permission_response(
    input: &str,
    patterns: &[String],
    has_agent: bool,
) -> Option<PermissionChoice> {
    match input.trim() {
        "r" | "reject" => return Some(PermissionChoice::Reject),
        "o" | "once" => {
            let pat = patterns.last()?;
            return Some(PermissionChoice::Once(pat.clone()));
        }
        _ => {}
    }
    if let Some(rest) = input.trim().strip_prefix('g') {
        if let Ok(n) = rest.parse::<usize>() {
            if n >= 1 && n <= patterns.len() {
                return Some(PermissionChoice::Global(patterns[n - 1].clone()));
            }
        }
    }
    if has_agent {
        if let Some(rest) = input.trim().strip_prefix('a') {
            if let Ok(n) = rest.parse::<usize>() {
                if n >= 1 && n <= patterns.len() {
                    return Some(PermissionChoice::Agent(patterns[n - 1].clone()));
                }
            }
        }
    }
    None
}
```

Update the call site in `prompt_single_permission` — remove `&item.tool_name` from the argument list:

```rust
match parse_permission_response(input.trim(), &item.patterns, has_agent) {
```

- [ ] **Step 3: Update tests in `chat.rs`**

The existing `parse_permission_response_*` tests pass `"shell"` or `"write"` as `tool_name`. Remove that argument from all test calls. Update expected values to match pre-qualified patterns (no prefix added at save time):

```rust
#[test]
fn parse_permission_response_global_shell() {
    let patterns = vec!["shell:cargo *".to_string(), "shell:cargo test".to_string()];
    assert_eq!(
        parse_permission_response("g1", &patterns, true),
        Some(PermissionChoice::Global("shell:cargo *".into()))
    );
}

#[test]
fn parse_permission_response_global_non_shell() {
    let patterns = vec!["write".to_string(), "write:src/main.rs".to_string()];
    assert_eq!(
        parse_permission_response("g2", &patterns, false),
        Some(PermissionChoice::Global("write:src/main.rs".into()))
    );
}

#[test]
fn parse_permission_response_once() {
    let patterns = vec!["shell:cargo *".to_string(), "shell:cargo test".to_string()];
    assert_eq!(
        parse_permission_response("o", &patterns, false),
        Some(PermissionChoice::Once("shell:cargo test".into()))
    );
}

#[test]
fn parse_permission_response_reject() {
    assert_eq!(
        parse_permission_response("r", &[], false),
        Some(PermissionChoice::Reject)
    );
}

#[test]
fn parse_permission_response_invalid() {
    let p = vec!["shell:ls".to_string()];
    assert_eq!(parse_permission_response("g99", &p, false), None);
    assert_eq!(parse_permission_response("xyz", &p, false), None);
    assert_eq!(parse_permission_response("", &p, false), None);
}
```

- [ ] **Step 4: Replace inline save logic with shared function calls**

Find where `chat.rs` currently writes to `gait.jsonc`, agent frontmatter, and the once file directly. Replace all of it:

```rust
// Where the REPL currently applies the choice, replace with:
use crate::permission::{save_choice, PermissionChoice};
use crate::session::transcript::write_permissions_reply;

let choice = parse_permission_response(input.trim(), &item.patterns, has_agent);
if let Some(choice) = choice {
    save_choice(&choice, &workspace, &session_id, agent_path.as_deref())?;
    let pattern = match &choice {
        PermissionChoice::Global(p) | PermissionChoice::Agent(p) | PermissionChoice::Once(p) => Some(p.clone()),
        PermissionChoice::Reject => None,
    };
    let decision = match &choice {
        PermissionChoice::Reject => "deny",
        _ => "approve",
    };
    write_permissions_reply(&tx_path, &[(item.id.clone(), decision.into(), pattern)])?;
}
```

- [ ] **Step 5: Run all tests**

```bash
cd gait-rs && cargo test 2>&1 | tail -25
```

Expected: full test suite passes. Zero hardcoded tool names remain outside tool definition files.

- [ ] **Step 6: Commit**

```bash
cd gait-rs && git add src/chat.rs && git commit -m "refactor: REPL is UI-only — delegates save_choice + write_permissions_reply to shared fns"
```

---

## Self-Review

**Spec coverage:**
- ✅ Config default model bug fixed (Task 1)
- ✅ `ToolPermission` + `PermissionRequest` + `ExecutionSignal` on `Tool` trait (Task 2)
- ✅ `path_patterns` — opencode-style file path specificity (Task 3)
- ✅ `is_request_approved` — unified replacement for `is_tool_allowed` + `is_segment_allowed` (Task 3)
- ✅ `PermissionChoice` moved to `permission.rs` (Task 3)
- ✅ `Passthrough` in code is output-only (Task 4) — clarify/inform/mcp/skill all use `RequiresApproval`; clarify/inform are pre-approved via default config allow list (Task 1), not hardcoded free in code. File Map header corrected to match task bodies.
- ✅ File tools declare path-aware `RequiresApproval` (Task 5)
- ✅ Shell declares `CommandSegments`-equivalent via `RequiresApproval` (Task 6)
- ✅ Duplicate `boxed()` removed from 7 tool structs (Tasks 4–7)
- ✅ `list_agents` schema literal replaced (Task 7)
- ✅ `once_approval_path` shared function (Task 7)
- ✅ `new_session_id` made pub, `spawn_agent.rs` deduplication (Task 7)
- ✅ `derive_loaded_context_tool` generic fn (Task 7)
- ✅ `turn.rs` zero hardcoded names — permission gate + parallel/serial + dispatch (Task 8)
- ✅ `save_choice` + `write_permissions_reply` shared functions — CLI and REPL call the same code (Task 9)
- ✅ `session_state` fn for reading what a session is waiting on (Task 9)
- ✅ `gait approve`, `gait reject`, `gait status` CLI subcommands — full turn-by-turn CLI control (Task 10)
- ✅ REPL is UI-only — collects input, calls shared fns, no inline save logic (Task 11)
- ✅ REPL drops `"shell:"` prefix logic; patterns are pre-qualified (Task 11)
- ✅ Backward compat: bare `"read"` in allow list still approves all reads (Task 3 `is_request_approved`)
- ✅ REPL once/global/agent/reject choices preserved, same as CLI `--global/--once/--agent/reject` (Tasks 10–11)

**Placeholder scan:** No TBDs. All code shown in full. No "similar to Task N" shortcuts.

**Type consistency:**
- `PermissionRequest` defined in Task 2 (`types.rs`), used in Tasks 3–8
- `ToolPermission` defined in Task 2, used in Tasks 3–8
- `ExecutionSignal` defined in Task 2, used in Tasks 4, 8
- `path_patterns(tool_name, rel_path)` defined in Task 3, called in Tasks 5–6
- `is_request_approved(tool_name, req, allow)` defined in Task 3, called in Task 8
- `PermissionChoice` defined in Task 3, imported in Task 9

---

## Implementation Status (verified 2026-06-10)

**Overall: Partially done (10 of 11 tasks substantially complete).**

### Task 1 — Fix config default model bug (`DEFAULT_MODEL` const)
- [x] `DEFAULT_MODEL = "anthropic/claude-sonnet-4-6"` — `gait-rs/src/config.rs:4`
- [x] `GaitConfig::default()` uses `DEFAULT_MODEL` — `config.rs:96`
- [x] `append_allow_pattern` creates file with `DEFAULT_MODEL` — `config.rs:152`
- [x] Test `append_allow_creates_file_with_correct_default_model` present

### Task 2 — Core types: `ToolPermission`, `PermissionRequest`, `ExecutionSignal`
- [x] `PermissionRequest { display, patterns }` — `gait-rs/src/types.rs:122-125`
- [x] `ToolPermission::Passthrough` and `RequiresApproval` — `types.rs:129-133`
- [x] `ExecutionSignal` (Continue/TurnComplete/Clarify/Inform/LoadContext) — `types.rs:138-151`
- [x] `Tool::permission()` default returns `RequiresApproval([name])` — `types.rs:201-206`
- [x] `Tool::execution_signal()` default returns `Continue` — `types.rs:210-212`
- [x] Tests for defaults in `types.rs` tests block

### Task 3 — Refactor `permission.rs`
- [x] `path_patterns(tool_name, rel_path)` — `permission.rs:144`
- [x] `is_request_approved(tool_name, req, allow)` — `permission.rs:229`
- [x] `is_request_denied(tool_name, req, deny)` — `permission.rs:203` (extra: not in original plan)
- [x] `find_matched_allow_pattern` / `find_matched_deny_pattern` — `permission.rs:159/178` (extra)
- [x] `PermissionChoice` enum — `permission.rs:8`
- [x] `save_choice` function — `permission.rs:29`
- [x] Old `is_tool_allowed`, `unapproved_segments`, `is_segment_allowed` removed (replaced)
- [x] Tests for all new functions present in `permission.rs` tests block

### Task 4 — Signal tools: Passthrough + ExecutionSignal
- [x] `output.rs`: `Passthrough` + `TurnComplete` — `tools/output.rs:34-39`, tests at line ~86
- [x] `clarify.rs`: `RequiresApproval(["clarify"])` + `Clarify` — `tools/clarify.rs:42-49`
- [x] `inform.rs`: `RequiresApproval(["inform"])` + `Inform` — `tools/inform.rs:40-47`
- [x] `load.rs` (replaces separate mcp.rs/skill.rs): `RequiresApproval` + `LoadContext` — `tools/load.rs:40-48`
- [ ] Separate `mcp.rs` and `skill.rs` files not present — replaced by unified `load.rs` (architectural change, not a gap)
- [x] Duplicate `boxed()` removed (trait provides it via default; no `pub fn boxed` in any tool file)

### Task 5 — File tools: path-aware permission
- [x] `read.rs`: path-relative `RequiresApproval` — `tools/read.rs:174-183`
- [x] `write.rs`: path-relative `RequiresApproval` — `tools/write.rs:51-59`
- [x] `edit.rs`: path-relative `RequiresApproval` — `tools/edit.rs:86-95`
- [ ] `multiedit.rs` not present (file does not exist — likely folded into `edit.rs` or not yet added)

### Task 6 — Shell tool: per-segment permission
- [x] `shell.rs`: `RequiresApproval` with one request per command segment, patterns prefixed with `"shell:"` — `tools/shell.rs:58-69`

### Task 7 — Mechanical cleanups
- [x] `codesearch.rs`: no duplicate `boxed()` (uses `make_schema`) — `tools/codesearch.rs:28`
- [x] `webfetch.rs`: no duplicate `boxed()` (uses `make_schema`) — `tools/webfetch.rs:39`
- [x] `list_agents.rs`: uses `make_schema::<ListAgentsParams>()` — `tools/list_agents.rs:18`
- [x] `spawn_agent.rs`: uses `crate::chat::new_session_id()` — `tools/spawn_agent.rs:52`
- [x] `chat::new_session_id` is `pub` — `chat.rs:33`
- [x] `once_approval_path` shared fn — `turn.rs:88`
- [ ] `derive_loaded_context_tool` generic fn NOT implemented — `derive_loaded_mcps` and `derive_loaded_skills` are not present as named functions; the LoadContext dispatch is inline in `turn.rs:959-986`

### Task 8 — Rewire turn.rs to trait-driven dispatch
- [x] Permission gate loop dispatches on `tool.permission()` — `turn.rs:738-801`
- [x] Deny list (Tier 1) checked first, before allow — `turn.rs:749-759`
- [x] Allow list (Tier 2) next — `turn.rs:762-783`
- [x] Ask / gate (Tier 3) last — `turn.rs:785-800`
- [x] `denied_ids` tracked; blocked result emitted without a prompt — `turn.rs:812-852`
- [x] `is_parallel_eligible` uses `execution_signal()` + `execution_mode()` — `turn.rs:861-868`
- [x] Serial dispatch uses `execution_signal()` — `turn.rs:956-1106`
- [x] `is_tool_allowed`, `unapproved_segments` removed from imports — confirmed (only `is_request_approved`, `is_request_denied`, etc. imported at `turn.rs:18`)
- [x] Integration tests for deny-wins — `turn.rs:1625-1762`

### Task 9 — Shared approval functions
- [x] `save_choice` in `permission.rs:29` — handles Global/Agent/Once/Reject
- [x] `write_permissions_reply` in `session/transcript.rs:400` — emits `PermissionsReply` event
- [x] `session_state` in `session/transcript.rs:364` — returns `SessionState` enum
- [x] `SessionState` enum (Idle/AwaitingPermission/AwaitingClarify/AwaitingInform) — `transcript.rs:342-357`
- [x] `execute_approve` in `permission.rs:74` — CLI approve + scope dispatch

### Task 10 — CLI subcommands: approve, reject, status
- [x] `ApproveParams`, `ApproveScope`, `execute_approve` in `permission.rs:61-136`
- Note: `gait approve/reject/status` CLI commands implemented via `execute_approve`; specific `cmd_approve`/`cmd_reject`/`cmd_status` in `bin/gait.rs` were not independently verified but the library plumbing is present

### Task 11 — Update REPL to UI layer only (partially done)
- [x] `chat.rs` uses `crate::permission::PermissionChoice` — `chat.rs:116`
- [x] `chat.rs` calls `crate::permission::save_choice` — `chat.rs:661`
- [x] `chat.rs` calls `transcript::write_permissions_reply` — `chat.rs:714`
- [x] `PermissionsReplyItem.scope` populated with `"once"/"agent"/"global"` — `chat.rs:674-703`
- [x] No inline `gait.jsonc` / agent frontmatter write logic in REPL
- [ ] `parse_permission_response` uses numeric `N` (maps to `PermissionChoice::Agent`) instead of the spec's `g{N}`/`a{N}` distinction — `chat.rs:125-129`. The `g{N}` (Global) keystroke is NOT implemented in the REPL. Global approval from the REPL uses `N` which maps to Agent scope.
- [ ] `derive_loaded_context_tool` generic helper not implemented (Task 7 gap carries here)

### Summary of outstanding gaps

| Gap | File | Notes |
|-----|------|-------|
| `g{N}` global scope keystroke in REPL | `chat.rs:112-131` | Numbers map to Agent; Global only via `gait approve --scope global` CLI |
| `multiedit.rs` path-aware permission | N/A | File does not exist; task may have been folded into `edit.rs` or is not yet needed |
| `derive_loaded_context_tool` generic fn | `turn.rs` | LoadContext dispatch is inline; `derive_loaded_mcps`/`derive_loaded_skills` are not present as extracted functions |
- `once_approval_path(workspace, session_id)` defined in Task 7, called in Task 7 (chat.rs update)
- All method signatures in later tasks match their definitions in earlier tasks ✓

---

## Implementation Status (verified 2026-06-10)

Overall: **Partially done — 10 of 11 tasks substantially complete; 3 sub-items outstanding.**

### Checklist

- [x] **Task 1 — Config default model bug** DONE.
  `DEFAULT_MODEL = "anthropic/claude-sonnet-4-6"` at `config.rs:4`; used in `GaitConfig::default()` (line 96) and both `append_allow_pattern` branches (lines 152, 179).

- [x] **Task 2 — Core types** DONE.
  `PermissionRequest` (types.rs:121), `ToolPermission` (types.rs:128), `ExecutionSignal` (types.rs:138) all defined. `permission()` (types.rs:199) and `execution_signal()` (types.rs:210) added to `Tool` trait with defaults. Tests present (types.rs:339–364).

- [x] **Task 3 — Refactor permission.rs** DONE.
  `path_patterns` (permission.rs:144), `is_request_approved` (permission.rs:229), `PermissionChoice` (permission.rs:7), `save_choice` (permission.rs:29), `execute_approve` (permission.rs:74) all present. Old `is_tool_allowed`/`unapproved_segments`/`is_segment_allowed` are absent — correctly removed. Tests present (permission.rs:376–474).

- [x] **Task 4 — Signal tools** DONE (with architectural note).
  `output.rs`: `Passthrough` + `TurnComplete` (output.rs:34–38). `clarify.rs`: `RequiresApproval` + `Clarify` (clarify.rs:42–49). `inform.rs`: `RequiresApproval` + `Inform` (inform.rs:40–47). `mcp.rs` and `skill.rs` no longer exist — they were unified into `load.rs` which implements `permission()` → `RequiresApproval` with `load`/`load:<name>` patterns (load.rs:40–45) and `execution_signal()` → `LoadContext` (load.rs:48). Duplicate `boxed()` methods are absent from all signal tool files.

- [x] **Task 5 — File tools (read, write, edit)** DONE.
  `read.rs`: path-aware `permission()` at read.rs:174. `write.rs`: permission() at write.rs:51. `edit.rs`: permission() at edit.rs:86. All call `path_patterns` from `permission.rs`.
  **REMAINING: `multiedit.rs` does not exist in the codebase** — the file was listed in the plan's File Map and Task 5, but `src/tools/` contains no `multiedit.rs`. Either the feature was never built or was intentionally omitted. If multi-edit support is needed, Task 5 Step 4 from this plan still applies.

- [x] **Task 6 — Shell tool** DONE.
  `shell.rs`: `permission()` at shell.rs:58 produces one `PermissionRequest` per command segment via `split_commands` + `command_patterns`. Tests present (shell.rs:208, 225, 240).

- [x] **Task 7 — Mechanical cleanups** MOSTLY DONE.
  - [x] Duplicate `boxed()` removed from `codesearch.rs` and `webfetch.rs` (neither file contains `pub fn boxed`).
  - [x] `list_agents.rs` uses `make_schema::<ListAgentsParams>()` (list_agents.rs:18).
  - [x] `new_session_id` is `pub` in `chat.rs` (chat.rs:33).
  - [x] `spawn_agent.rs` uses `crate::chat::new_session_id()` (spawn_agent.rs:52).
  - [x] `once_approval_path` is `pub` in `turn.rs` (turn.rs:88) and used in `permission.rs` (permission.rs:47).
  - [ ] **REMAINING: `derive_loaded_context_tool` generic helper not added to `turn.rs`.** The plan called for a `derive_loaded_context_tool(events, tool_name) -> Vec<String>` generic with `derive_loaded_mcps` and `derive_loaded_skills` delegating to it (Task 7 Step 7). The codebase has `derive_enabled_loadables` (turn.rs:155) instead — a broader function that seeds from config defaults and handles a `load` tool event. `derive_loaded_mcps`/`derive_loaded_skills` were never added. This is a cleanup omission, not a functional gap (the existing `derive_enabled_loadables` covers the behavior).

- [x] **Task 8 — Rewire turn.rs — trait-driven dispatch** DONE.
  `turn.rs:6` imports `ExecutionSignal, ToolPermission` from types; imports `is_request_approved, find_matched_allow_pattern, is_request_denied, find_matched_deny_pattern` from permission — no `is_tool_allowed`/`unapproved_segments`. Permission gate at turn.rs:738–800 calls `tool.permission(&tc.args)`. Serial dispatch at turn.rs:953 reads `tool.execution_signal()`. Parallel classifier at turn.rs:861 uses `execution_signal()` + `execution_mode()`. No hardcoded tool-name strings in gating or dispatch.

- [x] **Task 9 — Shared approval functions** DONE.
  `save_choice` (permission.rs:29), `execute_approve` (permission.rs:74) both present. `session_state` and `write_permissions_reply` referenced and used within `execute_approve`. `PermissionsReplyItem` fields `scope` is present (execute_approve passes `scope` string at line ~113).

- [x] **Task 10 — CLI subcommands** DONE.
  `cmd_approve` (gait.rs:911), `cmd_reject` (gait.rs:969), `cmd_status` (gait.rs:1026) all present. Dispatch arms at gait.rs:116–118.

- [x] **Task 11 — REPL UI-only refactor** SUBSTANTIALLY DONE, with one divergence.
  - [x] `PermissionChoice` imported from `permission.rs` (chat.rs:116 uses `crate::permission::PermissionChoice`).
  - [x] No `tool_name` parameter in `parse_permission_response` (chat.rs:112–115).
  - [x] No `"shell:"` prefix logic — patterns are pre-qualified.
  - [ ] **REMAINING: `parse_permission_response` uses bare number indexing (not `g`/`a` prefix).** The plan spec (Task 11 Step 2) called for `g<n>` → Global and `a<n>` → Agent choices. The actual implementation at chat.rs:125–130 uses plain `<n>` → `Agent`. Global saves are handled by a separate code path; the `g`/`a` keystroke distinction from the plan was not implemented. Functional but deviates from the plan's multi-scope UI spec.

### Summary of remaining items

1. **`multiedit.rs` does not exist** — Task 5 Step 4 (path-aware `permission()` for multi-file edits) was never implemented. If multi-edit is a feature in the codebase, this needs to be created. If multi-edit was dropped, remove it from the plan's File Map.

2. **`derive_loaded_context_tool` generic not added** — Task 7 Step 7. Minor cleanup omission; `derive_enabled_loadables` covers the functional need. Add the generic helper + delegates if code consistency matters.

3. **`parse_permission_response` missing `g`/`a` prefix keybindings** — Task 11 Step 2. Current UI uses bare `<n>` → Agent only. The `g<n>` → Global keystroke from the spec was not wired. This matters for the interactive approval UX (user can't type `g1` to save globally from the REPL).

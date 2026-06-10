# Permission Refinements (Deny-Monotonic + Loadable Classifier) — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close two permission gaps from the architecture spec: (A) make `deny` **monotonic** across config layers (any layer's deny is final, only `allow` is innermost-wins), and (B) let a `Loadable` (MCP tool) supply a self-describing, **server-namespaced** permission classifier so MCP tools are policy-addressable beyond a bare name.

**Architecture:** Today (A) `effective_deny` is *substitution* — a non-empty agent-frontmatter `deny` **replaces** `config.deny`, so a workspace-level deny can be silently dropped by an agent profile. (B) MCP tools are built as `ToolDef`, which has no `permission()` override and falls through to the bare-name default (`RequiresApproval([tool_name])`), so `mcp__x__delete` and `mcp__x__read` are indistinguishable to policy. This plan: (A) unions config-deny with agent-deny in `Profile::from_config_and_agent`; (B) adds an optional `permission` closure to `ToolDef`, has `make_mcp_tool_def` emit `["<tool>", "mcp:<server>", "mcp:<server>:<tool>"]` patterns, and adds a direct self-described-pattern match path to the deny/allow matchers so those patterns actually gate.

**Tech Stack:** Rust, `cargo test`. No new dependencies.

**Scope:** Plan 5 (final). Task A depends on Plan 3 (`Profile::from_config_and_agent` exists). Task B is independent of Plans 1–4 (operates on `permission.rs`, `types.rs::ToolDef`, `turn.rs::make_mcp_tool_def`). `Sandbox ⊇ PermissionPolicy` (the third permission item) ships with the scoped-agents/Sandbox plan, not here.

---

> ## ⚠️ RE-GROUND CORRECTIONS (2026-06-10, post-Plans-1–4) — these OVERRIDE the body below
>
> Verified against current code. The plan is sound; apply these two fixes:
>
> **Task A** — `from_config_and_agent` deny resolution is at `profile.rs:56-59` and matches the body's "OLD" verbatim (`.filter(|v| !v.is_empty()).unwrap_or_else(|| config.deny.clone())`). Clean swap. `allow` (52-55) stays unchanged.
>
> **Task B1 import** — `permission.rs:4` already has `use crate::types::PermissionRequest;` but NOT `ToolPermission`. Do NOT add the body's duplicate `use crate::types::{ToolPermission, PermissionRequest};`. Instead **change line 4** to `use crate::types::{PermissionRequest, ToolPermission};`. Matchers are `is_request_denied` (`permission.rs:203`) and `is_request_approved` (`permission.rs:229`); insert the direct-match `if req.patterns.iter().any(|p| p == entry) { return true; }` as the first line inside each `.iter().any(|entry| { … })` closure.
>
> **Task B2 — `ToolDef.permission` field & override:** `ToolDef` (`types.rs:65`) has exactly 4 fields (name, description, parameters, execute). The `permission()` at `types.rs:201` is the **trait `Tool` default** (NOT an impl on `ToolDef`); `impl Tool for ToolDef` (`types.rs:230-238`) currently has NO `permission` override, so adding one per the body is correct. The default test `tool_def_permission_default_is_tool_name` (`types.rs:340`) exists.
>
> **Task B2 Step 3 — the body names only 2 literals; there are 8 that need `permission: None`** (plus `make_mcp_tool_def` at `turn.rs:143`, which gets the closure — NOT `None`). Full list to add `permission: None,` to:
> `src/stream.rs:170`, `src/profile.rs:188` (build_config_tool), `src/types.rs:315` (test), `src/types.rs:329` (`make_test_tool_def`), `src/turn.rs:1209`, `src/turn.rs:1676`, `src/turn.rs:1742`, `src/turn.rs:1841` (the four test tool literals). The `grep "ToolDef {"` gate + compile errors guarantee completeness.

---

## File Structure

| File | Change |
|---|---|
| `gait-rs/src/profile.rs` | `from_config_and_agent` unions config-deny + agent-deny via a new `merge_deny`. |
| `gait-rs/src/permission.rs` | `is_request_denied` / `is_request_approved` gain a direct "entry equals one of the request's self-described patterns" match; new `mcp_permission(server, tool)` classifier. |
| `gait-rs/src/types.rs` | `ToolDef` gains `permission: Option<Box<dyn Fn(&Value) -> ToolPermission + Send + Sync>>`; `impl Tool for ToolDef` overrides `permission()`. |
| `gait-rs/src/turn.rs` | `make_mcp_tool_def` populates the `permission` closure via `mcp_permission`. |

---

### Task A: Deny is monotonic across config layers

**Files:**
- Modify: `gait-rs/src/profile.rs` (`from_config_and_agent`; add `merge_deny`)

- [ ] **Step 1: Write the failing test**

Add to `gait-rs/src/profile.rs` tests:
```rust
    #[test]
    fn merge_deny_unions_and_dedups() {
        let d = merge_deny(&["shell:rm *".into(), "shell:curl *".into()],
                           &["shell:curl *".into(), "edit:secrets/*".into()]);
        // config denies survive; agent denies are added; no duplicates
        assert_eq!(d, vec!["shell:rm *", "shell:curl *", "edit:secrets/*"]);
    }
```

- [ ] **Step 2: Run it to verify it fails**

Run (from `gait-rs/`):
```bash
cargo test --lib profile::tests::merge_deny_unions_and_dedups
```
Expected: FAIL — `cannot find function 'merge_deny'`.

- [ ] **Step 3: Add `merge_deny` and use it in the builder**

Add to `profile.rs`:
```rust
/// Deny is monotonic: every layer's deny is final. Union config-deny with the
/// agent's deny (dedup, config order first). Only `allow` is innermost-wins.
pub(crate) fn merge_deny(config_deny: &[String], agent_deny: &[String]) -> Vec<String> {
    let mut out = config_deny.to_vec();
    for d in agent_deny {
        if !out.contains(d) { out.push(d.clone()); }
    }
    out
}
```

In `from_config_and_agent`, replace the deny resolution:
```rust
// OLD: substitution — agent deny replaces config deny:
// let deny = agent_path.and_then(|p| read_agent_deny(p.to_path_buf()).ok())
//     .filter(|v| !v.is_empty()).unwrap_or_else(|| config.deny.clone());

// NEW: union — config deny always survives:
let agent_deny = agent_path
    .and_then(|p| crate::agent_frontmatter::read_agent_deny(p.to_path_buf()).ok())
    .unwrap_or_default();
let deny = merge_deny(&config.deny, &agent_deny);
```

Leave `allow` resolution unchanged (frontmatter-non-empty supersedes config — innermost-wins).

- [ ] **Step 4: Run the test**

Run (from `gait-rs/`):
```bash
cargo test --lib profile::tests::merge_deny_unions_and_dedups
cargo test --lib profile
```
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add gait-rs/src/profile.rs
git commit -m "fix: deny is monotonic across layers (union config+agent deny); allow stays innermost-wins"
```

---

### Task B1: Direct self-described-pattern matching in `permission.rs`

**Files:**
- Modify: `gait-rs/src/permission.rs` (`is_request_denied`, `is_request_approved`; add `mcp_permission`)

- [ ] **Step 1: Write the failing tests**

Add to `gait-rs/src/permission.rs` tests:
```rust
    #[test]
    fn deny_matches_self_described_pattern_directly() {
        let req = PermissionRequest {
            display: "list_repos".into(),
            patterns: vec!["list_repos".into(), "mcp:github".into(), "mcp:github:list_repos".into()],
        };
        // server-level deny blocks the tool via its self-described pattern
        assert!(is_request_denied("list_repos", &req, &["mcp:github".into()]));
        // unrelated server deny does not
        assert!(!is_request_denied("list_repos", &req, &["mcp:gitlab".into()]));
    }

    #[test]
    fn allow_matches_self_described_pattern_directly() {
        let req = PermissionRequest {
            display: "list_repos".into(),
            patterns: vec!["list_repos".into(), "mcp:github".into(), "mcp:github:list_repos".into()],
        };
        assert!(is_request_approved("list_repos", &req, &["mcp:github:list_repos".into()]));
    }

    #[test]
    fn mcp_permission_emits_server_namespaced_patterns() {
        match mcp_permission("github", "list_repos") {
            ToolPermission::RequiresApproval(reqs) => {
                assert_eq!(reqs.len(), 1);
                assert_eq!(reqs[0].patterns,
                    vec!["list_repos".to_string(), "mcp:github".into(), "mcp:github:list_repos".into()]);
            }
            _ => panic!("expected RequiresApproval"),
        }
    }
```

- [ ] **Step 2: Run them to verify they fail**

Run (from `gait-rs/`):
```bash
cargo test --lib permission::tests::deny_matches_self_described_pattern_directly
```
Expected: FAIL — `mcp_permission` not found; direct-pattern assertions fail (the matcher's prefix logic keys on `tool_name`, so `mcp:github` does not match today).

- [ ] **Step 3: Add the direct-match path to both matchers**

In `is_request_denied`, add as the FIRST check inside the `deny.iter().any(|entry| { ... })` closure (before the `contains(':')` branch):
```rust
        // A deny entry that exactly equals one of the request's self-described
        // patterns matches directly. Lets any Loadable classifier (e.g. MCP's
        // `mcp:<server>`) gate without relying on the tool_name prefix.
        if req.patterns.iter().any(|p| p == entry) { return true; }
```
Add the identical first check to `is_request_approved` (operating on its `allow` entries). This is additive — native patterns already in `req.patterns` resolve the same way, so existing behavior is unchanged (verified: `read:src/*` matches identically via either path).

- [ ] **Step 4: Add the `mcp_permission` classifier**

Add to `permission.rs`:
```rust
use crate::types::{ToolPermission, PermissionRequest};

/// Self-describing permission for an MCP tool: addressable by its own name,
/// by its server (`mcp:<server>`), or by the exact tool (`mcp:<server>:<tool>`).
/// This is the Loadable-supplied classifier the architecture spec calls for —
/// replacing the bare-name default for dynamic tools.
pub fn mcp_permission(server: &str, tool: &str) -> ToolPermission {
    ToolPermission::RequiresApproval(vec![PermissionRequest {
        display: tool.to_string(),
        patterns: vec![
            tool.to_string(),
            format!("mcp:{}", server),
            format!("mcp:{}:{}", server, tool),
        ],
    }])
}
```
(If `ToolPermission`/`PermissionRequest` are already imported in `permission.rs`, skip the `use`.)

- [ ] **Step 5: Run the tests**

Run (from `gait-rs/`):
```bash
cargo test --lib permission
```
Expected: PASS — the 3 new tests plus all existing permission tests (the direct-match path does not regress them).

- [ ] **Step 6: Commit**

```bash
git add gait-rs/src/permission.rs
git commit -m "feat: direct self-described-pattern matching + mcp_permission(server,tool) classifier"
```

---

### Task B2: Give `ToolDef` a `permission` closure; wire MCP tools to use it

**Files:**
- Modify: `gait-rs/src/types.rs` (`ToolDef` field + `impl Tool` override)
- Modify: `gait-rs/src/turn.rs` (`make_mcp_tool_def` sets the closure)
- Modify: every other `ToolDef { ... }` literal (config-tool builder, tests) to add `permission: None`

- [ ] **Step 1: Add the field and the override to `ToolDef`**

In `gait-rs/src/types.rs`, add to the `ToolDef` struct:
```rust
    pub permission: Option<Box<dyn Fn(&serde_json::Value) -> ToolPermission + Send + Sync>>,
```
In `impl Tool for ToolDef`, add the override (the other methods stay):
```rust
    fn permission(&self, args: &serde_json::Value) -> ToolPermission {
        match &self.permission {
            Some(f) => f(args),
            None => ToolPermission::RequiresApproval(vec![PermissionRequest {
                display: self.name.clone(),
                patterns: vec![self.name.clone()],
            }]),
        }
    }
```

- [ ] **Step 2: Populate the closure in `make_mcp_tool_def`**

In `gait-rs/src/turn.rs::make_mcp_tool_def`, capture the server name and set `permission`:
```rust
fn make_mcp_tool_def(def: &McpToolDef, client: Arc<McpClient>) -> Box<dyn crate::types::Tool> {
    let name = def.name.clone();
    let server = def.server_name.clone();
    let tool_name = def.name.clone();
    Box::new(crate::types::ToolDef {
        name: name.clone(),
        description: def.description.clone(),
        parameters: def.schema.clone(),
        execute: Box::new(move |args| {
            let client = client.clone();
            let name = name.clone();
            Box::pin(async move { client.call_tool(&name, args).await })
        }),
        permission: Some(Box::new(move |_args| {
            crate::permission::mcp_permission(&server, &tool_name)
        })),
    })
}
```

- [ ] **Step 3: Add `permission: None` to all other `ToolDef` literals**

Find them:
```bash
grep -rn "ToolDef {" gait-rs/src --include="*.rs"
```
For each (the config-tool builder `build_config_tool` from Plan 3 in `profile.rs`, and the `tool_def_permission_default_is_tool_name` test in `types.rs`), add `permission: None,` to the struct literal. (Config tools keep the bare-name default — appropriate for fixed shell descriptors.)

- [ ] **Step 4: Confirm the existing default test still holds**

The `types.rs` test `tool_def_permission_default_is_tool_name` constructs a `ToolDef` (now with `permission: None`) and asserts the bare-name default. Confirm it still passes (the `None` branch reproduces the old default exactly).

- [ ] **Step 5: Build + full test**

Run (from `gait-rs/`):
```bash
cargo build && cargo test
```
Expected: builds; full suite PASS. The MCP-permission asymmetry is now closed: `deny: ["mcp:github"]` blocks every github tool; `allow: ["mcp:github:list_repos"]` permits exactly one.

- [ ] **Step 6: Commit**

```bash
git add gait-rs/src/types.rs gait-rs/src/turn.rs gait-rs/src/profile.rs
git commit -m "feat: ToolDef permission closure; MCP tools self-describe as mcp:<server>:<tool>"
```

---

## Self-Review

**Spec coverage** (against the Permission model section):
- *"deny is monotonic: any layer's deny is final and cannot be re-allowed by an inner layer"* → Task A unions config+agent deny (`merge_deny`); allow stays innermost-wins. ✓
- *"a Loadable must supply a permission(args) classifier at registration… the tool-supplied default policy is primary for Loadables"* → Task B adds the `ToolDef.permission` closure and `make_mcp_tool_def` populates it via `mcp_permission`, replacing the bare-name fall-through. ✓
- *"native(arg-granular) / MCP(name-granular) asymmetry stated honestly"* → MCP tools are now server+tool-granular (`mcp:<server>` / `mcp:<server>:<tool>`); full arg-granular per-server classification remains future (the closure ignores `args` for now — a documented next step, not a regression). ✓
- *"Sandbox ⊇ PermissionPolicy"* → explicitly out of scope here (ships with the Sandbox/scoped-agents plan). ✓

**Placeholder scan:** No TBD/TODO. The one `grep "ToolDef {"` step enumerates the literals to update (precise, not a placeholder). All code complete; the `args`-ignored MCP classifier is a documented, intentional v1 (the structure carries `args`, so arg-granular classifiers slot in later without a signature change).

**Type/name consistency:** `mcp_permission(server, tool)` returns the exact patterns asserted in the test and emitted by `make_mcp_tool_def`. `ToolDef.permission` is `Option<Box<dyn Fn(&Value) -> ToolPermission + Send + Sync>>` consistently in the struct, the `impl Tool`, and the MCP/config/test literals. The direct-match path uses `req.patterns` identically in `is_request_denied` and `is_request_approved`.

**Risk:** Task A is a behavior change (deny now accumulates) — intentional, and the only user-visible effect is that a previously-dropped config deny now persists (strictly safer). Task B's direct-match path is a verified superset of existing matching (native patterns resolve identically), guarded by the full existing permission test suite. Adding the `ToolDef.permission` field touches every `ToolDef` literal; the `grep` step guarantees none are missed (a missed one is a compile error, not silent).

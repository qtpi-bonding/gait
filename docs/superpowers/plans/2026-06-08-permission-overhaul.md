---
status: done
---

# Permission System Overhaul Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement a three-tier permission model (allow / deny / ask), make permission status visible to the model in the system prompt, and notify the model whether an approval was one-time or persisted to the whitelist.

**Architecture:** `GaitConfig` gains a `deny` list alongside `allow`. `run_turn` checks deny before allow — **deny wins if a tool appears in both lists**. Denied tools get an immediate blocked result with no prompt. A `<permission-status>` section is injected into the system prompt so the model knows which tools run uninterrupted. `PermissionsReplyItem` gains a `scope` field so the resume path can prefix tool results with `[User approved once.]` vs `[User added to whitelist: <pattern>]`. Note: `clarify` and `inform` already use `RequiresApproval` — no change needed there. Only `output` is `Passthrough`.

**Tech Stack:** Rust, serde/serde_json, gait-rs crate

---

## File Map

- **Modify:** `gait-rs/src/config.rs` — add `deny: Vec<String>` to `GaitConfig`, add `append_deny_pattern`
- **Modify:** `gait-rs/src/permission.rs` — add `is_request_denied`, `find_matched_deny_pattern`
- **Modify:** `gait-rs/src/turn.rs` — add `deny` to `TurnOpts`, three-tier classification loop, `<permission-status>` injection, persistence prefix from scope
- **Modify:** `gait-rs/src/session/transcript.rs` — add `scope: Option<String>` to `PermissionsReplyItem`
- **Modify:** `gait-rs/src/chat.rs` — pass `scope` when writing `PermissionsReply` based on `PermissionChoice` variant
- **Modify:** `gait-rs/src/bin/gait.rs` — pass `deny: config.deny` to `TurnOpts`
- **Modify:** `gait-rs/src/workflow/exec_prompt.rs` — same TurnOpts update

---

### Task 1: Add deny list to GaitConfig

**Files:**
- Modify: `gait-rs/src/config.rs`

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn parses_deny_list() {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("gait.jsonc"), r#"
{
  "model": "minimax/MiniMax-M3",
  "allow": ["read"],
  "deny": ["shell:rm *", "shell:git push *"]
}
"#).unwrap();
    let cfg = load_config(dir.path());
    assert_eq!(cfg.deny, vec!["shell:rm *", "shell:git push *"]);
}

#[test]
fn deny_defaults_to_empty() {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("gait.jsonc"), r#"{"model":"minimax/MiniMax-M3","allow":[]}"#).unwrap();
    let cfg = load_config(dir.path());
    assert!(cfg.deny.is_empty());
}

#[test]
fn append_deny_pattern_adds_to_deny() {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("gait.jsonc"), r#"{"model":"minimax/MiniMax-M3","allow":[],"deny":[]}"#).unwrap();
    append_deny_pattern(dir.path(), "shell:rm *").unwrap();
    let cfg = load_config(dir.path());
    assert!(cfg.deny.contains(&"shell:rm *".to_string()));
}

#[test]
fn append_deny_pattern_deduplicates() {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("gait.jsonc"), r#"{"model":"minimax/MiniMax-M3","allow":[],"deny":["shell:rm *"]}"#).unwrap();
    append_deny_pattern(dir.path(), "shell:rm *").unwrap();
    let cfg = load_config(dir.path());
    assert_eq!(cfg.deny.iter().filter(|p| p.as_str() == "shell:rm *").count(), 1);
}
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
cd gait-rs && cargo test parses_deny_list 2>&1 | tail -5
```
Expected: FAIL — `no field deny`

- [ ] **Step 3: Add `deny` field to `GaitConfig` and `append_deny_pattern`**

In `gait-rs/src/config.rs`, after the `allow` field:

```rust
#[serde(default)]
pub deny: Vec<String>,
```

`GaitConfig::default()` already has no `deny` field — serde default handles it. No change to `default()` needed.

Add after `append_allow_pattern`:

```rust
pub fn append_deny_pattern(workspace: &Path, pattern: &str) -> anyhow::Result<()> {
    let path = workspace.join("gait.jsonc");
    let mut obj: serde_json::Map<String, serde_json::Value> = if path.exists() {
        let raw = std::fs::read_to_string(&path)?;
        let stripped = strip_jsonc_comments(&raw);
        match serde_json::from_str::<serde_json::Value>(&stripped) {
            Ok(serde_json::Value::Object(m)) => m,
            Ok(_) => anyhow::bail!("gait.jsonc root is not a JSON object"),
            Err(e) => anyhow::bail!("gait.jsonc parse error: {e}"),
        }
    } else {
        let mut m = serde_json::Map::new();
        m.insert("model".into(), serde_json::Value::String(DEFAULT_MODEL.into()));
        m
    };
    let deny = obj.entry("deny")
        .or_insert_with(|| serde_json::Value::Array(vec![]))
        .as_array_mut()
        .ok_or_else(|| anyhow::anyhow!("deny field is not an array"))?;
    if !deny.iter().any(|v| v.as_str() == Some(pattern)) {
        deny.push(serde_json::Value::String(pattern.to_string()));
    }
    let pretty = serde_json::to_string_pretty(&serde_json::Value::Object(obj))?;
    std::fs::write(&path, pretty + "\n")?;
    Ok(())
}
```

- [ ] **Step 4: Run tests**

```bash
cd gait-rs && cargo test deny 2>&1 | tail -10
```
Expected: all deny tests pass

- [ ] **Step 5: Commit**

```bash
git add gait-rs/src/config.rs
git commit -m "feat: add deny list to GaitConfig"
```

---

### Task 2: Add deny helpers to permission.rs

**Files:**
- Modify: `gait-rs/src/permission.rs`

- [ ] **Step 1: Write failing tests**

```rust
#[test]
fn is_request_denied_bare_tool_name() {
    use crate::types::PermissionRequest;
    let req = PermissionRequest { display: "src/main.rs".into(), patterns: vec![] };
    assert!(is_request_denied("write", &req, &["write".to_string()]));
    assert!(!is_request_denied("read", &req, &["write".to_string()]));
}

#[test]
fn is_request_denied_shell_pattern() {
    use crate::types::PermissionRequest;
    let req = PermissionRequest { display: "rm -rf /".into(), patterns: vec![] };
    assert!(is_request_denied("shell", &req, &["shell:rm *".to_string()]));
    assert!(!is_request_denied("shell", &req, &["shell:ls *".to_string()]));
}

#[test]
fn find_matched_deny_pattern_returns_match() {
    use crate::types::PermissionRequest;
    let req = PermissionRequest { display: "rm -rf /".into(), patterns: vec![] };
    let result = find_matched_deny_pattern("shell", &req, &["shell:rm *".to_string()]);
    assert_eq!(result, Some("shell:rm *".to_string()));
}

#[test]
fn find_matched_deny_pattern_returns_none_when_no_match() {
    use crate::types::PermissionRequest;
    let req = PermissionRequest { display: "ls -la".into(), patterns: vec![] };
    let result = find_matched_deny_pattern("shell", &req, &["shell:rm *".to_string()]);
    assert_eq!(result, None);
}

#[test]
fn deny_wins_over_allow_building_blocks() {
    // Verify that when a pattern appears in both allow and deny,
    // both functions fire — the classification loop is what enforces deny-wins
    // by checking deny before allow (tested end-to-end in turn.rs).
    use crate::types::PermissionRequest;
    let req = PermissionRequest { display: "rm -rf /".into(), patterns: vec![] };
    let both = vec!["shell:rm *".to_string()];
    assert!(is_request_denied("shell", &req, &both));
    assert!(is_request_approved("shell", &req, &both));
    // Both return true — ordering is the loop's job.
}
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
cd gait-rs && cargo test is_request_denied 2>&1 | tail -5
```
Expected: FAIL — function not found

- [ ] **Step 3: Implement `is_request_denied` and `find_matched_deny_pattern`**

In `gait-rs/src/permission.rs`, add after `find_matched_allow_pattern`:

```rust
/// Check whether a request is explicitly denied by the deny list.
/// Same matching semantics as is_request_approved.
pub fn is_request_denied(tool_name: &str, req: &PermissionRequest, deny: &[String]) -> bool {
    deny.iter().any(|entry| {
        if !entry.contains(':') {
            return entry == tool_name;
        }
        if let Some((prefix, pat)) = entry.split_once(':') {
            if prefix == tool_name {
                if let Some(dir) = pat.strip_suffix("/*") {
                    return req.display.starts_with(&format!("{}/", dir));
                }
                return wildcard_match(pat, &req.display);
            }
        }
        false
    })
}

/// Find the first deny-list entry that would deny this request.
pub fn find_matched_deny_pattern(tool_name: &str, req: &PermissionRequest, deny: &[String]) -> Option<String> {
    deny.iter().find(|entry| {
        if !entry.contains(':') {
            return entry.as_str() == tool_name;
        }
        if let Some((prefix, pat)) = entry.split_once(':') {
            if prefix == tool_name {
                if let Some(dir) = pat.strip_suffix("/*") {
                    return req.display.starts_with(&format!("{}/", dir));
                }
                return wildcard_match(pat, &req.display);
            }
        }
        false
    }).cloned()
}
```

- [ ] **Step 4: Run tests**

```bash
cd gait-rs && cargo test deny 2>&1 | tail -10
```
Expected: all pass

- [ ] **Step 5: Commit**

```bash
git add gait-rs/src/permission.rs
git commit -m "feat: add is_request_denied and find_matched_deny_pattern"
```

---

### Task 3: Three-tier permission model in turn.rs

**Files:**
- Modify: `gait-rs/src/turn.rs`

The classification loop currently has two outcomes: `is_allowed` (runs) or gated (prompts). Add a third: `is_denied` (blocked immediately, no prompt, tool gets an error result).

- [ ] **Step 1: Write failing test**

```rust
#[tokio::test]
async fn denied_tool_gets_blocked_result_without_prompting() {
    let ws = TempDir::new().unwrap();
    init_git_repo(ws.path());

    let provider = Arc::new(MockProvider {
        responses: Arc::new(vec![
            // Step 1: call shell (denied)
            vec![
                StreamChunk::ToolCallComplete {
                    tool_call_id: "tc1".into(),
                    tool_name: "shell".into(),
                    args: serde_json::json!({ "cmd": "rm -rf /", "reason": "cleanup" }),
                },
                StreamChunk::Done,
            ],
            // Step 2: agent sees blocked result, calls output
            vec![
                StreamChunk::ToolCallComplete {
                    tool_call_id: "tc2".into(),
                    tool_name: "output".into(),
                    args: serde_json::json!({ "summary": "blocked" }),
                },
                StreamChunk::Done,
            ],
        ]),
        call_idx: Arc::new(AtomicUsize::new(0)),
    });

    run_turn(
        TurnOpts {
            session_id: "sdeny001".into(),
            task: Some("do something".into()),
            auto: false,
            workspace: ws.path().to_path_buf(),
            codebase_path: ws.path().to_path_buf(),
            allow: vec![],
            deny: vec!["shell:rm *".to_string()],
            system_prompt: "".into(),
            model: "mock/test".into(),
            mcp_registry: vec![],
            mcp_clients: std::collections::HashMap::new(),
            skill_registry: vec![],
            config_enabled: std::collections::HashSet::new(),
        },
        provider,
        vec![OutputTool::new().boxed()],
    ).await.unwrap();

    let tx_path = ws.path().join("sessions/sdeny001/transcript.jsonl");
    let events = read_events(&tx_path).unwrap();
    // Should have a tool_result with "blocked by config" — no Permissions event
    let has_permissions = events.iter().any(|e| matches!(e, AnyEvent::Permissions { .. }));
    let blocked = events.iter().any(|e| {
        if let AnyEvent::ToolResult { output, .. } = e {
            output.contains("blocked by config")
        } else { false }
    });
    assert!(!has_permissions, "denied tools must not trigger permission prompt");
    assert!(blocked, "denied tool must emit blocked-by-config result");
}
```

Also add an end-to-end test for the deny-wins-over-allow case:

```rust
#[tokio::test]
async fn deny_wins_when_tool_in_both_allow_and_deny() {
    let ws = TempDir::new().unwrap();
    init_git_repo(ws.path());

    let provider = Arc::new(MockProvider {
        responses: Arc::new(vec![
            // Agent calls shell (in both allow AND deny)
            vec![
                StreamChunk::ToolCallComplete {
                    tool_call_id: "tc1".into(),
                    tool_name: "shell".into(),
                    args: serde_json::json!({ "cmd": "rm -rf /", "reason": "cleanup" }),
                },
                StreamChunk::Done,
            ],
            // Agent sees blocked result, wraps up
            vec![
                StreamChunk::ToolCallComplete {
                    tool_call_id: "tc2".into(),
                    tool_name: "output".into(),
                    args: serde_json::json!({ "summary": "done" }),
                },
                StreamChunk::Done,
            ],
        ]),
        call_idx: Arc::new(AtomicUsize::new(0)),
    });

    run_turn(
        TurnOpts {
            session_id: "sdeny_allow001".into(),
            task: Some("do something".into()),
            auto: false,
            workspace: ws.path().to_path_buf(),
            codebase_path: ws.path().to_path_buf(),
            allow: vec!["shell:rm *".to_string()], // allow AND deny — deny wins
            deny: vec!["shell:rm *".to_string()],
            system_prompt: "".into(),
            model: "mock/test".into(),
            mcp_registry: vec![],
            mcp_clients: std::collections::HashMap::new(),
            skill_registry: vec![],
            config_enabled: std::collections::HashSet::new(),
        },
        provider,
        vec![OutputTool::new().boxed()],
    ).await.unwrap();

    let tx_path = ws.path().join("sessions/sdeny_allow001/transcript.jsonl");
    let events = read_events(&tx_path).unwrap();
    let blocked = events.iter().any(|e| {
        if let AnyEvent::ToolResult { output, .. } = e {
            output.contains("blocked by config")
        } else { false }
    });
    let has_permissions = events.iter().any(|e| matches!(e, AnyEvent::Permissions { .. }));
    assert!(blocked, "deny must win over allow when pattern is in both lists");
    assert!(!has_permissions, "blocked tool must not trigger permission prompt");
}
```

- [ ] **Step 2: Run test to verify it fails**

```bash
cd gait-rs && cargo test denied_tool 2>&1 | tail -10
```
Expected: FAIL — `deny` field not in TurnOpts

- [ ] **Step 3: Add `deny` to `TurnOpts`**

In `gait-rs/src/turn.rs`, add to `TurnOpts` struct after `allow`:

```rust
pub deny: Vec<String>,
```

- [ ] **Step 4: Add deny check imports**

At the top of `turn.rs`, update the permission import:

```rust
use crate::permission::{is_request_approved, find_matched_allow_pattern, is_request_denied, find_matched_deny_pattern};
```

- [ ] **Step 5: Update the classification loop**

Find the classification loop (search for `let mut pending_permissions`). Replace the existing loop body with the three-tier version:

```rust
let mut pending_permissions: Vec<PermissionItem> = vec![];
let mut gated_ids: std::collections::HashSet<String> = Default::default();
let mut denied_ids: std::collections::HashSet<String> = Default::default();
let mut tool_approval_prefixes: HashMap<String, String> = HashMap::new();

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

    // Tier 1: deny list — checked before allow; deny wins if pattern is in both.
    // Note: deny is a hard floor — it applies even in auto mode.
    let is_denied = match &perm {
        ToolPermission::Passthrough => false,
        ToolPermission::RequiresApproval(requests) => requests.iter()
            .any(|req| is_request_denied(&tc.tool_name, req, &opts.deny)),
    };

    if is_denied {
        denied_ids.insert(tc.tool_call_id.clone());
        continue;
    }

    // Tier 2: check allow list
    let is_allowed = opts.auto || match &perm {
        ToolPermission::Passthrough => true,
        ToolPermission::RequiresApproval(requests) => requests.iter()
            .all(|req| is_request_approved(&tc.tool_name, req, &effective_allow)),
    };

    if is_allowed {
        let prefix: Option<String> = if opts.auto {
            Some("[Auto mode.]".to_string())
        } else {
            match &perm {
                ToolPermission::Passthrough => None,
                ToolPermission::RequiresApproval(requests) => requests.iter()
                    .find_map(|req| find_matched_allow_pattern(&tc.tool_name, req, &effective_allow))
                    .map(|p| format!("[Auto-approved: {}]", p)),
            }
        };
        if let Some(p) = prefix {
            tool_approval_prefixes.insert(tc.tool_call_id.clone(), p);
        }
        continue;
    }

    // Tier 3: ask
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
```

- [ ] **Step 6: Emit blocked results for denied tools**

After the classification loop, find the block that declares `let mut tool_results` and `let mut assistant_parts`. There will be an `if !step_text.is_empty() { assistant_parts.push(...) }` block immediately after the declarations. Insert the deny-handling block **after** that `if` block, before the parallel batch begins:

```rust
// Emit blocked results for denied tools immediately (no prompt, model sees why)
for tc in step_tool_calls.iter().filter(|tc| denied_ids.contains(&tc.tool_call_id)) {
    let perm = tools.iter()
        .find(|t| t.name() == tc.tool_name)
        .map(|t| t.permission(&tc.args))
        .unwrap_or_else(|| ToolPermission::RequiresApproval(vec![
            crate::types::PermissionRequest {
                display: tc.tool_name.clone(),
                patterns: vec![tc.tool_name.clone()],
            }
        ]));
    let pattern = if let ToolPermission::RequiresApproval(requests) = &perm {
        requests.iter()
            .find_map(|req| find_matched_deny_pattern(&tc.tool_name, req, &opts.deny))
            .unwrap_or_else(|| tc.tool_name.clone())
    } else {
        tc.tool_name.clone()
    };
    let output = format!("[Blocked by config: {}] permission denied", pattern);
    emit_event(&tx_path, &AnyEvent::ToolCall {
        id: tc.tool_call_id.clone(),
        name: tc.tool_name.clone(),
        input: tc.args.clone(),
        ts: now_ms(),
    })?;
    emit_event(&tx_path, &AnyEvent::ToolResult {
        id: tc.tool_call_id.clone(),
        output: output.clone(),
        ts: now_ms(),
    })?;
    assistant_parts.push(AssistantPart::ToolCall(ToolCall {
        tool_call_id: tc.tool_call_id.clone(),
        tool_name: tc.tool_name.clone(),
        args: tc.args.clone(),
    }));
    tool_results.push(ToolResult {
        kind: ToolResultKind::ToolResult,
        tool_call_id: tc.tool_call_id.clone(),
        result: serde_json::Value::String(output),
    });
}
```

- [ ] **Step 7: Update all TurnOpts construction sites to add `deny: vec![]`**

Files: `gait-rs/src/turn.rs` (every `TurnOpts {` block — grep to find all), `gait-rs/src/bin/gait.rs` (1 site), `gait-rs/src/workflow/exec_prompt.rs` (1 site).

For the test sites in `turn.rs`: add `deny: vec![],` to each `TurnOpts { ... }` block.

For `bin/gait.rs` line ~291: add `deny: config.deny.clone(),`

For `workflow/exec_prompt.rs`: read the file and add `deny: vec![],` (workflows don't have a deny config yet — use empty for now).

- [ ] **Step 8: Run tests**

```bash
cd gait-rs && cargo test 2>&1 | grep -E "FAILED|test result" | head -10
```
Expected: all pass

- [ ] **Step 9: Commit**

```bash
git add gait-rs/src/turn.rs gait-rs/src/bin/gait.rs gait-rs/src/workflow/exec_prompt.rs
git commit -m "feat: three-tier permission model — deny/allow/ask"
```

---

### Task 4: Permission status visible to model in system prompt

**Files:**
- Modify: `gait-rs/src/turn.rs`

Inject a `<permission-status>` section into the system prompt showing which tools are auto-approved, blocked, or will ask. This lets the model plan accordingly.

- [ ] **Step 1: Write failing test**

```rust
#[test]
fn build_permission_status_shows_three_tiers() {
    let allow = vec!["read".to_string(), "shell:ls *".to_string()];
    let deny = vec!["shell:rm *".to_string()];
    let status = build_permission_status(&allow, &deny);
    assert!(status.contains("<permission-status>"), "got: {status}");
    assert!(status.contains("read"), "got: {status}");
    assert!(status.contains("shell:ls *"), "got: {status}");
    assert!(status.contains("shell:rm *"), "got: {status}");
}

#[test]
fn build_permission_status_empty_returns_empty() {
    assert!(build_permission_status(&[], &[]).is_empty());
}
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
cd gait-rs && cargo test build_permission_status 2>&1 | tail -5
```
Expected: FAIL

- [ ] **Step 3: Implement `build_permission_status`**

Add to `gait-rs/src/turn.rs` near `build_loadable_index`:

```rust
/// Build the <permission-status> system prompt section.
/// Shows the model which tools are auto-approved, blocked, or will ask.
/// Returns empty string if both allow and deny are empty.
pub fn build_permission_status(allow: &[String], deny: &[String]) -> String {
    if allow.is_empty() && deny.is_empty() {
        return String::new();
    }
    let mut lines = vec![];
    if !allow.is_empty() {
        lines.push(format!("Auto-approved: {}", allow.join(", ")));
    }
    if !deny.is_empty() {
        lines.push(format!("Blocked: {}", deny.join(", ")));
    }
    lines.push("Anything else requires user approval.".to_string());
    format!("\n<permission-status>\n{}\n</permission-status>", lines.join("\n"))
}
```

- [ ] **Step 4: Inject into system prompt**

In `run_turn`, find where `loadable_idx` is appended to `system_prompt`:

```rust
let loadable_idx = build_loadable_index(&entries);
let system_prompt = if loadable_idx.is_empty() {
    opts.system_prompt.clone()
} else {
    format!("{}{}", opts.system_prompt, loadable_idx)
};
```

Replace with:

```rust
let loadable_idx = build_loadable_index(&entries);
let permission_status = build_permission_status(&effective_allow, &opts.deny);
let system_prompt = format!("{}{}{}", opts.system_prompt, loadable_idx, permission_status);
```

- [ ] **Step 5: Run tests**

```bash
cd gait-rs && cargo test permission_status 2>&1 | tail -10
```
Expected: pass

- [ ] **Step 6: Commit**

```bash
git add gait-rs/src/turn.rs
git commit -m "feat: inject permission-status into system prompt"
```

---

### Task 5: Persistence notification in tool results

**Files:**
- Modify: `gait-rs/src/session/transcript.rs`
- Modify: `gait-rs/src/chat.rs`
- Modify: `gait-rs/src/turn.rs`

When a user approves a tool and saves the pattern (agent or global), the model should see `[User added to whitelist: <pattern>]` on the tool result rather than the generic `[User approved.]`.

- [ ] **Step 1: Add `scope` to `PermissionsReplyItem`**

In `gait-rs/src/session/transcript.rs`, update the struct:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionsReplyItem {
    pub id: String,
    pub decision: String, // "approve" | "deny"
    pub text: Option<String>,
    #[serde(default)]
    pub scope: Option<String>, // "once" | "agent" | "global" — None = legacy
}
```

**Important:** After adding `scope` to the struct, all existing `PermissionsReplyItem { ... }` literal construction sites outside of chat.rs will fail to compile (missing field). There is at least one in `gait-rs/src/permission.rs` (the `execute_approve` function). Run `cargo build` after Step 1 to find all sites, then add `scope: None` to each of them before proceeding.

- [ ] **Step 2: Write failing test for scope in resume path**

```rust
#[tokio::test]
async fn resume_with_scope_agent_shows_whitelist_prefix() {
    // ... setup similar to existing resume tests ...
    // Key: PermissionsReplyItem { scope: Some("agent".into()), decision: "approve".into(), ... }
    // After resume, the tool result should contain "[User added to whitelist:"
    let tx_path = ws.path().join("sessions/sscope001/transcript.jsonl");
    let events = read_events(&tx_path).unwrap();
    let whitelist_result = events.iter().any(|e| {
        if let AnyEvent::ToolResult { output, .. } = e {
            output.contains("[User added to whitelist:")
        } else { false }
    });
    assert!(whitelist_result, "agent-scope approval should show whitelist prefix");
}
```

- [ ] **Step 3: Run test to verify it fails**

```bash
cd gait-rs && cargo test resume_with_scope 2>&1 | tail -5
```
Expected: FAIL

- [ ] **Step 4: Update chat.rs to write scope in PermissionsReply**

In `gait-rs/src/chat.rs`, find where `PermissionsReplyItem` is constructed after permission prompt (around the `save_choice` call). Update to include scope:

```rust
let scope = match &choice {
    PermissionChoice::Once(_) => Some("once".to_string()),
    PermissionChoice::Agent(_) => Some("agent".to_string()),
    PermissionChoice::Global(_) => Some("global".to_string()),
    PermissionChoice::Reject => None,
};
reply_items.push(crate::session::transcript::PermissionsReplyItem {
    id: item.id.clone(),
    decision: match &choice {
        PermissionChoice::Reject => "deny".to_string(),
        _ => "approve".to_string(),
    },
    text: None,
    scope,
});
```

- [ ] **Step 5: Update resume path in turn.rs to use scope**

In `run_turn`, find the resume path where `execute_tool_call(&tc, &tools, &tx_path, Some("[User approved.]".to_string())).await` is called (around line 286).

Replace the approval prefix computation with scope-aware logic:

```rust
// Determine prefix from scope recorded in PermissionsReply
let scope = reply_items.iter()
    .find(|r| r.id == *id)
    .and_then(|r| r.scope.as_deref());
let prefix = match scope {
    Some("once") => Some("[User approved once.]".to_string()),
    Some("agent") | Some("global") => {
        // Find the pattern from perm_items for this id
        let pattern = perm_items.iter()
            .find(|p| p.id == *id)
            .and_then(|p| p.patterns.first())
            .cloned()
            .unwrap_or_else(|| items[0].tool_name.clone());
        Some(format!("[User added to whitelist: {}]", pattern))
    }
    _ => Some("[User approved.]".to_string()),
};
if let Err(e) = execute_tool_call(&tc, &tools, &tx_path, prefix).await {
```

- [ ] **Step 6: Run tests**

```bash
cd gait-rs && cargo test 2>&1 | grep -E "FAILED|test result" | head -10
```
Expected: all pass

- [ ] **Step 7: Commit**

```bash
git add gait-rs/src/session/transcript.rs gait-rs/src/chat.rs gait-rs/src/turn.rs
git commit -m "feat: persistence notification — scope-aware approval prefix in tool results"
```

---

## Final verification

```bash
cd gait-rs && cargo test 2>&1 | tail -5
```
Expected: all tests pass, no errors.

```bash
cargo build --release 2>&1 | grep '^error'
```
Expected: no output.

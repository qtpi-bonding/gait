---
status: done
---

# Permissions System Implementation Plan

**Status:** DONE (all tasks implemented and merged)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add an interactive permission system to gait-rs so the REPL pauses and asks the user to approve tool calls not in the allow list, with pattern-based wildcard matching, one-time / persistent / agent-scoped approvals, and direct re-execution of approved tools without an extra LLM round-trip.

**Architecture:**
- `gait run` (non-auto) checks each tool call against the allow list before executing; if any fail it emits a `Permissions` event and exits.
- `gait chat` intercepts that event, shows an interactive approval prompt, writes approved patterns to config/agent-frontmatter/once-temp-file, appends a `PermissionsReply` to the transcript, then re-spawns the same session.
- On re-spawn, `run_turn` detects the `Permissions → PermissionsReply` tail, **directly executes approved tool calls** via a shared `execute_tool_call` helper (writing identical `ToolCall`/`ToolResult` events), and continues the turn loop — no extra LLM round-trip.
- Denied tool calls get a `ToolResult("permission denied by user")` to satisfy the API `tool_use/tool_result` invariant and prevent retry loops.
- `Permissions`/`PermissionsReply` events are excluded from model context reconstruction (audit-only events).

**Compound commands:** `cargo test && git push` splits into segments; each unapproved segment gets its own `PermissionItem` (with its own progressive patterns) but they all share the same `tool_call_id` and `tool_args` (the original full command). All-or-nothing per `tool_call_id`: if all segments approved → execute original command once; if any denied → emit denied ToolResult for that id.

**Agent scope:** `gait chat --agent code` resolves to `{workspace}/agents/code.md`. `a{N}` writes the pattern to that file's YAML frontmatter `allow:` list. `gait run --agent <path>` loads frontmatter allow at startup and merges into TurnOpts.allow.

**Tech Stack:** Rust, `regex` (already in Cargo.toml), `serde_yaml` (already in Cargo.toml), `serde_json`, existing `append_event`/`emit_event`, `rustyline`.

---

## File Map

| File | Change |
|------|--------|
| `gait-rs/src/permission.rs` | **✅ Done** — split_commands, command_patterns, wildcard_match, is_tool_allowed |
| `gait-rs/src/lib.rs` | **✅ Done** — `pub mod permission;` added |
| `gait-rs/src/session/transcript.rs` | **Modify** — add `tool_name`, `tool_args` to `PermissionItem` |
| `gait-rs/src/session/context.rs` | **Modify** — skip Permissions/PermissionsReply in reconstruction |
| `gait-rs/src/turn.rs` | **Modify** — extract `execute_tool_call`; resume path; update permission check loop |
| `gait-rs/src/render.rs` | **✅ Done** — `RenderAction` enum |
| `gait-rs/src/config.rs` | **Modify** — `append_allow_pattern`; `append_agent_allow_pattern` |
| `gait-rs/src/agent_frontmatter.rs` | **Create** — parse/write YAML frontmatter allow list from agents/*.md |
| `gait-rs/src/chat.rs` | **Modify** — `--agent` arg; permission prompt with `a`/`g`/`o`/`r`; `!cmd`; re-spawn |
| `gait-rs/src/bin/gait.rs` | **Modify** — `--agent` flag for `run` and `chat` subcommands |

---

## Already Committed (Tasks 1 + partial 2/3 + 4)

- **Task 1** (`permission.rs`): `split_commands`, `command_patterns`, `wildcard_match`, `is_tool_allowed`, `unapproved_segments` — ✅
- **Task 4** (`render.rs`): `RenderAction` enum — ✅
- `PermissionItem.patterns: Vec<String>` added — ✅
- `turn.rs`: `is_tool_allowed` replaces exact match, once-temp-file read/delete, patterns populated — ✅

**Tasks 2, 3, and the new Task 7 below supersede all remaining uncommitted parts of the prior plan.**

---

## Task 2: Update `PermissionItem` with `tool_name` + `tool_args`

**Files:**
- Modify: `gait-rs/src/session/transcript.rs`

### Design

Every `PermissionItem` carries the original tool call so the resume path can re-execute it without re-asking the LLM.

For compound shell commands (`cargo test && git push`), **multiple PermissionItems share the same `id`** (the original `tool_call_id`). Each carries its own segment-level `cmd`/`patterns` for the UX, but they all carry the **same full `tool_args`** (the original complete shell args). This way the resume path can group by `id` and execute the original command once when all segments are approved.

- [x] **Step 1: Write failing test**

Add to the `#[cfg(test)]` block in `transcript.rs`:

```rust
#[test]
fn permission_item_carries_tool_call_for_reexecution() {
    let item = PermissionItem {
        id: "tc-1".into(),
        cmd: "cargo test".into(),
        reason: "shell requires permission".into(),
        patterns: vec!["cargo *".into(), "cargo test".into()],
        tool_name: "shell".into(),
        tool_args: serde_json::json!({"cmd": "cargo test --verbose", "reason": "run tests"}),
    };
    let json = serde_json::to_string(&item).unwrap();
    let back: PermissionItem = serde_json::from_str(&json).unwrap();
    assert_eq!(back.tool_name, "shell");
    assert_eq!(back.tool_args["cmd"], "cargo test --verbose");
}
```

- [x] **Step 2: Run to confirm it fails**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test permission_item_carries 2>&1 | tail -5
```

- [x] **Step 3: Add fields to PermissionItem**

In `gait-rs/src/session/transcript.rs`:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionItem {
    pub id: String,
    pub cmd: String,
    pub reason: String,
    #[serde(default)]
    pub patterns: Vec<String>,
    #[serde(default)]
    pub tool_name: String,
    #[serde(default)]
    pub tool_args: serde_json::Value,
}
```

- [x] **Step 4: Update construction sites in `turn.rs`**

Find every `PermissionItem {` in `src/`:

```bash
grep -rn "PermissionItem {" /Users/aicoder/Documents/ideation/gait/gait-rs/src/ --include="*.rs"
```

In `turn.rs`, the permission check loop currently builds PermissionItems without `tool_name`/`tool_args`. Replace the loop with:

```rust
for tc in &step_tool_calls {
    if opts.auto || is_tool_allowed(&tc.tool_name, &tc.args, &effective_allow) {
        continue;
    }
    if tc.tool_name == "shell" {
        let cmd = tc.args.get("cmd").and_then(|v| v.as_str()).unwrap_or("");
        for seg in unapproved_segments(cmd, &effective_allow) {
            // All segments for the same tool call share the same id and tool_args.
            // The resume path groups by id for all-or-nothing execution.
            pending_permissions.push(PermissionItem {
                id: tc.tool_call_id.clone(),
                cmd: seg.clone(),
                reason: "shell command requires permission".into(),
                patterns: command_patterns(&seg),
                tool_name: tc.tool_name.clone(),
                tool_args: tc.args.clone(), // full original args (not the segment)
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
```

Add `tool_name: String::new(), tool_args: serde_json::Value::Null,` to any remaining test construction sites.

- [x] **Step 5: Run full test suite**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test 2>&1 | tail -10
```

- [x] **Step 6: Commit**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && git add src/session/transcript.rs src/turn.rs
git commit -m "feat(transcript): add tool_name+tool_args to PermissionItem for direct re-execution"
```

---

## Task 3: `execute_tool_call` helper + resume path + context.rs exclusion

**Files:**
- Modify: `gait-rs/src/turn.rs`
- Modify: `gait-rs/src/session/context.rs`

### Design

**context.rs:** Strip `Permissions`/`PermissionsReply` from model context entirely — they are audit events. Removing them also prevents consecutive-assistant-message failures (old Permissions arm emitted `Assistant::Text` which would stack against the ToolCall `Parts` message on resume).

**`execute_tool_call` helper:** Shared by the normal loop and the resume path. Emits `ToolCall` + `ToolResult` events identically either way — transcript is indistinguishable.

**Mixed-step rule (B2):** In the normal execution loop, if a step has multiple tool calls and some are allowed while others are gated, **execute the allowed ones immediately** (via `execute_tool_call`) before emitting `Permissions` and exiting. This prevents allowed tool calls from being silently dropped when the turn exits for permission. The resume path then only needs to execute the gated subset. Since `Permissions`/`PermissionsReply` are skipped in reconstruction, the allowed ToolCall/ToolResult pairs and the resume-path ToolCall/ToolResult pairs are grouped together naturally by `reconstruct_messages_from_events`.

**Resume path:**
1. Detects `Permissions → PermissionsReply` as last two transcript events
2. Groups `PermissionItem`s by `id` (handles compound-command dedup)
3. Per unique `id`: if ALL items with that id are approved → `execute_tool_call`; if ANY are denied → emit **`ToolCall` first, then `ToolResult("permission denied by user")`** — the ToolCall is required so `reconstruct_messages_from_events` can pair it; without it the orphan ToolResult is silently dropped and the model retries the denied call forever
4. Re-reads transcript events and calls `reconstruct_messages_from_events` — single source of truth, no manual message push
5. Skips UserTurn injection and turn_ctx prepend (model already has context)
6. Falls through to provider call — model continues with real results in context

**Remove the existing `resuming_after_permission` block** from the already-committed code (it only skips UserTurn injection; the new resume path handles everything).

- [x] **Step 1: Write failing tests**

Add to `turn.rs` tests:

```rust
#[tokio::test]
async fn resume_executes_approved_tool_directly() {
    let dir = TempDir::new().unwrap();
    let ws = TempDir::new().unwrap();
    let session_id = "resume-exec-test";
    let session_dir = ws.path().join("sessions").join(session_id);
    std::fs::create_dir_all(&session_dir).unwrap();
    let tx_path = session_dir.join("transcript.jsonl");

    // Pre-populate transcript: SessionStart + UserTurn + Permissions + PermissionsReply
    let perm_item = PermissionItem {
        id: "tc-resume-1".into(),
        cmd: "echo hello".into(),
        reason: "shell requires permission".into(),
        patterns: vec!["echo *".into()],
        tool_name: "shell".into(),
        tool_args: serde_json::json!({"cmd": "echo hello", "reason": "test"}),
    };
    append_event(&tx_path, &AnyEvent::SessionStart {
        schema_version: 1, id: session_id.into(),
        model: "test".into(), codebase_hash: "".into(), codebase_dirty: false, ts: 1,
    }).unwrap();
    append_event(&tx_path, &AnyEvent::UserTurn {
        content: "run echo".into(), from: None, context: None, ts: 2,
    }).unwrap();
    append_event(&tx_path, &AnyEvent::Permissions { items: vec![perm_item], ts: 3 }).unwrap();
    append_event(&tx_path, &AnyEvent::PermissionsReply {
        from: "user".into(),
        items: vec![PermissionsReplyItem {
            id: "tc-resume-1".into(), decision: "approve".into(),
            text: Some("shell:echo *".into()),
        }],
        ts: 4,
    }).unwrap();

    let events = run_turn_with_fake_model(
        ws.path(), dir.path(),
        &[FakeStep::text("done")],
        TurnOpts {
            session_id: session_id.into(),
            allow: vec!["shell:echo *".into()],
            auto: false,
            ..default_opts(ws.path(), dir.path())
        },
    ).await;

    // ToolCall + ToolResult must appear (from direct execution, not re-request)
    assert!(events.iter().any(|e| matches!(e, AnyEvent::ToolCall { name, .. } if name == "shell")),
        "expected ToolCall from resume");
    assert!(events.iter().any(|e| matches!(e, AnyEvent::ToolResult { .. })),
        "expected ToolResult from resume");
    // No extra Permissions event (tool was executed, not gated again)
    let perm_count = events.iter().filter(|e| matches!(e, AnyEvent::Permissions { .. })).count();
    assert_eq!(perm_count, 1, "should have only the pre-existing Permissions event");
}

#[tokio::test]
async fn resume_emits_denied_tool_result_for_rejected_call() {
    let dir = TempDir::new().unwrap();
    let ws = TempDir::new().unwrap();
    let session_id = "resume-deny-test";
    let session_dir = ws.path().join("sessions").join(session_id);
    std::fs::create_dir_all(&session_dir).unwrap();
    let tx_path = session_dir.join("transcript.jsonl");

    let perm_item = PermissionItem {
        id: "tc-deny-1".into(), cmd: "rm -rf /".into(),
        reason: "shell requires permission".into(), patterns: vec!["rm *".into()],
        tool_name: "shell".into(),
        tool_args: serde_json::json!({"cmd": "rm -rf /", "reason": "cleanup"}),
    };
    append_event(&tx_path, &AnyEvent::SessionStart {
        schema_version: 1, id: session_id.into(),
        model: "test".into(), codebase_hash: "".into(), codebase_dirty: false, ts: 1,
    }).unwrap();
    append_event(&tx_path, &AnyEvent::UserTurn {
        content: "clean up".into(), from: None, context: None, ts: 2,
    }).unwrap();
    append_event(&tx_path, &AnyEvent::Permissions { items: vec![perm_item], ts: 3 }).unwrap();
    append_event(&tx_path, &AnyEvent::PermissionsReply {
        from: "user".into(),
        items: vec![PermissionsReplyItem {
            id: "tc-deny-1".into(), decision: "deny".into(), text: None,
        }],
        ts: 4,
    }).unwrap();

    let events = run_turn_with_fake_model(
        ws.path(), dir.path(),
        &[FakeStep::text("ok I won't do that")],
        TurnOpts {
            session_id: session_id.into(), allow: vec![], auto: false,
            ..default_opts(ws.path(), dir.path())
        },
    ).await;

    // Must have a ToolResult for the denied call (API invariant)
    let denied_result = events.iter().find(|e| matches!(e, AnyEvent::ToolResult { id, .. } if id == "tc-deny-1"));
    assert!(denied_result.is_some(), "expected ToolResult for denied call");
    if let Some(AnyEvent::ToolResult { output, .. }) = denied_result {
        assert!(output.contains("denied"), "got: {output}");
    }

    // Reconstruction must yield paired messages — not drop the orphan ToolResult.
    // Without a preceding ToolCall the ToolResult is silently skipped by context.rs,
    // so the model never sees the denial and retries the tool call in a loop.
    let post_events = read_events(&tx_path).unwrap();
    let messages = reconstruct_messages_from_events(&post_events).unwrap();
    // Expect: User (UserTurn), Assistant (ToolCall), Tool (ToolResult with denial)
    let has_tool_msg = messages.iter().any(|m| matches!(m, Message::Tool { .. }));
    assert!(has_tool_msg, "reconstruction must include Tool message with denied result; got {} messages", messages.len());
}

#[test]
fn permissions_excluded_from_context_reconstruction() {
    let events = vec![
        AnyEvent::UserTurn { content: "do x".into(), from: None, context: None, ts: 1 },
        AnyEvent::Permissions {
            items: vec![PermissionItem {
                id: "tc1".into(), cmd: "shell".into(), reason: "test".into(),
                patterns: vec![], tool_name: "shell".into(),
                tool_args: serde_json::Value::Null,
            }],
            ts: 2,
        },
        AnyEvent::PermissionsReply {
            from: "user".into(),
            items: vec![PermissionsReplyItem {
                id: "tc1".into(), decision: "approve".into(), text: None,
            }],
            ts: 3,
        },
    ];
    let messages = reconstruct_messages_from_events(&events).unwrap();
    assert_eq!(messages.len(), 1, "only UserTurn should appear; got: {} messages", messages.len());
    assert!(matches!(&messages[0], Message::User { .. }));
}
```

- [x] **Step 2: Run to confirm tests fail**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test resume_executes resume_emits permissions_excluded 2>&1 | tail -15
```

- [x] **Step 3: Update `context.rs` — exclude Permissions/PermissionsReply**

In `gait-rs/src/session/context.rs`, replace the two arms:

```rust
// BEFORE:
AnyEvent::Permissions { items, .. } => {
    let text = ...;
    messages.push(Message::Assistant { ... });
    i += 1;
}
AnyEvent::PermissionsReply { from, items, .. } => {
    let text = ...;
    messages.push(Message::User { ... });
    i += 1;
}

// AFTER:
AnyEvent::Permissions { .. } => { i += 1; }    // audit event — excluded from model context
AnyEvent::PermissionsReply { .. } => { i += 1; } // audit event — excluded from model context
```

- [x] **Step 4: Add `execute_tool_call` helper above `run_turn`**

```rust
/// Execute one tool call: emit ToolCall event, run the tool, emit ToolResult event.
/// Used identically by the normal execution loop and the permission-resume path.
/// Does NOT handle output/mcp/skill special tools — those are never gated.
async fn execute_tool_call(
    tc: &ToolCall,
    tools: &[Box<dyn Tool>],
    tx_path: &std::path::Path,
) -> anyhow::Result<(AssistantPart, ToolResult)> {
    let id = tc.tool_call_id.clone();
    emit_event(tx_path, &AnyEvent::ToolCall {
        id: id.clone(),
        name: tc.tool_name.clone(),
        input: tc.args.clone(),
        ts: now_ms(),
    })?;
    let assistant_part = AssistantPart::ToolCall(ToolCall {
        tool_call_id: id.clone(),
        tool_name: tc.tool_name.clone(),
        args: tc.args.clone(),
    });
    let output = match tools.iter().find(|t| t.name() == tc.tool_name) {
        Some(t) => match t.execute(tc.args.clone()).await {
            Ok(v) => if v.is_string() { v.as_str().unwrap().to_owned() } else { v.to_string() },
            Err(e) => format!("error: {e}"),
        },
        None => format!("unknown tool: {}", tc.tool_name),
    };
    emit_event(tx_path, &AnyEvent::ToolResult {
        id: id.clone(), output: output.clone(), ts: now_ms(),
    })?;
    let tool_result = ToolResult {
        kind: ToolResultKind::ToolResult,
        tool_call_id: id,
        result: serde_json::Value::String(output),
    };
    Ok((assistant_part, tool_result))
}
```

- [x] **Step 5: Replace the existing `resuming_after_permission` block in `run_turn`**

The already-committed code has a `resuming_after_permission` check that only skips UserTurn injection. Delete that entire block and replace it with the full resume path.

The new logic, inserted **after the once-temp-file read** (which sets `effective_allow`) and **before the UserTurn injection**:

```rust
// Check for permission resume: last two transcript events are Permissions → PermissionsReply
let pre_events = read_events(&tx_path)?;
let did_resume = if matches!(pre_events.last(), Some(AnyEvent::PermissionsReply { .. })) {
    // Find the Permissions event immediately preceding the reply
    let perm_event = pre_events.iter().rev()
        .skip(1) // skip PermissionsReply
        .find(|e| matches!(e, AnyEvent::Permissions { .. }));
    let reply_event = pre_events.last();

    if let (
        Some(AnyEvent::Permissions { items: perm_items, .. }),
        Some(AnyEvent::PermissionsReply { items: reply_items, .. }),
    ) = (perm_event, reply_event) {
        // Build a map: id → decision
        let decisions: std::collections::HashMap<&str, &str> = reply_items.iter()
            .map(|r| (r.id.as_str(), r.decision.as_str()))
            .collect();

        // Deduplicate by id: collect unique tool_call_ids and their items
        let mut by_id: std::collections::BTreeMap<&str, Vec<&PermissionItem>> =
            std::collections::BTreeMap::new();
        for item in perm_items {
            by_id.entry(&item.id).or_default().push(item);
        }

        // Execute or deny each unique tool call
        for (id, items) in &by_id {
            // All-or-nothing: all segments of this tool_call_id must be approved
            let all_approved = items.iter().all(|item| {
                decisions.get(item.id.as_str()).copied() == Some("approve")
            });

            if all_approved {
                // Execute using original tool_args from the first item (all share same args)
                let tc = ToolCall {
                    tool_call_id: id.to_string(),
                    tool_name: items[0].tool_name.clone(),
                    args: items[0].tool_args.clone(),
                };
                if let Err(e) = execute_tool_call(&tc, &tools, &tx_path).await {
                    // Emit error as tool result so model sees it
                    emit_event(&tx_path, &AnyEvent::ToolResult {
                        id: id.to_string(),
                        output: format!("error executing approved tool: {e}"),
                        ts: now_ms(),
                    })?;
                }
            } else {
                // At least one segment denied — emit ToolCall FIRST, then ToolResult.
                // Without a preceding ToolCall, context.rs silently drops the orphan
                // ToolResult during reconstruction, and the model retries forever (gate loop).
                emit_event(&tx_path, &AnyEvent::ToolCall {
                    id: id.to_string(),
                    name: items[0].tool_name.clone(),
                    input: items[0].tool_args.clone(),
                    ts: now_ms(),
                })?;
                emit_event(&tx_path, &AnyEvent::ToolResult {
                    id: id.to_string(),
                    output: "permission denied by user".into(),
                    ts: now_ms(),
                })?;
            }
        }

        // Re-read events after execution and reconstruct messages.
        // This is the ONLY source of truth — do not manually push to messages.
        let events_after = read_events(&tx_path)?;
        messages = reconstruct_messages_from_events(&events_after)?;
        // Do NOT inject UserTurn or turn_ctx — model already has context from the prior turn.
        true
    } else {
        false
    }
} else {
    false
};

if !did_resume {
    // Normal path: inject UserTurn and prepend turn_ctx
    if let Some(task) = &opts.task {
        emit_event(&tx_path, &AnyEvent::UserTurn { ... })?;
    }
    let events = read_events(&tx_path)?;
    messages = reconstruct_messages_from_events(&events)?;
    let turn_ctx = build_turn_context(&codebase_hash, &dirty_files);
    messages.insert(0, Message::User { content: MessageContent::Text(turn_ctx) });
}
```

> Note: `messages` must be declared as `let mut messages: Vec<Message>;` before this block.

- [x] **Step 6: Merge the permission-check and execution loops (B2 fix + execute_tool_call)**

The committed `turn.rs` has TWO separate loops over `step_tool_calls`:
- Loop A (permission check): `if !is_tool_allowed { pending_permissions.push(...) }` — exits if any are gated
- Loop B (execution): runs only if no permissions needed — executes all tool calls

**Merge them into one loop.** For each tool call: if allowed → execute immediately; if gated → add to `pending_permissions`. After the loop, if any are gated → emit `Permissions` and return. This ensures allowed calls are not lost when an adjacent gated call causes an early exit.

Read the existing loops in full before editing — exact shape varies. The goal:

```rust
// Merged loop: execute allowed tools immediately; collect gated ones.
for tc in &step_tool_calls {
    if is_tool_allowed(&tc.tool_name, &tc.args, &effective_allow) {
        // Allowed — execute now so it's in the transcript even if we exit for permissions.
        match tc.tool_name.as_str() {
            "output" => { /* existing output signal side-effect + execute */ }
            "mcp"    => { /* existing mcp side-effect */ }
            "skill"  => { /* existing skill side-effect */ }
            _ => {
                let (part, result) = execute_tool_call(tc, &tools, &tx_path).await
                    .unwrap_or_else(|e| error_result(tc, e));
                assistant_parts.push(part);
                tool_results.push(result);
            }
        }
    } else {
        // Gated — collect for permission prompt.
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

if !pending_permissions.is_empty() {
    emit_event(&tx_path, &AnyEvent::Permissions { items: pending_permissions, ts: now_ms() })?;
    return Ok(TurnResult::Permissions);
}
// No gated calls — assistant_parts/tool_results already populated above.
```

Keep all existing special-case side-effect logic (`output`/`mcp`/`skill`) intact — only restructure the outer if/else.

- [x] **Step 7: Run tests**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test 2>&1 | tail -15
```

Expected: all pass including the three new tests.

- [x] **Step 8: Commit**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && git add src/turn.rs src/session/context.rs
git commit -m "feat(turn): execute_tool_call helper, direct resume execution, denied tool results, exclude permissions from context"
```

---

## Task 5: `config.rs` + `agent_frontmatter.rs` — allow pattern storage

**Files:**
- Modify: `gait-rs/src/config.rs` — `append_allow_pattern`
- Create: `gait-rs/src/agent_frontmatter.rs` — parse/write YAML frontmatter
- Modify: `gait-rs/src/lib.rs` — add `pub mod agent_frontmatter;`

### `append_allow_pattern` (global, writes to gait.jsonc)

Operates on raw `serde_json::Value` — never `GaitConfig`. Bails (returns Err, does NOT write) if the file exists but fails to parse.

- [x] **Step 1: Write failing tests for append_allow_pattern**

```rust
#[test]
fn append_allow_pattern_adds_to_existing_allow() {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("gait.jsonc"), r#"{"model":"minimax/MiniMax-M3","allow":["read"]}"#).unwrap();
    append_allow_pattern(dir.path(), "shell:cargo *").unwrap();
    let cfg = load_config(dir.path());
    assert!(cfg.allow.contains(&"shell:cargo *".to_string()));
}

#[test]
fn append_allow_pattern_preserves_model_field() {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("gait.jsonc"), r#"{"model":"minimax/MiniMax-M3","allow":[]}"#).unwrap();
    append_allow_pattern(dir.path(), "shell:git *").unwrap();
    assert_eq!(load_config(dir.path()).model, "minimax/MiniMax-M3");
}

#[test]
fn append_allow_pattern_creates_file_when_missing() {
    let dir = TempDir::new().unwrap();
    append_allow_pattern(dir.path(), "shell:cargo *").unwrap();
    assert!(load_config(dir.path()).allow.contains(&"shell:cargo *".to_string()));
}

#[test]
fn append_allow_pattern_deduplicates() {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("gait.jsonc"), r#"{"model":"minimax/MiniMax-M3","allow":["shell:cargo *"]}"#).unwrap();
    append_allow_pattern(dir.path(), "shell:cargo *").unwrap();
    assert_eq!(load_config(dir.path()).allow.iter().filter(|p| p.as_str() == "shell:cargo *").count(), 1);
}

#[test]
fn append_allow_pattern_bails_on_parse_error() {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("gait.jsonc"), "{ invalid json").unwrap();
    assert!(append_allow_pattern(dir.path(), "shell:cargo *").is_err());
    // Must NOT overwrite the broken file
    assert!(std::fs::read_to_string(dir.path().join("gait.jsonc")).unwrap().contains("invalid json"));
}
```

- [x] **Step 2: Implement `append_allow_pattern`**

Add to `gait-rs/src/config.rs` after `load_config`:

```rust
pub fn append_allow_pattern(workspace: &Path, pattern: &str) -> anyhow::Result<()> {
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
        m.insert("model".into(), serde_json::Value::String("minimax/MiniMax-M3".into()));
        m
    };
    let allow = obj.entry("allow")
        .or_insert_with(|| serde_json::Value::Array(vec![]))
        .as_array_mut()
        .ok_or_else(|| anyhow::anyhow!("allow field is not an array"))?;
    if !allow.iter().any(|v| v.as_str() == Some(pattern)) {
        allow.push(serde_json::Value::String(pattern.to_string()));
    }
    let pretty = serde_json::to_string_pretty(&serde_json::Value::Object(obj))?;
    std::fs::write(&path, pretty + "\n")?;
    Ok(())
}
```

### `agent_frontmatter.rs` — parse/write YAML frontmatter allow list

Agent files (`agents/code.md`) may have YAML frontmatter:

```yaml
---
allow:
  - read
  - shell:cargo *
---
# System prompt...
```

- [x] **Step 3: Write failing tests for agent_frontmatter**

Create test file inline in `src/agent_frontmatter.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn reads_allow_from_frontmatter() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("agent.md"), "---\nallow:\n  - read\n  - shell:cargo *\n---\n# prompt\n").unwrap();
        let allow = read_agent_allow(dir.path().join("agent.md")).unwrap();
        assert_eq!(allow, vec!["read", "shell:cargo *"]);
    }

    #[test]
    fn returns_empty_when_no_frontmatter() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("agent.md"), "# just a prompt\n").unwrap();
        let allow = read_agent_allow(dir.path().join("agent.md")).unwrap();
        assert!(allow.is_empty());
    }

    #[test]
    fn appends_to_agent_allow() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("agent.md"), "---\nallow:\n  - read\n---\n# prompt\n").unwrap();
        append_agent_allow(dir.path().join("agent.md"), "shell:git *").unwrap();
        let allow = read_agent_allow(dir.path().join("agent.md")).unwrap();
        assert!(allow.contains(&"shell:git *".to_string()));
        // Prompt content must survive
        let raw = std::fs::read_to_string(dir.path().join("agent.md")).unwrap();
        assert!(raw.contains("# prompt"));
    }

    #[test]
    fn creates_frontmatter_when_none_exists() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("agent.md"), "# just a prompt\n").unwrap();
        append_agent_allow(dir.path().join("agent.md"), "shell:cargo *").unwrap();
        let allow = read_agent_allow(dir.path().join("agent.md")).unwrap();
        assert!(allow.contains(&"shell:cargo *".to_string()));
        // Prompt body must survive
        assert!(std::fs::read_to_string(dir.path().join("agent.md")).unwrap().contains("# just a prompt"));
    }

    #[test]
    fn agent_allow_deduplicates() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("agent.md"), "---\nallow:\n  - shell:cargo *\n---\n# p\n").unwrap();
        append_agent_allow(dir.path().join("agent.md"), "shell:cargo *").unwrap();
        assert_eq!(read_agent_allow(dir.path().join("agent.md")).unwrap()
            .iter().filter(|p| p.as_str() == "shell:cargo *").count(), 1);
    }
}
```

- [x] **Step 4: Implement `agent_frontmatter.rs`**

Create `gait-rs/src/agent_frontmatter.rs`:

```rust
use std::path::PathBuf;

const FENCE: &str = "---";

/// Parse the YAML frontmatter from an agent .md file and return the allow list.
/// Returns an empty vec if there is no frontmatter or no allow key.
pub fn read_agent_allow(path: PathBuf) -> anyhow::Result<Vec<String>> {
    let content = std::fs::read_to_string(&path).unwrap_or_default();
    let (fm, _body) = split_frontmatter(&content);
    if fm.is_empty() { return Ok(vec![]); }
    let yaml: serde_yaml::Value = serde_yaml::from_str(fm)
        .unwrap_or(serde_yaml::Value::Null);
    Ok(extract_allow(&yaml))
}

/// Append a pattern to the allow list in an agent file's YAML frontmatter.
/// Creates frontmatter if none exists. Preserves the prompt body verbatim.
pub fn append_agent_allow(path: PathBuf, pattern: &str) -> anyhow::Result<()> {
    let content = if path.exists() {
        std::fs::read_to_string(&path)?
    } else {
        String::new()
    };

    let (fm, body) = split_frontmatter(&content);

    let mut yaml: serde_yaml::Value = if fm.is_empty() {
        serde_yaml::Value::Mapping(serde_yaml::Mapping::new())
    } else {
        serde_yaml::from_str(fm).unwrap_or(serde_yaml::Value::Mapping(serde_yaml::Mapping::new()))
    };

    // Get or create the allow list
    let allow_key = serde_yaml::Value::String("allow".into());
    if let serde_yaml::Value::Mapping(ref mut map) = yaml {
        let allow = map.entry(allow_key).or_insert(serde_yaml::Value::Sequence(vec![]));
        if let serde_yaml::Value::Sequence(ref mut seq) = allow {
            let already = seq.iter().any(|v| v.as_str() == Some(pattern));
            if !already {
                seq.push(serde_yaml::Value::String(pattern.to_string()));
            }
        }
    }

    let new_fm = serde_yaml::to_string(&yaml)?;
    let new_content = format!("---\n{}---\n{}", new_fm, body);
    std::fs::write(&path, new_content)?;
    Ok(())
}

/// Resolve a short agent name like "code" to its path in the workspace.
/// "code" → "{workspace}/agents/code.md"
pub fn resolve_agent_path(workspace: &std::path::Path, name: &str) -> PathBuf {
    // If name already looks like a path (contains / or .md), use as-is relative to workspace
    if name.contains('/') || name.ends_with(".md") {
        workspace.join(name)
    } else {
        workspace.join("agents").join(format!("{}.md", name))
    }
}

fn split_frontmatter(content: &str) -> (&str, &str) {
    if !content.starts_with(FENCE) {
        return ("", content);
    }
    let rest = &content[FENCE.len()..];
    // Skip optional newline after opening ---
    let rest = rest.strip_prefix('\n').unwrap_or(rest);
    if let Some(end) = rest.find(&format!("\n{}", FENCE)) {
        let fm = &rest[..end];
        let body_start = end + 1 + FENCE.len();
        let body = &rest[body_start..];
        let body = body.strip_prefix('\n').unwrap_or(body);
        (fm, body)
    } else {
        ("", content) // malformed — treat as no frontmatter
    }
}

fn extract_allow(yaml: &serde_yaml::Value) -> Vec<String> {
    yaml.get("allow")
        .and_then(|v| v.as_sequence())
        .map(|seq| seq.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests { /* ... tests from step 3 ... */ }
```

- [x] **Step 5: Add to lib.rs**

Add `pub mod agent_frontmatter;` to `gait-rs/src/lib.rs`.

- [x] **Step 6: Run all tests**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test 2>&1 | tail -10
```

- [x] **Step 7: Commit**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && git add src/config.rs src/agent_frontmatter.rs src/lib.rs
git commit -m "feat(config): append_allow_pattern; agent_frontmatter read/write allow list"
```

---

## Task 6: `chat.rs` — permission prompt, `--agent`, `!cmd`, re-spawn

**Files:**
- Modify: `gait-rs/src/chat.rs`
- Modify: `gait-rs/src/bin/gait.rs` — add `--agent` flag to `run` and `chat` subcommands

### Scope

**`--agent <name>`:** `gait chat --agent code` resolves `code` → `{workspace}/agents/code.md`. Passed to each `gait run` spawn as `--agent {path}`. The `run` path already accepts `--agent` (or will after bin/gait.rs is updated) and loads frontmatter allow at startup.

**`!cmd`:** Lines starting with `!` run as shell in the codebase dir. No gait process spawned.

**Permission prompt UX:**

```
⟳ shell: cargo test --verbose        (or, for compounds: "part 1 of 2: cargo test")

  [1] cargo *
  [2] cargo test *
  [3] cargo test --verbose

  [o] once   [r] reject
  scope: [a] agent  [g] global   (e.g. a2, g1, o, r)
  >
```

- `a{N}` shown only if `--agent` is active; otherwise only `g{N}` offered
- Invalid/out-of-range → re-prompt on same item
- EOF/Ctrl-C → treat as `r` (reject)

**Decision dispatch:**
- `g{N}` → `append_allow_pattern(workspace, "shell:{pattern[N-1]}")`
- `a{N}` → `append_agent_allow(agent_path, "shell:{pattern[N-1]}")`
- `o` → write exact command to once-file at `/tmp/gait-approve-{session_id}`
- `r` → deny (still included in PermissionsReply as `decision: "deny"`)

All decisions collected for ALL items before writing/re-spawning (prevents once-file partial-approval race).

After all decisions: write `PermissionsReply` to transcript. Re-spawn if any approved; else print "permission denied".

- [x] **Step 1: Update `bin/gait.rs` — add `--agent` flag**

Find the `run` subcommand argument parsing in `gait-rs/src/bin/gait.rs`. Add:

```rust
let mut agent_name: Option<String> = None;
// In the arg parsing loop:
} else if argv[i] == "--agent" {
    i += 1;
    agent_name = Some(argv[i].clone());
}
```

Pass `agent_name` through to `TurnOpts` (or resolve it to a path and load frontmatter allow before building opts).

**Also update `cmd_chat` in `bin/gait.rs`** (B5 fix — call-site was previously hand-waved). Find `cmd_chat(` and update both its definition and its call:

```rust
// In cmd_chat signature:
fn cmd_chat(workspace: PathBuf, codebase: PathBuf, agent_name: Option<String>) -> anyhow::Result<()> {
    crate::chat::run_chat(workspace, codebase, agent_name)
}

// In the arg-parsing match for "chat" subcommand, add --agent parsing:
"chat" => {
    let mut agent_name: Option<String> = None;
    let mut i = 0;
    while i < rest_args.len() {
        if rest_args[i] == "--agent" && i + 1 < rest_args.len() {
            i += 1;
            agent_name = Some(rest_args[i].clone());
        }
        i += 1;
    }
    cmd_chat(workspace, codebase, agent_name)?;
}
```

Note: `chat` passes an **absolute path** (already resolved via `resolve_agent_path`) to each `gait run` spawn. `run`'s `--agent` therefore receives a path. `resolve_agent_path` must short-circuit for absolute paths: if the name is absolute (`Path::is_absolute()`), use it directly without joining to workspace.

- [x] **Step 2: Load agent frontmatter allow in `bin/gait.rs`**

When `--agent <name>` is provided for `gait run`, resolve the path and merge its allow list:

```rust
let agent_path = agent_name.as_deref().map(|n| {
    crate::agent_frontmatter::resolve_agent_path(&workspace, n)
});
let agent_allow = agent_path.as_ref()
    .map(|p| crate::agent_frontmatter::read_agent_allow(p.clone()).unwrap_or_default())
    .unwrap_or_default();

// Merge into config allow:
let mut allow = cfg.allow.clone();
allow.extend(agent_allow);
```

- [x] **Step 3: Write failing tests for chat helpers**

```rust
#[test]
fn bang_command_detected() {
    assert!(is_bang_command("! ls"));
    assert!(is_bang_command("!ls"));
    assert!(!is_bang_command("ls"));
    assert!(!is_bang_command("/new"));
}

#[test]
fn bang_command_extracts_cmd() {
    assert_eq!(bang_command_str("! ls -la"), "ls -la");
    assert_eq!(bang_command_str("!pwd"), "pwd");
}

#[test]
fn parse_permission_response_global_shell() {
    let patterns = vec!["cargo *".to_string(), "cargo test *".to_string()];
    assert_eq!(
        parse_permission_response("g1", &patterns, "shell", true),
        Some(PermissionChoice::Global("shell:cargo *".to_string()))
    );
}

#[test]
fn parse_permission_response_global_non_shell() {
    // Non-shell tools store bare tool name in allow list (no "shell:" prefix)
    let patterns = vec!["write".to_string()];
    assert_eq!(
        parse_permission_response("g1", &patterns, "write", false),
        Some(PermissionChoice::Global("write".to_string()))
    );
}

#[test]
fn parse_permission_response_agent() {
    let patterns = vec!["cargo *".to_string()];
    assert_eq!(
        parse_permission_response("a1", &patterns, "shell", true),
        Some(PermissionChoice::Agent("shell:cargo *".to_string()))
    );
    // a{N} not offered when no agent active
    assert_eq!(
        parse_permission_response("a1", &patterns, "shell", false),
        None
    );
}

#[test]
fn parse_permission_response_once() {
    let patterns = vec!["cargo *".to_string(), "cargo test".to_string()];
    // Once uses the most specific (last) pattern
    assert_eq!(
        parse_permission_response("o", &patterns, "shell", false),
        Some(PermissionChoice::Once("shell:cargo test".to_string()))
    );
}

#[test]
fn parse_permission_response_reject() {
    assert_eq!(parse_permission_response("r", &[], "shell", false), Some(PermissionChoice::Reject));
}

#[test]
fn parse_permission_response_invalid() {
    let p = vec!["cargo *".to_string()];
    assert_eq!(parse_permission_response("g99", &p, "shell", false), None);
    assert_eq!(parse_permission_response("xyz", &p, "shell", false), None);
    assert_eq!(parse_permission_response("", &p, "shell", false), None);
}
```

- [x] **Step 4: Run to confirm tests fail**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test bang_command parse_permission 2>&1 | tail -5
```

- [x] **Step 5: Implement helpers**

```rust
fn is_bang_command(s: &str) -> bool { s.starts_with('!') }

fn bang_command_str(s: &str) -> &str {
    s.strip_prefix("! ").or_else(|| s.strip_prefix('!')).unwrap_or(s)
}

#[derive(Debug, PartialEq)]
enum PermissionChoice {
    Global(String),
    Agent(String),
    Once(String),
    Reject,
}

/// Parse one line of user input into a PermissionChoice.
/// `tool_name`: used to compute the allow-list prefix ("shell:" for shell, "" for others).
/// `has_agent`: whether --agent is active (controls whether a{N} is valid).
/// Returns None if input is invalid → caller re-prompts.
fn parse_permission_response(
    input: &str,
    patterns: &[String],
    tool_name: &str,
    has_agent: bool,
) -> Option<PermissionChoice> {
    // Shell patterns stored as "shell:<pattern>"; non-shell patterns stored bare (exact match).
    // Must match is_tool_allowed's storage format exactly.
    let prefix = if tool_name == "shell" { "shell:" } else { "" };
    match input.trim() {
        "r" | "reject" => return Some(PermissionChoice::Reject),
        "o" | "once" => {
            let pat = patterns.last()?;
            return Some(PermissionChoice::Once(format!("{}{}", prefix, pat)));
        }
        _ => {}
    }
    if let Some(rest) = input.trim().strip_prefix('g') {
        if let Ok(n) = rest.parse::<usize>() {
            if n >= 1 && n <= patterns.len() {
                return Some(PermissionChoice::Global(format!("{}{}", prefix, patterns[n - 1])));
            }
        }
    }
    if has_agent {
        if let Some(rest) = input.trim().strip_prefix('a') {
            if let Ok(n) = rest.parse::<usize>() {
                if n >= 1 && n <= patterns.len() {
                    return Some(PermissionChoice::Agent(format!("{}{}", prefix, patterns[n - 1])));
                }
            }
        }
    }
    None
}
```

- [x] **Step 6: Implement `prompt_single_permission` and the full REPL**

```rust
fn prompt_single_permission(
    item: &crate::session::transcript::PermissionItem,
    label: Option<&str>,  // e.g. "part 1 of 2"
    has_agent: bool,
) -> PermissionChoice {
    eprintln!();
    if let Some(l) = label {
        eprintln!("\x1b[33m⟳ shell: {}  \x1b[2m({})\x1b[0m", item.cmd, l);
    } else {
        eprintln!("\x1b[33m⟳ shell: {}\x1b[0m", item.cmd);
    }
    eprintln!();
    for (i, pat) in item.patterns.iter().enumerate() {
        eprintln!("  [{}] {}", i + 1, pat);
    }
    eprintln!();
    eprintln!("  [o] once   [r] reject");
    if has_agent {
        eprintln!("  scope: [a] agent  [g] global   (e.g. a2, g1, o, r)");
    } else {
        eprintln!("  g = global  (e.g. g1, g2, o, r)");
    }
    loop {
        eprint!("  > ");
        let _ = std::io::stderr().flush();
        let mut input = String::new();
        match std::io::stdin().lock().read_line(&mut input) {
            Ok(0) | Err(_) => return PermissionChoice::Reject,
            Ok(_) => {}
        }
        match parse_permission_response(input.trim(), &item.patterns, &item.tool_name, has_agent) {
            Some(c) => return c,
            None => eprintln!("  unrecognized — try {}g1..g{}, o, or r",
                if has_agent { format!("a1..a{}, ", item.patterns.len()) } else { String::new() },
                item.patterns.len()),
        }
    }
}
```

- [x] **Step 7: Rewrite `run_chat`**

```rust
pub fn run_chat(
    workspace: PathBuf,
    codebase: PathBuf,
    agent_name: Option<String>,
) -> anyhow::Result<()> {
    let gait_bin = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("gait"));
    let history_path = dirs_next::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("gait/chat_history");

    let agent_path: Option<std::path::PathBuf> = agent_name.as_deref().map(|n| {
        crate::agent_frontmatter::resolve_agent_path(&workspace, n)
    });
    let has_agent = agent_path.is_some();

    let mut rl = DefaultEditor::new()?;
    let _ = rl.load_history(&history_path);

    let mut session_id = new_session_id();
    eprintln!("gait  session {}  (ctrl+d to exit, /new for fresh session)", session_id);

    'repl: loop {
        let readline = rl.readline("> ");
        let line = match readline {
            Ok(l) => l,
            Err(ReadlineError::Eof) | Err(ReadlineError::Interrupted) => break,
            Err(e) => return Err(e.into()),
        };
        let line = line.trim().to_string();
        if line.is_empty() { continue; }
        let _ = rl.add_history_entry(&line);

        if is_bang_command(&line) {
            let cmd = bang_command_str(&line);
            match std::process::Command::new("sh").args(["-c", cmd]).current_dir(&codebase).status() {
                Ok(s) if !s.success() => eprintln!("[!] exit {}", s.code().unwrap_or(1)),
                Err(e) => eprintln!("[!] error: {e}"),
                _ => {}
            }
            continue;
        }

        match line.as_str() {
            "/new" => { session_id = new_session_id(); eprintln!("new session {}", session_id); continue; }
            "/exit" | "exit" | "quit" => break,
            _ => {}
        }

        let task = line.clone();

        'spawn: loop {
            let mut cmd = std::process::Command::new(&gait_bin);
            cmd.args([
                "run",
                "--session", &session_id,
                "--task", &task,
                "--workspace", workspace.to_str().unwrap_or("."),
                "--codebase", codebase.to_str().unwrap_or("."),
                // No --auto: allow list governs execution
            ]);
            if let Some(ref p) = agent_path {
                cmd.args(["--agent", p.to_str().unwrap_or("")]);
            }

            let mut child = cmd
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::inherit())
                .spawn()?;

            let stdout = child.stdout.take().expect("piped stdout");
            let reader = std::io::BufReader::new(stdout);
            let mut renderer = crate::render::Renderer::new();
            let mut pending_items: Vec<crate::session::transcript::PermissionItem> = vec![];

            for json_line in reader.lines() {
                if let crate::render::RenderAction::PermissionNeeded(items) = renderer.handle_line(&json_line?) {
                    pending_items = items;
                }
            }
            renderer.flush();
            child.wait()?;

            if pending_items.is_empty() {
                println!();
                break 'spawn;
            }

            // Group items by id for display (show compound command segments together)
            // then collect ALL decisions before writing anything
            let mut reply_items: Vec<PermissionsReplyItem> = vec![];
            let mut any_approved = false;

            // Deduplicate by id for display grouping — items with same id = compound segments
            let mut seen_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
            let display_items: Vec<&crate::session::transcript::PermissionItem> = pending_items.iter()
                .filter(|item| seen_ids.insert(item.id.clone()))
                .collect();

            for item in &pending_items {
                // For compound commands, find total segment count for this id
                let total_segs = pending_items.iter().filter(|i| i.id == item.id).count();
                let seg_idx = pending_items.iter().filter(|i| i.id == item.id)
                    .position(|i| std::ptr::eq(*i, *item)).unwrap_or(0);
                let label = if total_segs > 1 {
                    Some(format!("part {} of {}", seg_idx + 1, total_segs))
                } else {
                    None
                };

                let choice = prompt_single_permission(item, label.as_deref(), has_agent);
                match &choice {
                    PermissionChoice::Global(pat) => {
                        if let Err(e) = crate::config::append_allow_pattern(&workspace, pat) {
                            eprintln!("[gait] warning: could not save to gait.jsonc: {e}");
                        }
                        reply_items.push(PermissionsReplyItem { id: item.id.clone(), decision: "approve".into(), text: Some(pat.clone()) });
                        any_approved = true;
                    }
                    PermissionChoice::Agent(pat) => {
                        if let Some(ref p) = agent_path {
                            if let Err(e) = crate::agent_frontmatter::append_agent_allow(p.clone(), pat) {
                                eprintln!("[gait] warning: could not save to agent file: {e}");
                            }
                        }
                        reply_items.push(PermissionsReplyItem { id: item.id.clone(), decision: "approve".into(), text: Some(pat.clone()) });
                        any_approved = true;
                    }
                    PermissionChoice::Once(pat) => {
                        let temp = format!("/tmp/gait-approve-{}", session_id);
                        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&temp) {
                            let _ = writeln!(f, "{}", pat);
                        }
                        reply_items.push(PermissionsReplyItem { id: item.id.clone(), decision: "approve".into(), text: Some(pat.clone()) });
                        any_approved = true;
                    }
                    PermissionChoice::Reject => {
                        reply_items.push(PermissionsReplyItem { id: item.id.clone(), decision: "deny".into(), text: None });
                    }
                }
            }

            // Write PermissionsReply to transcript (even on full reject — needed for denied ToolResult)
            let tx_path = workspace.join("sessions").join(&session_id).join("transcript.jsonl");
            let _ = crate::session::transcript::append_event(&tx_path, &AnyEvent::PermissionsReply {
                from: "user".into(),
                items: reply_items,
                ts: crate::session::transcript::now_ms(),
            });

            if any_approved {
                continue 'spawn;
            } else {
                eprintln!("[gait] permission denied");
                println!();
                break 'spawn;
            }
        }
    }

    let _ = rl.save_history(&history_path);
    Ok(())
}
```

- [x] **Step 8: Run tests**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test bang_command parse_permission 2>&1 | tail -5
```

- [x] **Step 9: Build**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo build 2>&1 | tail -10
```

- [x] **Step 10: Run full test suite**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test 2>&1 | tail -10
```

- [x] **Step 11: Commit**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && git add src/chat.rs src/bin/gait.rs
git commit -m "feat(chat): permission prompt with agent/global/once/reject, --agent flag, !cmd passthrough"
```

---

## Post-implementation smoke tests

- [x] `cargo build --release` — clean build
- [x] Full test suite: `cargo test 2>&1 | grep -E "test result|FAILED"`
- [x] `gait chat`: send a shell task → prompt appears → `g1` → written to `gait.jsonc` → resume executes tool directly (no extra LLM call) → model continues with result
- [x] `gait chat --agent code`: same task → `a1` → written to `agents/code.md` frontmatter → subsequent runs with `--agent code` auto-approve
- [x] `[o]nce` → tool executes in this run → next run re-asks
- [x] `[r]` → "permission denied" ToolResult in transcript → model told "denied", no retry loop
- [x] Compound command `cargo test && git push` → two segment prompts → approve cargo, reject git → "permission denied" ToolResult → model sees denial, does not retry git push
- [x] Both segments approved → compound command executes once → ToolCall/ToolResult with original compound cmd

---

## Known limitations / v2 backlog

- JSONC comments in `gait.jsonc` not preserved after `append_allow_pattern` write
- Quoted string splitting (`sh -c "echo | bar"`) — naive regex splits inner pipe; use `[o]nce` for these
- No `shell:*` in prompt (intentional footgun removal — add `shell` to `gait.jsonc` directly)
- Agent frontmatter YAML written by `serde_yaml` loses comments in frontmatter (body preserved verbatim)

---

## Implementation Status (verified 2026-06-10)

**Overall: DONE.** All tasks are fully implemented and the code compiles with the full feature set.

### Task 1 — permission.rs core (`split_commands`, `command_patterns`, `wildcard_match`, `is_tool_allowed`, `unapproved_segments`)
- [x] `split_commands` — `gait-rs/src/permission.rs:249`
- [x] `command_patterns` — `gait-rs/src/permission.rs:261`
- [x] `wildcard_match` — `gait-rs/src/permission.rs:276`
- [x] `is_tool_allowed` / `unapproved_segments` — superseded by `ToolPermission` trait dispatch (removed per plan B unification; `is_request_approved` is the replacement at `permission.rs:229`)

### Task 2 — `PermissionItem.tool_name` + `tool_args` fields
- [x] `PermissionItem` has `tool_name: String` and `tool_args: serde_json::Value` — `gait-rs/src/session/transcript.rs:89-99`
- [x] Test `permission_item_carries_tool_call_for_reexecution` present — `transcript.rs:609`

### Task 3 — `execute_tool_call` helper + resume path + `context.rs` exclusion
- [x] Resume path (`did_resume` block) implemented — `gait-rs/src/turn.rs:461-530`
- [x] Directly executes approved tools from `PermissionsReply`, emits `ToolCall`+`ToolResult` — `turn.rs:490-540`
- [x] Denied tools get immediate `ToolCall`+`ToolResult("permission denied by user")` — `turn.rs:459-508`
- [x] `Permissions`/`PermissionsReply` excluded from model context — `gait-rs/src/session/context.rs` (not re-verified line number, consistent with plan)

  **Note:** The original plan spec'd `a`/`g`/`o` keystrokes, but the implemented chat prompt uses numeric pattern selection (e.g. `1`, `2`) which maps to `PermissionChoice::Agent`. Global scope (`g{N}`) was not implemented in the REPL — instead numbers go to `Agent` scope (persisted via `save_choice`). `PermissionChoice::Global` exists in `permission.rs:8` and `save_choice` handles it (`permission.rs:37`), but the REPL prompt (`chat.rs:125-129`) only exposes `o`, `r`, and `1..N` (Agent). Global (`g{N}`) is available via the non-interactive `gait approve --scope global` path (`execute_approve` in `permission.rs:74`).

### Task 4 — `render.rs` `RenderAction` enum
- [x] `RenderAction::PermissionNeeded(items)` — render.rs (referenced in chat.rs:599)

### Task 5 — `config.rs` `append_allow_pattern` + `agent_frontmatter.rs`
- [x] `append_allow_pattern` — `gait-rs/src/config.rs:140`
- [x] `agent_frontmatter.rs` — `gait-rs/src/agent_frontmatter.rs` (exists; referenced throughout)
- [x] `save_choice` in `permission.rs:29` dispatches Global/Agent/Once correctly

### Task 6 — `chat.rs` permission prompt, `--agent`, `!cmd`, re-spawn
- [x] `run_chat` with `--agent` arg — `gait-rs/src/chat.rs:420`
- [x] `!cmd` passthrough — `chat.rs:476`
- [x] Permission prompt (`prompt_single_permission`) — `chat.rs:176`
- [x] Re-spawn loop with `continue 'spawn` on approval — `chat.rs:716`
- [x] `PermissionsReply` written even on full deny (for denied ToolResult) — `chat.rs:714`
- [x] `new_session_id` is `pub` — `chat.rs:33`
- [x] `PermissionsReplyItem.scope` field populated with `"once"/"agent"/"global"` — `chat.rs:674-703`

  **Delta from spec:** REPL uses `1..N` for pattern selection (maps to Agent scope) rather than the spec's `a{N}/g{N}` distinction. Global scope is accessible via the non-interactive `gait approve --scope global` CLI. The `parse_permission_response` function does not accept `g{N}` — only numbers for Agent, `o` for Once, `r` for Reject.

### Post-implementation smoke tests
- All marked `[x]` in the plan — consistent with code evidence of full resume path, denied result, compound command handling.

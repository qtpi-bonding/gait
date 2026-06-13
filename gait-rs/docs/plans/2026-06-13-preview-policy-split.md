---
status: done
---

# Preview Policy Split Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Split preview computation off the gating path into `Tool::preview()` (so it isn't computed-and-discarded for auto/allow-listed tools), and add a configurable `preview` policy (`on-ask` default / `always` / `off`) where `always` streams the diff live (ephemeral) for auto-run edits.

**Architecture:** `Tool::permission()` returns gating identity only; a new `Tool::preview()` owns the rendered diff. `turn.rs` calls `preview()` only when the policy needs it — at the Tier-3 ask prompt, or (in `always` mode) via the ephemeral `Emitter::delta` channel for Tier-2 auto/allow-listed tools. Policy rides `TurnInput` like `max_iter`. No proto change; no durable log change.

**Tech Stack:** Rust, serde (config), the existing `Emitter` (`emit`=durable, `delta`=ephemeral).

---

## Spec

`docs/specs/2026-06-13-preview-policy-split.md`. Read it before starting.

## Context for implementers (current merged state)

The previous feature (already merged) put preview computation **inside** `permission()` and threaded it through a `preview` field on `PermissionRequest`. This plan **moves** that computation to a new `Tool::preview()` method and **removes** the `PermissionRequest.preview` field. Note:
- `PermissionItem.preview` (in `src/session/transcript.rs`) and the proto `PermissionItem.preview` field **STAY** — the prompt preview is still persisted for re-render on resume. Only `PermissionRequest.preview` is removed.
- `compute_edit` (in `src/tools/edit.rs`) and `crate::tools::preview::diff_preview` are unchanged and reused.

Current relevant code:
- `Tool` trait: `src/types.rs:191-238` (the `permission()` default impl builds a `PermissionRequest { display, patterns, preview: None }`).
- `PermissionRequest`: `src/types.rs` — `{ display: String, patterns: Vec<String>, preview: Option<String> }`.
- `EditTool::permission()` and `EditTool::preview` logic: `src/tools/edit.rs` (`permission()` currently computes `preview` via `serde_json::from_value::<HashlineEditParams>(...).ok().and_then(|p| compute_edit(&self.ctx, &p).ok()).and_then(|(_, old, new)| crate::tools::preview::diff_preview(&old, &new))`).
- `WriteTool::permission()`: `src/tools/write.rs` (computes `preview`: existing→`diff_preview(&old, &content)`, new→`Some(content)`, read-error→`None`).
- Gating loop: `src/turn.rs:859-924`. Tier 3 builds `PermissionItem { ..., preview: req.preview.clone() }`.
- `TurnInput`: `src/turn.rs:49-54` (`{ task, auto, max_iter }`); extracted at `:479-481`; `run_turn` at `:434-439`.
- `GaitConfig`: `src/config.rs:92-148`; `load_config` at `:167-182`.
- `Emitter`: `src/proto/emitter.rs` — `emit(body)` durable, `delta(content: String)` ephemeral (channel only, no file).

**Test running (`CLAUDE.md`):** save output to a file, then grep. Never re-run cargo just to see results.
```bash
cargo test 2>&1 > /tmp/gait-test-output.txt; echo "exit: $?"
grep "FAILED\|test result\|^error" /tmp/gait-test-output.txt
```

---

### Task 1: Add `Tool::preview()` and implement it on EditTool / WriteTool

Introduce the method and move the diff logic into it. `permission()` delegates to it for now (`preview: self.preview(args)`), so behavior is unchanged and everything stays green. The `PermissionRequest.preview` field still exists at this point.

**Files:**
- Modify: `src/types.rs` (Tool trait — add `preview()` default)
- Modify: `src/tools/edit.rs` (add `preview()`, delegate from `permission()`, add tests)
- Modify: `src/tools/write.rs` (add `preview()`, delegate from `permission()`, add tests)

- [ ] **Step 1: Add the trait method**

In `src/types.rs`, in the `Tool` trait (after the `permission()` default impl), add:

```rust
    /// A rendered preview of what this tool will do (e.g. a diff), shown in the
    /// permission prompt or streamed live per the preview policy. Default: no
    /// preview. Cheap tools return None; edit/write override this.
    fn preview(&self, _args: &serde_json::Value) -> Option<String> { None }
```

- [ ] **Step 2: Write failing `preview()` tests for edit**

In `src/tools/edit.rs` `#[cfg(test)] mod tests`, add (uses existing `make_ctx`, `compute_line_hash`, `TempDir`):

```rust
#[test]
fn preview_returns_diff_for_valid_edit() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("f.rs");
    std::fs::write(&path, "fn foo() {\n    bar();\n}").unwrap();
    let content = std::fs::read_to_string(&path).unwrap();
    let lines: Vec<&str> = content.split('\n').collect();
    let hash = compute_line_hash(2, lines[1]);

    let tool = EditTool::new(make_ctx(&dir));
    let args = serde_json::json!({
        "file_path": "f.rs",
        "edits": [{"op": "replace", "pos": format!("2#{}", hash), "lines": ["    baz();"]}]
    });
    let preview = tool.preview(&args).expect("preview should be Some");
    assert!(preview.contains("-    bar();"), "preview:\n{preview}");
    assert!(preview.contains("+    baz();"), "preview:\n{preview}");
}

#[test]
fn preview_is_none_for_stale_anchor() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("f.rs");
    std::fs::write(&path, "fn foo() {\n    bar();\n}").unwrap();
    let tool = EditTool::new(make_ctx(&dir));
    let args = serde_json::json!({
        "file_path": "f.rs",
        "edits": [{"op": "replace", "pos": "2#ZZ", "lines": ["    baz();"]}]
    });
    assert_eq!(tool.preview(&args), None);
}
```

- [ ] **Step 3: Run to verify they fail** — `cargo test tools::edit::tests::preview 2>&1 > /tmp/t.txt; grep "FAILED\|cannot find\|test result" /tmp/t.txt`. Expected: FAIL (`preview` not found / default returns None).

- [ ] **Step 4: Implement `EditTool::preview()` and delegate from `permission()`**

In `src/tools/edit.rs`, add the `preview()` method inside `impl Tool for EditTool` and change `permission()` to delegate:

```rust
    fn preview(&self, args: &Value) -> Option<String> {
        serde_json::from_value::<HashlineEditParams>(args.clone())
            .ok()
            .and_then(|params| compute_edit(&self.ctx, &params).ok())
            .and_then(|(_, old, new)| crate::tools::preview::diff_preview(&old, &new))
    }

    fn permission(&self, args: &Value) -> ToolPermission {
        let raw = args.get("file_path").and_then(|v| v.as_str()).unwrap_or("*");
        let rel = std::path::Path::new(raw)
            .strip_prefix(&*self.ctx.codebase_path)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| raw.to_string());
        ToolPermission::RequiresApproval(vec![PermissionRequest {
            display: rel.clone(),
            patterns: path_patterns("edit", &rel),
            preview: self.preview(args),
        }])
    }
```

- [ ] **Step 5: Write failing `preview()` tests for write**

In `src/tools/write.rs` `#[cfg(test)] mod tests`, add (uses existing `make_ctx`):

```rust
#[test]
fn preview_is_full_content_for_new_file() {
    let dir = tempfile::TempDir::new().unwrap();
    let tool = WriteTool::new(make_ctx(&dir));
    let args = serde_json::json!({"file_path": "new.txt", "content": "hello\nworld\n"});
    assert_eq!(tool.preview(&args).as_deref(), Some("hello\nworld\n"));
}

#[test]
fn preview_is_diff_for_existing_file() {
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::write(dir.path().join("f.txt"), "one\ntwo\n").unwrap();
    let tool = WriteTool::new(make_ctx(&dir));
    let args = serde_json::json!({"file_path": "f.txt", "content": "one\nTWO\n"});
    let preview = tool.preview(&args).expect("preview Some");
    assert!(preview.contains("-two"), "preview:\n{preview}");
    assert!(preview.contains("+TWO"), "preview:\n{preview}");
}
```

- [ ] **Step 6: Implement `WriteTool::preview()` and delegate from `permission()`**

In `src/tools/write.rs`, add inside `impl Tool for WriteTool` and change `permission()` to delegate:

```rust
    fn preview(&self, args: &serde_json::Value) -> Option<String> {
        serde_json::from_value::<WriteParams>(args.clone()).ok().and_then(|params| {
            let resolved = if std::path::Path::new(&params.file_path).is_absolute() {
                PathBuf::from(&params.file_path)
            } else {
                self.ctx.codebase_path.join(&params.file_path)
            };
            if resolved.exists() {
                std::fs::read_to_string(&resolved)
                    .ok()
                    .and_then(|old| crate::tools::preview::diff_preview(&old, &params.content))
            } else {
                Some(params.content.clone())
            }
        })
    }

    fn permission(&self, args: &serde_json::Value) -> ToolPermission {
        let raw = args.get("file_path").and_then(|v| v.as_str()).unwrap_or("*");
        let rel = std::path::Path::new(raw)
            .strip_prefix(&*self.ctx.codebase_path)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| raw.to_string());
        ToolPermission::RequiresApproval(vec![PermissionRequest {
            display: rel.clone(),
            patterns: path_patterns("write", &rel),
            preview: self.preview(args),
        }])
    }
```

- [ ] **Step 7: Run the tool tests + full build** — `cargo test tools::edit tools::write 2>&1 > /tmp/t.txt; grep "FAILED\|test result" /tmp/t.txt`. Expected: all green (new `preview()` tests pass; the old `permission_*preview*` tests still pass because `permission()` now delegates to `preview()`).

- [ ] **Step 8: Commit**

```bash
git add src/types.rs src/tools/edit.rs src/tools/write.rs
git commit -m "feat: add Tool::preview() and implement it on edit/write (permission delegates)"
```

---

### Task 2: Remove `preview` from `PermissionRequest`; flip the wiring

`permission()` returns gating identity only. `turn.rs` Tier 3 now calls `tool.preview()` directly. This is where the auto-mode waste is fixed: `permission()` (called for every tool call) no longer computes a diff; `preview()` runs only at the Tier-3 ask path.

**Files:**
- Modify: `src/types.rs` (`PermissionRequest` struct + default `permission()` impl + `ToolDef` impl + test)
- Modify: `src/tools/edit.rs`, `src/tools/write.rs` (`permission()` drops the `preview:` line; delete the now-obsolete `permission_*preview*` tests)
- Modify: `src/tools/read.rs`, `src/tools/shell.rs`, `src/tools/load.rs` (`permission()` sites drop `preview: None`)
- Modify: `src/permission.rs` (~14 construction sites drop `preview: None`)
- Modify: `src/turn.rs` (gating loop: bind `tool`, Tier 3 uses `tool.preview()`; two fallback `PermissionRequest` literals drop `preview: None`)

- [ ] **Step 1: Remove the struct field**

In `src/types.rs`, change `PermissionRequest` back to two fields:

```rust
#[derive(Debug, Clone, PartialEq)]
pub struct PermissionRequest {
    pub display: String,
    pub patterns: Vec<String>,
}
```

- [ ] **Step 2: Update edit/write `permission()` and delete obsolete tests**

In `src/tools/edit.rs`, the `permission()` from Task 1 drops its `preview:` line:

```rust
    fn permission(&self, args: &Value) -> ToolPermission {
        let raw = args.get("file_path").and_then(|v| v.as_str()).unwrap_or("*");
        let rel = std::path::Path::new(raw)
            .strip_prefix(&*self.ctx.codebase_path)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| raw.to_string());
        ToolPermission::RequiresApproval(vec![PermissionRequest {
            display: rel.clone(),
            patterns: path_patterns("edit", &rel),
        }])
    }
```

Delete the now-obsolete tests `permission_includes_diff_preview_for_valid_edit` and `permission_preview_is_none_for_stale_anchor` from `src/tools/edit.rs` (the `preview()` tests added in Task 1 replace them). Apply the identical `preview:`-line removal to `WriteTool::permission()` in `src/tools/write.rs`, and delete its obsolete tests `permission_preview_is_full_content_for_new_file` and `permission_preview_is_diff_for_existing_file`.

- [ ] **Step 3: Build to surface every remaining construction site**

```bash
cargo build 2>&1 > /tmp/b.txt; echo "exit: $?"
grep "struct .PermissionRequest. has no field .preview.\|missing field\|no field .preview.\|^error" /tmp/b.txt | head -40
```
Remove the `preview: None,` line from every reported `PermissionRequest { .. }` literal. Known sites: `src/types.rs` (default `permission()` impl, `ToolDef` impl, a test), `src/tools/read.rs`, `src/tools/shell.rs`, `src/tools/load.rs`, and `src/permission.rs` (~14 sites incl. tests). Trust the compiler — iterate build→fix.

- [ ] **Step 4: Fix the turn.rs gating loop**

In `src/turn.rs`, the gating loop currently (around `:859`) does `let perm = tools.iter().find(|t| t.name() == tc.tool_name).map(|t| t.permission(&tc.args)).unwrap_or_else(...)`. Bind the tool once so it's available for both `permission()` and `preview()`:

```rust
        for tc in &step_tool_calls {
            let tool = tools.iter().find(|t| t.name() == tc.tool_name);
            let perm = tool
                .map(|t| t.permission(&tc.args))
                .unwrap_or_else(|| ToolPermission::RequiresApproval(vec![
                    crate::types::PermissionRequest {
                        display: tc.tool_name.clone(),
                        patterns: vec![tc.tool_name.clone()],
                    }
                ]));
```

Then in the Tier-3 ask block, change the `PermissionItem` construction's `preview` line from `preview: req.preview.clone(),` to:

```rust
                        preview: tool.and_then(|t| t.preview(&tc.args)),
```

(The other fallback `PermissionRequest` literal in the same file — there are two, around the original lines 864 and 940 — also drop their `preview: None` lines per Step 3.)

- [ ] **Step 5: Build + full test suite**

```bash
cargo build 2>&1 > /tmp/b.txt; echo "build $?"; grep "^error\|no field" /tmp/b.txt | head
cargo test 2>&1 > /tmp/t.txt; echo "test $?"; grep "FAILED\|test result" /tmp/t.txt | tail
```
Expected: build exit 0; all tests green. The `PermissionItem` proto round-trip and `perm_to_tx` tests (which test `PermissionItem.preview`, untouched here) still pass.

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "refactor: remove PermissionRequest.preview; turn.rs Tier-3 calls Tool::preview()"
```

---

### Task 3: `PreviewPolicy` enum + `gait.jsonc` config field

**Files:**
- Modify: `src/config.rs` (enum + `GaitConfig.preview` field + `Default` + parse test)

- [ ] **Step 1: Write the failing parse test**

In `src/config.rs` `#[cfg(test)] mod tests` (add the block if absent), add:

```rust
#[test]
fn preview_policy_parses_from_jsonc() {
    let cfg: GaitConfig = serde_json::from_str(r#"{"model":"m","preview":"always"}"#).unwrap();
    assert_eq!(cfg.preview, PreviewPolicy::Always);
    let cfg: GaitConfig = serde_json::from_str(r#"{"model":"m","preview":"off"}"#).unwrap();
    assert_eq!(cfg.preview, PreviewPolicy::Off);
    // absent → default on-ask
    let cfg: GaitConfig = serde_json::from_str(r#"{"model":"m"}"#).unwrap();
    assert_eq!(cfg.preview, PreviewPolicy::OnAsk);
}
```

- [ ] **Step 2: Run to verify it fails** — `cargo test config::tests::preview_policy 2>&1 > /tmp/t.txt; grep "cannot find\|test result\|FAILED" /tmp/t.txt`. Expected: FAIL (`PreviewPolicy` not found).

- [ ] **Step 3: Add the enum and field**

In `src/config.rs`, add the enum (near the top-level config types):

```rust
/// When to render edit/write previews. `on-ask` (default): only at the permission
/// prompt. `always`: also stream the diff live (ephemeral) for auto/allow-listed
/// tools. `off`: never.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum PreviewPolicy {
    #[default]
    OnAsk,
    Always,
    Off,
}
```

Add the field to `GaitConfig` (after `max_iter`):

```rust
    /// When to render edit/write previews. Defaults to on-ask.
    #[serde(default)]
    pub preview: PreviewPolicy,
```

And add `preview: PreviewPolicy::OnAsk,` to the `GaitConfig::default()` literal.

- [ ] **Step 4: Run to verify it passes** — `cargo test config:: 2>&1 > /tmp/t.txt; grep "FAILED\|test result" /tmp/t.txt`. Expected: green.

- [ ] **Step 5: Commit**

```bash
git add src/config.rs
git commit -m "feat: add PreviewPolicy enum + gait.jsonc preview field (default on-ask)"
```

---

### Task 4: Thread the policy into the turn; gate the prompt preview on `off`

**Files:**
- Modify: `src/turn.rs` (`TurnInput` field; extract in `run_turn`; Tier-3 `off` gating)
- Modify: `src/bin/gait.rs` (populate `TurnInput.preview_policy` from config)
- Modify: any other `TurnInput { .. }` construction sites (tests)

- [ ] **Step 1: Add the field to `TurnInput`**

In `src/turn.rs`, `TurnInput` (around `:49`):

```rust
pub struct TurnInput {
    pub task: Option<String>,
    pub auto: bool,
    pub max_iter: Option<usize>,
    pub preview_policy: crate::config::PreviewPolicy,
}
```

- [ ] **Step 2: Extract it in `run_turn`**

In `run_turn` (after `let max_iter = input.max_iter;` around `:481`):

```rust
    let preview_policy = input.preview_policy;
```

- [ ] **Step 3: Gate the prompt preview on `off`**

In the Tier-3 block (the `PermissionItem` construction from Task 2), change the preview line to:

```rust
                        preview: if preview_policy == crate::config::PreviewPolicy::Off {
                            None
                        } else {
                            tool.and_then(|t| t.preview(&tc.args))
                        },
```

- [ ] **Step 4: Populate from config in `bin/gait.rs`**

Find every `TurnInput { .. }` construction (`grep -rn "TurnInput {" src/`). In `src/bin/gait.rs`, where `max_iter` is sourced from the loaded `GaitConfig`, add `preview_policy: config.preview,` (use the actual config binding name in scope). For any `TurnInput` literal in tests or elsewhere that has no config, add `preview_policy: gait_ai::config::PreviewPolicy::default(),` (or the crate-local path `crate::config::PreviewPolicy::default()`).

- [ ] **Step 5: Build + full suite**

```bash
cargo build 2>&1 > /tmp/b.txt; echo "build $?"; grep "^error\|missing field" /tmp/b.txt | head
cargo test 2>&1 > /tmp/t.txt; echo "test $?"; grep "FAILED\|test result" /tmp/t.txt | tail
```
Expected: build 0; all green. (Add `preview_policy` to any `TurnInput` literal the compiler flags.)

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "feat: thread preview_policy through TurnInput; gate prompt preview on off"
```

---

### Task 5: `always` mode — stream the diff live (ephemeral) for auto/allow-listed tools

**Files:**
- Modify: `src/turn.rs` (helper `stream_preview_if_always` + call it in the Tier-2 allowed branch + unit tests)

- [ ] **Step 1: Write the failing helper tests**

In `src/turn.rs` `#[cfg(test)] mod tests`, add (mirrors `src/proto/emitter.rs` test style — build an `Emitter` from an mpsc channel and inspect the receiver):

```rust
#[tokio::test]
async fn always_policy_streams_edit_preview_delta() {
    use crate::config::PreviewPolicy;
    use crate::proto::gait::log::v1::stream_item::Item;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f.rs");
    std::fs::write(&path, "fn foo() {\n    bar();\n}").unwrap();
    let content = std::fs::read_to_string(&path).unwrap();
    let lines: Vec<&str> = content.split('\n').collect();
    let hash = crate::hashline::compute_line_hash(2, lines[1]);

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let emitter = crate::proto::Emitter::new(dir.path().join("log.jsonl"), tx);
    let ctx = crate::tools::ctx::ToolCtx::new(dir.path().to_path_buf(), dir.path().to_path_buf(), "t".into());
    let tools: Vec<Box<dyn Tool>> = vec![Box::new(crate::tools::EditTool::new(ctx))];
    let args = serde_json::json!({
        "file_path": "f.rs",
        "edits": [{"op": "replace", "pos": format!("2#{}", hash), "lines": ["    baz();"]}]
    });

    stream_preview_if_always(PreviewPolicy::Always, tools.iter().find(|t| t.name() == "edit"), &args, &emitter);

    // No transcript write (ephemeral).
    assert!(!dir.path().join("log.jsonl").exists());
    // A Delta carrying the diff was streamed.
    let item = rx.try_recv().expect("a delta").item.unwrap();
    let Item::Delta(d) = item else { panic!("expected Delta") };
    assert!(d.content.contains("-    bar();") && d.content.contains("+    baz();"), "delta: {}", d.content);
}

#[tokio::test]
async fn on_ask_policy_streams_nothing() {
    use crate::config::PreviewPolicy;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("f.rs"), "fn foo() {}").unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let emitter = crate::proto::Emitter::new(dir.path().join("log.jsonl"), tx);
    let ctx = crate::tools::ctx::ToolCtx::new(dir.path().to_path_buf(), dir.path().to_path_buf(), "t".into());
    let tools: Vec<Box<dyn Tool>> = vec![Box::new(crate::tools::EditTool::new(ctx))];
    let args = serde_json::json!({"file_path": "f.rs", "edits": []});

    stream_preview_if_always(PreviewPolicy::OnAsk, tools.iter().find(|t| t.name() == "edit"), &args, &emitter);
    assert!(rx.try_recv().is_err(), "on-ask must stream nothing");
}
```

(Confirm the in-scope paths for `Tool`, `EditTool`, `ToolCtx`, `Emitter`, `compute_line_hash` against the file's existing imports; adjust the qualified paths if the test module already imports them.)

- [ ] **Step 2: Run to verify they fail** — `cargo test turn::tests::on_ask_policy turn::tests::always_policy 2>&1 > /tmp/t.txt; grep "cannot find\|FAILED\|test result" /tmp/t.txt`. Expected: FAIL (`stream_preview_if_always` not found).

- [ ] **Step 3: Implement the helper**

In `src/turn.rs` (module level, near `execute_tool_call`):

```rust
/// In `always` mode, stream a tool's preview (e.g. a diff) live on the ephemeral
/// `delta` channel — never written to the log. No-op for other policies or when
/// the tool has no preview.
fn stream_preview_if_always(
    policy: crate::config::PreviewPolicy,
    tool: Option<&Box<dyn Tool>>,
    args: &serde_json::Value,
    emitter: &Emitter,
) {
    if policy != crate::config::PreviewPolicy::Always {
        return;
    }
    if let Some(p) = tool.and_then(|t| t.preview(args)) {
        emitter.delta(format!("\n{}\n", p));
    }
}
```

- [ ] **Step 4: Call it from the Tier-2 allowed branch**

In the gating loop, inside the `if is_allowed { ... }` block (before the `continue`), add a call. `tool` is already bound (Task 2 Step 4):

```rust
            if is_allowed {
                stream_preview_if_always(preview_policy, tool, &tc.args, emitter);
                // ... existing prefix logic ...
                continue;
            }
```

- [ ] **Step 5: Run the helper tests + full suite**

```bash
cargo test turn:: 2>&1 > /tmp/t.txt; echo "exit $?"; grep "FAILED\|test result" /tmp/t.txt | tail
cargo build 2>&1 > /tmp/b.txt; grep "^error" /tmp/b.txt | head
```
Expected: green; build 0.

- [ ] **Step 6: Commit**

```bash
git add src/turn.rs
git commit -m "feat: always mode streams edit/write diff live (ephemeral delta)"
```

---

### Task 6: Docs + mark spec/plan done

**Files:**
- Modify: `CLAUDE.md`, `docs/architecture.md` (boundary rule), spec + plan frontmatter

- [ ] **Step 1: Update the boundary rule**

In `CLAUDE.md` (the "Tools own preview content" bullet) and the corresponding rule in `docs/architecture.md`, replace the old "set `PermissionRequest.preview` in `permission()`" wording with: preview content is produced by `Tool::preview()`; `turn.rs` decides *when* to call it per the `preview` policy (the ask prompt, or an ephemeral `delta` in `always` mode); the UI only renders. Note `permission()` is gating-identity only.

- [ ] **Step 2: Mark spec + plan done**

Prepend `---\nstatus: done\n---\n` to `docs/specs/2026-06-13-preview-policy-split.md` and `docs/plans/2026-06-13-preview-policy-split.md`.

- [ ] **Step 3: Commit**

```bash
git add CLAUDE.md docs/architecture.md docs/specs/2026-06-13-preview-policy-split.md docs/plans/2026-06-13-preview-policy-split.md 2>/dev/null; git add -A
git commit -m "docs: update preview boundary rule for Tool::preview() split; mark spec/plan done"
```
(`CLAUDE.md` is gitignored — if `git add` reports it's ignored, commit the rest; the local edit is fine.)

---

## Self-Review

**Spec coverage:**
- §1 Tool trait split (`preview()` added, `permission()` gating-only) — Tasks 1, 2. ✅
- §2 remove `preview` from `PermissionRequest`, keep `PermissionItem.preview` — Task 2 (proto/transcript untouched). ✅
- §3 edit/write move logic to `preview()` — Task 1. ✅
- §4 `PreviewPolicy` enum + `GaitConfig.preview` — Task 3. ✅
- §5 thread policy via `TurnInput` → `bin/gait.rs` → `run_turn` — Task 4. ✅
- §6 Tier-3 `off` gating (Task 4) + Tier-2 `always` ephemeral delta (Task 5). ✅
- §7 no proto/`execute_tool_call` change — honored (nothing in any task touches them). ✅
- §8 docs boundary rule — Task 6. ✅
- Testing: `preview()` tests (Task 1), config parse (Task 3), `always`/`on-ask` delta + no-log-write (Task 5), existing `PermissionItem` round-trip stays green (Task 2). ✅

**Type consistency:** `Tool::preview(&self, &Value) -> Option<String>` defined Task 1, called in turn.rs Tasks 2/4/5 and the helper. `PreviewPolicy` defined Task 3, used in `TurnInput`/`run_turn`/helper Tasks 4/5. `stream_preview_if_always(policy, Option<&Box<dyn Tool>>, &Value, &Emitter)` defined and called in Task 5. `tool` binding introduced in Task 2 Step 4 is reused by Tasks 4/5. ✅

**Placeholder scan:** every code step has complete code. Steps that say "confirm in-scope paths / find all `TurnInput {` sites" are verification instructions against the compiler, not placeholders. ✅

**Ordering:** each task compiles and is green on its own — Task 1 keeps `permission()` working via delegation; Task 2 flips the wiring atomically (field removal + all consumers in one commit); Tasks 3-5 layer the policy without breaking earlier behavior. The auto-mode-waste fix lands in Task 2 (preview computed only at Tier 3), independent of the policy work. ✅

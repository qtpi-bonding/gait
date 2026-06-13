# Permission Diff Preview Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Show the user a `-`/`+` diff (edit) or full-content/diff (write) of what a tool is about to change, in the permission prompt, before they approve it.

**Architecture:** A single optional field `preview: Option<String>` is computed by the tool's `permission()` method, threaded along the existing `PermissionRequest → PermissionItem (Rust) → proto PermissionItem → log → proto PermissionItem → PermissionItem (Rust) → UI` path, and rendered as a block above the allow-list patterns. The tool layer owns the content (per `docs/architecture.md`); the UI renders the string blind to format. The preview is persisted in the log so a resumed/headless session re-renders without the UI doing file I/O.

**Tech Stack:** Rust, `similar` crate (line diffing), prost + pbjson (proto), serde.

---

## Spec

Design doc: `docs/specs/2026-06-13-permission-diff-preview-design.md`. Read it before starting.

## Corrections to the design doc (discovered during planning — follow THIS plan)

The design doc's "UI render" section names `src/ui/permission.rs::prompt_permission`. That function is a dead/legacy duplicate. The **live** permission prompt is `src/ui/builder.rs::run_permission`. Render there.

The design doc says the `events::permission_item` converter copies `preview` "both directions." In fact there are **two** converters:
- Rust→proto: `src/proto/events.rs::permission_item` (line 361)
- proto→Rust: `src/ui/state.rs::perm_to_tx` (line 30)

Both must copy `preview`.

## File Structure

| File | Responsibility | Change |
|------|----------------|--------|
| `src/tools/preview.rs` | Pure diff-rendering helper | **Create** |
| `src/tools/mod.rs` | Module registration | Add `pub mod preview;` |
| `Cargo.toml` | Dependencies | Add `similar` |
| `src/types.rs` | `PermissionRequest` struct | Add `preview` field |
| `src/session/transcript.rs` | Rust `PermissionItem` struct | Add `preview` field |
| `proto/log.proto` | proto `PermissionItem` message | Add `optional string preview = 7;` |
| `src/proto/events.rs` | Rust→proto converter | Copy `preview` |
| `src/ui/state.rs` | proto→Rust converter | Copy `preview` |
| `src/turn.rs` | Live `PermissionRequest`→`PermissionItem` mapping | Map `preview` |
| `src/tools/edit.rs` | `EditTool` — factor `compute_edit`, populate preview | Modify |
| `src/tools/write.rs` | `WriteTool` — populate preview | Modify |
| `src/ui/permission.rs` | Pure `preview_block` formatter | Add fn + test |
| `src/ui/builder.rs` | Live prompt render | Print preview block |

**Note on the proto crate path:** `proto/log.proto` lives at `/Users/aicoder/Documents/ideation/gait/proto/log.proto` (repo root, one level above `gait-rs/`). It is compiled by `gait-rs/build.rs` (prost + pbjson) on every `cargo build`. Editing the `.proto` and rebuilding regenerates the Rust `pb::PermissionItem` type automatically — no manual codegen step.

**Test running (from `CLAUDE.md`):** always save output to a file, then grep it. Never re-run `cargo test` just to see results.
```bash
cargo test 2>&1 > /tmp/gait-test-output.txt; echo "exit: $?"
grep "FAILED\|test result\|^error" /tmp/gait-test-output.txt
```

---

### Task 1: `diff_preview` helper + `similar` dependency

Pure, self-contained line-diff renderer. No other code depends on the field yet, so this task stands alone.

**Files:**
- Modify: `Cargo.toml` (add `similar` to `[dependencies]`)
- Create: `src/tools/preview.rs`
- Modify: `src/tools/mod.rs` (register module)

- [ ] **Step 1: Add the `similar` dependency**

In `Cargo.toml`, in the `[dependencies]` section, add this line (place it near the other simple deps, e.g. after `regex = "1"`):

```toml
similar = "2"
```

- [ ] **Step 2: Register the module**

In `src/tools/mod.rs`, add a module declaration. After the line `pub mod annotate;` (currently line 20) add:

```rust
pub mod preview;
```

- [ ] **Step 3: Write the failing tests**

Create `src/tools/preview.rs` with ONLY the tests first (the function will not exist yet, so it won't compile — that's the failing state):

```rust
//! Pure rendering of a tool's pending change for the permission prompt.
//! The tool layer owns this content; the UI renders the returned string blind to format.

use similar::{ChangeTag, TextDiff};

/// Unified `-`/`+` diff of `old`→`new` with 2 lines of context.
/// Returns `None` when the inputs are identical (no change to preview).
/// No truncation — the full diff is returned. No `@@` hunk headers.
pub fn diff_preview(old: &str, new: &str) -> Option<String> {
    unimplemented!()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_line_change_shows_minus_plus_and_context() {
        let old = "alpha\nbravo\ncharlie\n";
        let new = "alpha\nBRAVO\ncharlie\n";
        let out = diff_preview(old, new).expect("changed input → Some");
        assert!(out.contains("-bravo"), "missing deleted line:\n{out}");
        assert!(out.contains("+BRAVO"), "missing inserted line:\n{out}");
        assert!(out.contains(" alpha"), "missing context line:\n{out}");
        // No unified-diff hunk headers.
        assert!(!out.contains("@@"), "should not contain @@ headers:\n{out}");
    }

    #[test]
    fn identical_input_is_none() {
        assert_eq!(diff_preview("same\ntext\n", "same\ntext\n"), None);
    }

    #[test]
    fn multi_hunk_change_shows_both_hunks() {
        let old = "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\n";
        // change the first and last lines — two separated hunks
        let new = "A\nb\nc\nd\ne\nf\ng\nh\ni\nJ\n";
        let out = diff_preview(old, new).expect("changed input → Some");
        assert!(out.contains("-a") && out.contains("+A"), "first hunk missing:\n{out}");
        assert!(out.contains("-j") && out.contains("+J"), "second hunk missing:\n{out}");
    }
}
```

- [ ] **Step 4: Run tests to verify they fail to compile**

Run:
```bash
cargo test -p gait_ai tools::preview 2>&1 > /tmp/gait-test-output.txt; echo "exit: $?"
grep "test result\|^error\|unimplemented" /tmp/gait-test-output.txt
```
Expected: compile error or `unimplemented` panic (function body is `unimplemented!()`).

- [ ] **Step 5: Implement `diff_preview`**

Replace the `unimplemented!()` body in `src/tools/preview.rs`:

```rust
pub fn diff_preview(old: &str, new: &str) -> Option<String> {
    if old == new {
        return None;
    }
    let diff = TextDiff::from_lines(old, new);
    let mut out = String::new();
    let mut changed = false;
    // grouped_ops(2) → 2 lines of surrounding context per hunk, no @@ headers.
    for group in diff.grouped_ops(2) {
        for op in group.iter() {
            for change in diff.iter_changes(op) {
                let sign = match change.tag() {
                    ChangeTag::Delete => '-',
                    ChangeTag::Insert => '+',
                    ChangeTag::Equal => ' ',
                };
                if change.tag() != ChangeTag::Equal {
                    changed = true;
                }
                out.push(sign);
                out.push_str(change.value()); // value() includes the line's trailing '\n'
            }
        }
    }
    if changed {
        Some(out.trim_end_matches('\n').to_string())
    } else {
        None
    }
}
```

- [ ] **Step 6: Run tests to verify they pass**

Run:
```bash
cargo test -p gait_ai tools::preview 2>&1 > /tmp/gait-test-output.txt; echo "exit: $?"
grep "test result\|^error" /tmp/gait-test-output.txt
```
Expected: `test result: ok. 3 passed`.

- [ ] **Step 7: Commit**

```bash
git add Cargo.toml Cargo.lock src/tools/preview.rs src/tools/mod.rs
git commit -m "feat: add diff_preview helper for permission previews"
```

---

### Task 2: Add `preview` to `PermissionRequest` (compile-only field)

Add the field and fix every construction site so the crate compiles. Tool sites get `None` here; they are populated in Tasks 4–5.

**Files:**
- Modify: `src/types.rs:122-128` (struct + default `permission()` impl at ~205)
- Modify: `src/permission.rs` (sites at 92, 158, 412, 419, 427, 435, 444, 450, 458, 466, 474, 485, 494, 505)
- Modify: `src/turn.rs:864`
- Modify: `src/tools/write.rs:60`, `src/tools/shell.rs:71`, `src/tools/load.rs:43`, `src/tools/edit.rs:157`, `src/tools/read.rs:177`
- Modify: `src/types.rs:248` (ToolDef impl), `src/types.rs:412` (test)

- [ ] **Step 1: Add the field**

In `src/types.rs`, change the `PermissionRequest` struct (lines 122–128) to:

```rust
/// One thing a tool needs approved. `display` is shown to the user and matched
/// against the allow list. `patterns` are the full allow-list entries the user
/// can choose to save, from broadest to most specific. `preview` is an optional
/// rendered view of the pending change (e.g. a diff), owned by the tool's
/// `permission()` method and shown above the patterns.
#[derive(Debug, Clone, PartialEq)]
pub struct PermissionRequest {
    pub display: String,
    pub patterns: Vec<String>,
    pub preview: Option<String>,
}
```

- [ ] **Step 2: Build to surface every broken construction site**

Run:
```bash
cargo build 2>&1 > /tmp/gait-build-output.txt; echo "exit: $?"
grep "missing field .preview\|^error" /tmp/gait-build-output.txt | head -40
```
Expected: a list of `missing field `preview`` errors, one per construction site (~25). Use this list as the worklist for Step 3.

- [ ] **Step 3: Add `preview: None` to every `PermissionRequest { .. }` literal**

For each construction site reported, add `preview: None,` as the last field. The known sites are:

- `src/types.rs` — the default `permission()` impl (~line 205) and the `ToolDef` impl (~line 248) and the test (~line 412)
- `src/turn.rs:864` (the fallback when a tool isn't found)
- `src/permission.rs` — lines 92, 158, and the test block (412, 419, 427, 435, 444, 450, 458, 466, 474, 485, 494, 505)
- `src/tools/write.rs:60`, `src/tools/shell.rs:71`, `src/tools/load.rs:43`, `src/tools/edit.rs:157`, `src/tools/read.rs:177`

Example — the default impl in `src/types.rs`:

```rust
fn permission(&self, _args: &serde_json::Value) -> ToolPermission {
    ToolPermission::RequiresApproval(vec![PermissionRequest {
        display: self.name().to_string(),
        patterns: vec![self.name().to_string()],
        preview: None,
    }])
}
```

Example — `src/tools/edit.rs:151-162` (set `None` for now; Task 4 replaces this):

```rust
ToolPermission::RequiresApproval(vec![PermissionRequest {
    display: rel.clone(),
    patterns: path_patterns("edit", &rel),
    preview: None,
}])
```

- [ ] **Step 4: Build clean**

Run:
```bash
cargo build 2>&1 > /tmp/gait-build-output.txt; echo "exit: $?"
grep "missing field\|^error" /tmp/gait-build-output.txt | head -40
```
Expected: exit 0, no `missing field` errors.

- [ ] **Step 5: Run the test suite to confirm nothing regressed**

Run:
```bash
cargo test 2>&1 > /tmp/gait-test-output.txt; echo "exit: $?"
grep "FAILED\|test result\|^error" /tmp/gait-test-output.txt
```
Expected: all green (no `FAILED`).

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "feat: add preview field to PermissionRequest (None at all sites)"
```

---

### Task 3: Thread `preview` through `PermissionItem` (Rust + proto + both converters + log)

Make `preview` survive the full persistence round-trip: Rust `PermissionItem` → proto → JSON log → proto → Rust `PermissionItem`.

**Files:**
- Modify: `proto/log.proto` (the `PermissionItem` message — repo-root proto, `/Users/aicoder/Documents/ideation/gait/proto/log.proto:259-266`)
- Modify: `src/session/transcript.rs:86-96` (Rust struct)
- Modify: `src/proto/events.rs:361` (Rust→proto)
- Modify: `src/ui/state.rs:30` (proto→Rust)
- Modify: `src/turn.rs:912` (map `req.preview` → item), and the other `PermissionItem { .. }` sites: `src/turn.rs:1857`, `1918`, `2007`; `src/session/retrace.rs:804`; tests at `src/session/transcript.rs:410,425` and `src/ui/state.rs:307`
- Test: `src/session/transcript.rs` (proto round-trip test) and `src/ui/state.rs` (perm_to_tx test)

- [ ] **Step 1: Add the proto field**

In `proto/log.proto`, change the `PermissionItem` message (lines 259–266) to add field 7:

```protobuf
message PermissionItem {
  string id        = 1;
  string cmd       = 2;
  string reason    = 3;
  repeated string patterns = 4;        // allow-list entries, broad → specific
  string tool_name = 5;
  google.protobuf.Struct tool_args = 6;
  optional string preview = 7;         // rendered pending change (diff/content), tool-owned
}
```

- [ ] **Step 2: Add the Rust struct field**

In `src/session/transcript.rs`, change the `PermissionItem` struct (lines 86–96) to add `preview`, matching the existing serde style for optional/defaulted fields:

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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
}
```

- [ ] **Step 3: Copy `preview` in the Rust→proto converter**

In `src/proto/events.rs`, change `permission_item` (lines 361–370) to:

```rust
fn permission_item(p: &tx::PermissionItem) -> pb::PermissionItem {
    pb::PermissionItem {
        id: p.id.clone(),
        cmd: p.cmd.clone(),
        reason: p.reason.clone(),
        patterns: p.patterns.clone(),
        tool_name: p.tool_name.clone(),
        tool_args: to_struct(&p.tool_args),
        preview: p.preview.clone(),
    }
}
```

- [ ] **Step 4: Copy `preview` in the proto→Rust converter**

In `src/ui/state.rs`, change `perm_to_tx` (lines 30–41) to:

```rust
pub(crate) fn perm_to_tx(p: &pb::PermissionItem) -> tx::PermissionItem {
    tx::PermissionItem {
        id: p.id.clone(),
        cmd: p.cmd.clone(),
        reason: p.reason.clone(),
        patterns: p.patterns.clone(),
        tool_name: p.tool_name.clone(),
        tool_args: p.tool_args.clone()
            .and_then(|s| serde_json::to_value(s).ok())
            .unwrap_or(serde_json::Value::Null),
        preview: p.preview.clone(),
    }
}
```

- [ ] **Step 5: Map `req.preview` at the live construction site**

In `src/turn.rs`, change the `PermissionItem` built in the gating loop (lines 912–919) to carry the preview from the `PermissionRequest`:

```rust
pending_permissions.push(PermissionItem {
    id: tc.tool_call_id.clone(),
    cmd: req.display.clone(),
    reason: format!("{} requires permission", tc.tool_name),
    patterns: req.patterns.clone(),
    tool_name: tc.tool_name.clone(),
    tool_args: tc.args.clone(),
    preview: req.preview.clone(),
});
```

- [ ] **Step 6: Fix the remaining `PermissionItem { .. }` sites with `preview: None`**

Build to find them:
```bash
cargo build 2>&1 > /tmp/gait-build-output.txt; echo "exit: $?"
grep "missing field .preview\|^error" /tmp/gait-build-output.txt | head -20
```
Add `preview: None,` to each reported site. Known sites: `src/turn.rs:1857`, `src/turn.rs:1918`, `src/turn.rs:2007`, `src/session/retrace.rs:804`, and the tests at `src/session/transcript.rs:410`, `src/session/transcript.rs:425`, `src/ui/state.rs:307`.

- [ ] **Step 7: Write the proto round-trip test**

In `src/session/transcript.rs`, in the `#[cfg(test)] mod tests` block, add:

```rust
#[test]
fn permission_item_preview_survives_proto_round_trip() {
    use crate::proto::{events, wire};
    let item = PermissionItem {
        id: "tc1".into(),
        cmd: "src/main.rs".into(),
        reason: "edit requires permission".into(),
        patterns: vec!["edit:src/main.rs".into()],
        tool_name: "edit".into(),
        tool_args: serde_json::json!({"file_path": "src/main.rs"}),
        preview: Some("-old\n+new".into()),
    };
    // Rust → proto Event body → JSON line → proto Event → items
    let body = events::permissions(std::slice::from_ref(&item));
    let line = wire::encode_body_to_line(&body);
    let ev: crate::proto::gait::log::v1::Event = serde_json::from_str(&line).unwrap();
    let kind = crate::proto::wire::event_kind(&ev).unwrap();
    let crate::proto::gait::log::v1::payload::Kind::Permissions(p) = kind else {
        panic!("expected Permissions");
    };
    assert_eq!(p.items[0].preview.as_deref(), Some("-old\n+new"));
}
```

NOTE: the exact helper to turn a `body` into a JSON log line and back may differ. Before writing this test, grep `src/proto/wire.rs` for the existing encode/append + `event_kind` helpers and an existing round-trip test (e.g. `read_events_parses_modern_log_without_rewrite`) and mirror its exact calls. The assertion that matters is: `p.items[0].preview == Some("-old\n+new")` after a serialize→deserialize cycle.

- [ ] **Step 8: Write the `perm_to_tx` preview test**

In `src/ui/state.rs`, in the `#[cfg(test)] mod tests` block, add:

```rust
#[test]
fn perm_to_tx_carries_preview() {
    let pb_item = pb::PermissionItem {
        id: "x".into(),
        cmd: "f.rs".into(),
        reason: "r".into(),
        patterns: vec![],
        tool_name: "edit".into(),
        tool_args: None,
        preview: Some("-a\n+b".into()),
    };
    let tx = perm_to_tx(&pb_item);
    assert_eq!(tx.preview.as_deref(), Some("-a\n+b"));
}
```

(Confirm the `pb` alias is in scope in the test module; the file already uses `pb::PermissionItem` at line 30, so `use` it the same way the existing tests do.)

- [ ] **Step 9: Run tests**

Run:
```bash
cargo test 2>&1 > /tmp/gait-test-output.txt; echo "exit: $?"
grep "FAILED\|test result\|^error" /tmp/gait-test-output.txt
```
Expected: all green, including `permission_item_preview_survives_proto_round_trip` and `perm_to_tx_carries_preview`.

- [ ] **Step 10: Commit**

```bash
git add -A
git commit -m "feat: persist permission preview through PermissionItem + proto round-trip"
```

---

### Task 4: Populate `preview` in `EditTool::permission()`

Factor the read+apply step into a shared helper so `permission()` and `execute()` compute the new content identically, then render the diff in `permission()`.

**Files:**
- Modify: `src/tools/edit.rs` (add `compute_edit`, refactor `execute`, populate `permission`)
- Test: `src/tools/edit.rs` `#[cfg(test)]`

- [ ] **Step 1: Add the shared `compute_edit` helper**

In `src/tools/edit.rs`, add a free function (near `HashlineEditParams`). It resolves the path, reads, and applies the edits — the exact logic currently inline in `execute()`:

```rust
/// Resolve `file_path`, read the file, and apply the hashline edits.
/// Returns `(resolved_path, old_content, new_content)`.
/// Errors on a missing file or a stale/invalid anchor (apply failure).
fn compute_edit(
    ctx: &crate::tools::ctx::ToolCtx,
    params: &HashlineEditParams,
) -> anyhow::Result<(std::path::PathBuf, String, String)> {
    let resolved = if std::path::Path::new(&params.file_path).is_absolute() {
        std::path::PathBuf::from(&params.file_path)
    } else {
        ctx.codebase_path.join(&params.file_path)
    };
    if !resolved.exists() {
        anyhow::bail!("File not found: {}", resolved.display());
    }
    let old_content = std::fs::read_to_string(&resolved)?;
    let new_content = apply_hashline_edits(&old_content, &params.edits)?;
    Ok((resolved, old_content, new_content))
}
```

- [ ] **Step 2: Refactor `execute()` to use it**

Change `EditTool::execute()` (lines 121–149) to delegate the read/apply to `compute_edit`:

```rust
async fn execute(&self, args: Value) -> anyhow::Result<Value> {
    let params: HashlineEditParams = serde_json::from_value(args)?;
    let (resolved, old_content, new_content) = compute_edit(&self.ctx, &params)?;
    std::fs::write(&resolved, &new_content)?;

    let lines_changed = (new_content.split('\n').count() as i64
        - old_content.split('\n').count() as i64)
        .unsigned_abs() as usize;

    let snippet = format_edit_snippet(&new_content, &params.edits, 2);
    Ok(Value::String(format!(
        "Edited {} ({} line(s) net change).\n{}",
        resolved.display(),
        lines_changed,
        snippet
    )))
}
```

- [ ] **Step 3: Run existing edit tests to confirm the refactor is behavior-preserving**

Run:
```bash
cargo test -p gait_ai tools::edit 2>&1 > /tmp/gait-test-output.txt; echo "exit: $?"
grep "FAILED\|test result" /tmp/gait-test-output.txt
```
Expected: the existing edit tests (e.g. `replaces_single_line`) still pass.

- [ ] **Step 4: Write the failing permission-preview tests**

In `src/tools/edit.rs` `#[cfg(test)] mod tests`, add (uses the existing `make_ctx` helper and `compute_line_hash`):

```rust
#[test]
fn permission_includes_diff_preview_for_valid_edit() {
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
    let crate::types::ToolPermission::RequiresApproval(reqs) = tool.permission(&args) else {
        panic!("expected RequiresApproval");
    };
    let preview = reqs[0].preview.as_deref().expect("preview should be Some");
    assert!(preview.contains("-    bar();"), "preview:\n{preview}");
    assert!(preview.contains("+    baz();"), "preview:\n{preview}");
}

#[test]
fn permission_preview_is_none_for_stale_anchor() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("f.rs");
    std::fs::write(&path, "fn foo() {\n    bar();\n}").unwrap();

    let tool = EditTool::new(make_ctx(&dir));
    let args = serde_json::json!({
        "file_path": "f.rs",
        // bogus hash → apply_hashline_edits fails → preview None
        "edits": [{"op": "replace", "pos": "2#ZZ", "lines": ["    baz();"]}]
    });
    let crate::types::ToolPermission::RequiresApproval(reqs) = tool.permission(&args) else {
        panic!("expected RequiresApproval");
    };
    assert_eq!(reqs[0].preview, None);
}
```

- [ ] **Step 5: Run to verify they fail**

Run:
```bash
cargo test -p gait_ai tools::edit::tests::permission 2>&1 > /tmp/gait-test-output.txt; echo "exit: $?"
grep "FAILED\|test result\|panicked" /tmp/gait-test-output.txt
```
Expected: FAIL — `permission_includes_diff_preview_for_valid_edit` fails because `preview` is currently `None` (set in Task 2).

- [ ] **Step 6: Populate `preview` in `permission()`**

Change `EditTool::permission()` (lines 151–162) to compute the diff best-effort. Any parse/IO/anchor failure degrades to `None`:

```rust
fn permission(&self, args: &Value) -> ToolPermission {
    let raw = args.get("file_path").and_then(|v| v.as_str()).unwrap_or("*");
    let rel = std::path::Path::new(raw)
        .strip_prefix(&*self.ctx.codebase_path)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| raw.to_string());
    let preview = serde_json::from_value::<HashlineEditParams>(args.clone())
        .ok()
        .and_then(|params| compute_edit(&self.ctx, &params).ok())
        .and_then(|(_, old, new)| crate::tools::preview::diff_preview(&old, &new));
    ToolPermission::RequiresApproval(vec![PermissionRequest {
        display: rel.clone(),
        patterns: path_patterns("edit", &rel),
        preview,
    }])
}
```

- [ ] **Step 7: Run to verify they pass**

Run:
```bash
cargo test -p gait_ai tools::edit 2>&1 > /tmp/gait-test-output.txt; echo "exit: $?"
grep "FAILED\|test result" /tmp/gait-test-output.txt
```
Expected: `test result: ok` — all edit tests pass.

- [ ] **Step 8: Commit**

```bash
git add src/tools/edit.rs
git commit -m "feat: render diff preview in EditTool::permission via compute_edit"
```

---

### Task 5: Populate `preview` in `WriteTool::permission()`

Existing file → diff; new file → full content; read error on an existing file → `None`.

**Files:**
- Modify: `src/tools/write.rs` (populate `permission`)
- Test: `src/tools/write.rs` `#[cfg(test)]`

- [ ] **Step 1: Confirm the test helper**

Open `src/tools/write.rs` and check the `#[cfg(test)]` block for an existing `make_ctx`/`TempDir` helper. If none exists, add this one to the test module (mirrors edit.rs):

```rust
fn make_ctx(dir: &tempfile::TempDir) -> crate::tools::ctx::ToolCtx {
    crate::tools::ctx::ToolCtx::new(
        dir.path().to_path_buf(),
        dir.path().to_path_buf(),
        "test".into(),
    )
}
```

(Check the top of the test module for how `TempDir` is imported — edit.rs uses `use tempfile::TempDir;`. Match it.)

- [ ] **Step 2: Write the failing tests**

In `src/tools/write.rs` `#[cfg(test)] mod tests`, add:

```rust
#[test]
fn permission_preview_is_full_content_for_new_file() {
    let dir = tempfile::TempDir::new().unwrap();
    let tool = WriteTool::new(make_ctx(&dir));
    let args = serde_json::json!({"file_path": "new.txt", "content": "hello\nworld\n"});
    let crate::types::ToolPermission::RequiresApproval(reqs) = tool.permission(&args) else {
        panic!("expected RequiresApproval");
    };
    assert_eq!(reqs[0].preview.as_deref(), Some("hello\nworld\n"));
}

#[test]
fn permission_preview_is_diff_for_existing_file() {
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::write(dir.path().join("f.txt"), "one\ntwo\n").unwrap();
    let tool = WriteTool::new(make_ctx(&dir));
    let args = serde_json::json!({"file_path": "f.txt", "content": "one\nTWO\n"});
    let crate::types::ToolPermission::RequiresApproval(reqs) = tool.permission(&args) else {
        panic!("expected RequiresApproval");
    };
    let preview = reqs[0].preview.as_deref().expect("preview Some");
    assert!(preview.contains("-two"), "preview:\n{preview}");
    assert!(preview.contains("+TWO"), "preview:\n{preview}");
}
```

(Confirm `WriteTool::new` is the constructor — check the top of `write.rs`; edit.rs uses `EditTool::new(ctx)`, write.rs should match.)

- [ ] **Step 3: Run to verify they fail**

Run:
```bash
cargo test -p gait_ai tools::write 2>&1 > /tmp/gait-test-output.txt; echo "exit: $?"
grep "FAILED\|test result\|panicked" /tmp/gait-test-output.txt
```
Expected: FAIL — `preview` is currently `None` (set in Task 2).

- [ ] **Step 4: Populate `preview` in `permission()`**

Change `WriteTool::permission()` (lines 54–65) to:

```rust
fn permission(&self, args: &serde_json::Value) -> ToolPermission {
    let raw = args.get("file_path").and_then(|v| v.as_str()).unwrap_or("*");
    let rel = std::path::Path::new(raw)
        .strip_prefix(&*self.ctx.codebase_path)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| raw.to_string());
    let preview = serde_json::from_value::<WriteParams>(args.clone()).ok().and_then(|params| {
        let resolved = if std::path::Path::new(&params.file_path).is_absolute() {
            PathBuf::from(&params.file_path)
        } else {
            self.ctx.codebase_path.join(&params.file_path)
        };
        if resolved.exists() {
            // Existing file → diff (None on read error or identical content).
            std::fs::read_to_string(&resolved)
                .ok()
                .and_then(|old| crate::tools::preview::diff_preview(&old, &params.content))
        } else {
            // New file → show the full content.
            Some(params.content.clone())
        }
    });
    ToolPermission::RequiresApproval(vec![PermissionRequest {
        display: rel.clone(),
        patterns: path_patterns("write", &rel),
        preview,
    }])
}
```

(`PathBuf` is already imported in `write.rs` — it is used in `execute()`. If the build complains, add `use std::path::PathBuf;`.)

- [ ] **Step 5: Run to verify they pass**

Run:
```bash
cargo test -p gait_ai tools::write 2>&1 > /tmp/gait-test-output.txt; echo "exit: $?"
grep "FAILED\|test result" /tmp/gait-test-output.txt
```
Expected: `test result: ok` — all write tests pass.

- [ ] **Step 6: Commit**

```bash
git add src/tools/write.rs
git commit -m "feat: render preview in WriteTool::permission (diff or full content)"
```

---

### Task 6: Render the preview block in the live permission prompt

Add a pure, testable formatter in `ui/permission.rs`, then print it in the live prompt (`ui/builder.rs::run_permission`) between the `⟳ <display>` subject line and the numbered patterns.

**Files:**
- Modify: `src/ui/permission.rs` (add `preview_block` fn + test)
- Modify: `src/ui/builder.rs:135-145` (print the block)

- [ ] **Step 1: Write the failing test for the formatter**

In `src/ui/permission.rs` `#[cfg(test)] mod tests`, add:

```rust
#[test]
fn preview_block_frames_lines_with_blanks() {
    assert_eq!(
        preview_block(Some("-a\n+b")),
        vec!["".to_string(), "-a".to_string(), "+b".to_string(), "".to_string()]
    );
}

#[test]
fn preview_block_empty_for_none_or_empty() {
    assert_eq!(preview_block(None), Vec::<String>::new());
    assert_eq!(preview_block(Some("")), Vec::<String>::new());
}
```

- [ ] **Step 2: Run to verify it fails**

Run:
```bash
cargo test -p gait_ai ui::permission 2>&1 > /tmp/gait-test-output.txt; echo "exit: $?"
grep "test result\|^error\|cannot find" /tmp/gait-test-output.txt
```
Expected: compile error — `preview_block` not found.

- [ ] **Step 3: Implement `preview_block`**

In `src/ui/permission.rs` (module level, not inside `tests`), add:

```rust
/// Lines to print for a permission preview, framed by a leading and trailing
/// blank line. `None` or empty preview → empty vec (nothing is printed).
/// The UI prints these verbatim — it is blind to the preview's format.
pub fn preview_block(preview: Option<&str>) -> Vec<String> {
    match preview {
        Some(p) if !p.is_empty() => {
            let mut out = vec![String::new()];
            out.extend(p.lines().map(|l| l.to_string()));
            out.push(String::new());
            out
        }
        _ => Vec::new(),
    }
}
```

- [ ] **Step 4: Run to verify it passes**

Run:
```bash
cargo test -p gait_ai ui::permission 2>&1 > /tmp/gait-test-output.txt; echo "exit: $?"
grep "FAILED\|test result" /tmp/gait-test-output.txt
```
Expected: `test result: ok`.

- [ ] **Step 5: Print the block in the live prompt**

In `src/ui/builder.rs::run_permission`, after the `⟳ <display>` subject is printed (the if/else at lines 137–141) and before the patterns loop (line 143), insert the preview block. The relevant region currently reads:

```rust
            if items.len() > 1 {
                eprintln!("{}⟳ {}  {}({} of {}){}", self.renderer.theme().accent.fg(), item.cmd, DIM_FG, idx + 1, items.len(), RESET);
            } else {
                eprintln!("{}⟳ {}{}", self.renderer.theme().accent.fg(), item.cmd, RESET);
            }
            eprintln!();
            for (i, pat) in item.patterns.iter().enumerate() {
```

Change it to:

```rust
            if items.len() > 1 {
                eprintln!("{}⟳ {}  {}({} of {}){}", self.renderer.theme().accent.fg(), item.cmd, DIM_FG, idx + 1, items.len(), RESET);
            } else {
                eprintln!("{}⟳ {}{}", self.renderer.theme().accent.fg(), item.cmd, RESET);
            }
            for line in crate::ui::permission::preview_block(item.preview.as_deref()) {
                eprintln!("{}{}{}", DIM_FG, line, RESET);
            }
            eprintln!();
            for (i, pat) in item.patterns.iter().enumerate() {
```

(The existing `eprintln!();` blank line before the patterns is kept; `preview_block` supplies its own trailing blank, so the patterns stay visually separated whether or not a preview is present.)

- [ ] **Step 6: Build and run the full suite**

Run:
```bash
cargo build 2>&1 > /tmp/gait-build-output.txt; echo "exit: $?"
grep "^error" /tmp/gait-build-output.txt | head
cargo test 2>&1 > /tmp/gait-test-output.txt; echo "exit: $?"
grep "FAILED\|test result" /tmp/gait-test-output.txt
```
Expected: build exit 0, all tests green.

- [ ] **Step 7: Commit**

```bash
git add src/ui/permission.rs src/ui/builder.rs
git commit -m "feat: render diff preview block in the permission prompt"
```

---

### Task 7: Mark the spec done

**Files:**
- Modify: `docs/specs/2026-06-13-permission-diff-preview-design.md`

- [ ] **Step 1: Add `status: done` frontmatter**

Per `CLAUDE.md`, prepend YAML frontmatter to the spec:

```
---
status: done
---
```

- [ ] **Step 2: Commit**

```bash
git add docs/specs/2026-06-13-permission-diff-preview-design.md
git commit -m "docs: mark permission diff-preview spec done"
```

---

## Self-Review

**Spec coverage:**
- `preview` field in 3 locations (types.rs, transcript.rs, proto) — Tasks 2, 3. ✅
- `events::permission_item` both directions — Task 3 covers BOTH converters (events.rs Rust→proto + state.rs proto→Rust; the spec missed the state.rs one). ✅
- `src/tools/preview.rs` with `diff_preview` using `similar` grouped_ops, no `@@`, no truncation — Task 1. ✅
- Shared `compute_edit` helper — Task 4. ✅
- `EditTool::permission()` best-effort, degrade to None — Task 4. ✅
- `WriteTool::permission()` existing→diff / new→full content / read-error→None — Task 5. ✅
- UI render between subject and patterns — Task 6 (in `builder.rs`, the live path; spec named the dead `prompt_permission`). ✅
- Tests: preview.rs unit, edit/write tool, proto round-trip, UI — Tasks 1, 3, 4, 5, 6. ✅
- Out of scope (shell previews, truncation, word-level, syntax color) — none added. ✅

**Persistence rationale (spec "Why persist"):** preview rides the proto log via Task 3, so a resumed/headless session re-renders from `perm_to_tx` without file I/O. ✅

**Type consistency:** `diff_preview(&str, &str) -> Option<String>` used identically in Tasks 4 and 5. `compute_edit(ctx, params) -> (PathBuf, String, String)` defined and consumed in Task 4. `preview_block(Option<&str>) -> Vec<String>` defined in Task 6 Step 3, called in Step 5. `preview: Option<String>` field name consistent across `PermissionRequest`, Rust `PermissionItem`, proto, and both converters. ✅

**Placeholder scan:** no TBD/TODO; every code step has complete code. Two steps ask the implementer to confirm an existing helper/import before writing (proto round-trip helper names in Task 3 Step 7; `WriteTool::new`/`TempDir` import in Task 5) — these are verification instructions with a concrete fallback, not placeholders. ✅

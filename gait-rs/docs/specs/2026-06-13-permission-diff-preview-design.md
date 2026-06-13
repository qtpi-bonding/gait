---
status: done
---

# Permission Diff Preview — Design

## Goal

Show the user *what a tool is about to change* before they approve it. Today the permission
prompt shows only the subject (`⟳ src/turn.rs`) and the allow-list patterns — not the actual
diff. This adds a `preview` block to the prompt: a unified `-`/`+` diff for `edit`, and a
diff-or-content view for `write`.

## Background

- The permission prompt lives in `src/ui/permission.rs::prompt_permission`. It renders
  `⟳ <display>`, then numbered allow-list patterns, then the input line.
- A tool declares what approval it needs via `Tool::permission(&self, args) -> ToolPermission`.
  For file tools this returns `ToolPermission::RequiresApproval(vec![PermissionRequest { display,
  patterns }])`.
- `PermissionRequest` (`src/types.rs`) currently has `display` and `patterns`.
- `turn.rs` converts each `PermissionRequest` into a `PermissionItem`
  (`src/session/transcript.rs` + proto `gait.log.v1.PermissionItem` + `events::permission_item`),
  which is emitted to the session log as part of a `Permissions` event.
- The UI reads `PermissionItem` (from the log, via the transcript) to render the prompt.
- The `edit` tool is **hashline-based**: `HashlineEditParams { file_path, edits: Vec<HashlineEditOp> }`.
  Each op references lines by `N#XY` anchors and carries replacement `lines`. `execute()` does
  `read_to_string(old)` → `apply_hashline_edits(&old, &edits)` → `new`, then writes `new`.

## Architecture

A single optional field, `preview: Option<String>`, threaded along the existing
`PermissionRequest → PermissionItem → UI` path. The **tool layer owns the content** — it computes
the preview string in `permission()`. The UI renders whatever string is there, blind to format.
This matches the boundary rule in `docs/architecture.md` ("the tool layer owns preview content").

### Why persist `preview` in the log

A turn can pause awaiting permission (the `Permissions` event is written, the process exits) and a
later `gait` invocation re-renders the prompt from the persisted `PermissionItem`s. If `preview`
were recomputed at render time, the UI would have to read files and apply edits — pushing tool
logic into the UI and breaking the boundary. Instead the tool computes the preview **once**, it is
frozen into the `PermissionItem`, persisted, and the UI stays dumb.

This means `PermissionItem` gains a field (the watch-point flagged as known-issue #4 in
`architecture.md`). That is acceptable here: `preview` is **frozen derived session state** — a
snapshot of what the change would do at request time — not pure presentation. It belongs in the log.

## Components

### 1. `preview` field (three locations)

- `src/types.rs` — `PermissionRequest { display, patterns, preview: Option<String> }`
- `proto/log.proto` — add `optional string preview` to the `PermissionItem` message
- `src/session/transcript.rs` — `PermissionItem` Rust struct gains `preview: Option<String>`
  (with `#[serde(default, skip_serializing_if = "Option::is_none")]`)
- `src/proto/events.rs` — `permission_item` converter copies `preview` both directions

All existing `PermissionRequest { .. }` and `PermissionItem { .. }` literals (≈25 + a few)
get `preview: None`. The field is opt-in, so this is mechanical.

### 2. `src/tools/preview.rs` (new module)

```rust
/// Unified -/+ diff of old→new (context_radius = 2). None if identical.
pub fn diff_preview(old: &str, new: &str) -> Option<String>
```

Uses `similar::TextDiff::from_lines(old, new)`, then renders manually from `grouped_ops(2)` (2
context lines) via `iter_changes`, prefixing each line `-`/`+`/space. Returns `None` when there are
no changes. **Do not** use `similar`'s built-in `unified_diff()` — it emits `@@ … @@` hunk headers,
which the chosen format omits. **No truncation** — the full diff is shown.

Add the `similar` crate to `Cargo.toml` (pure Rust, lightweight; used by insta/ruff).

### 3. Shared read+apply helper (edit tool)

Factor the `read_to_string` + `apply_hashline_edits` step out of `EditTool::execute()` into a small
helper so `permission()` and `execute()` compute `new_content` identically:

```rust
// returns (old_content, new_content) or an error (stale anchor / missing file)
fn compute_edit(ctx: &ToolCtx, params: &HashlineEditParams) -> anyhow::Result<(String, String)>
```

### 4. `EditTool::permission()`

1. Parse `file_path` + `edits` from `args` (best-effort; on parse failure → `preview = None`).
2. `compute_edit(...)`:
   - file missing or `apply_hashline_edits` errors (stale anchor) → `preview = None`
   - success → `preview = diff_preview(&old, &new)`
3. Return `RequiresApproval(vec![PermissionRequest { display, patterns, preview }])`.

`permission()` stays **synchronous** — small sync `fs::read`. Any IO/parse error degrades to
`preview = None`; it never blocks or panics the permission flow.

### 5. `WriteTool::permission()`

- File **exists** → `preview = diff_preview(&old_content, &new_content)` (overwrite shown as a diff).
- File is **new** → `preview = Some(new_content)` (full content, untruncated).
- Read error → `preview = None`.

### 6. UI render (`src/ui/permission.rs::prompt_permission`)

After the `⟳ <display>` line and before the numbered patterns, if `item.preview` is `Some`, print a
blank line, the preview block (already a `\n`-joined string), and a blank line. Absent preview →
unchanged layout.

## Data Flow

```
EditTool::permission(args)
  └─ compute_edit → (old, new) → diff_preview(old,new) ─┐
                                                         ▼
              PermissionRequest { display, patterns, preview: Some("- ...\n+ ...") }
  turn.rs → PermissionItem { ..., preview }  ──emit──▶  Permissions event in log.jsonl
  ui::prompt_permission(item) reads item.preview ──▶ renders the block above the patterns
```

## Error Handling

Every failure path in `permission()` degrades to `preview = None` — a missing preview is never an
error. Specifically: missing file, stale hashline anchor (apply fails), arg parse failure, or any
`fs` read error. The permission prompt always renders; only the preview block is omitted.

## Testing

**`preview.rs` unit tests**
- small change → output contains `-old` and `+new` lines plus context
- identical input → `None`
- multi-hunk change → both hunks present

**Tool tests**
- `edit`: valid edit → `permission()`'s `PermissionRequest.preview` is `Some` and contains the
  `-`/`+` lines; stale-anchor edit → `preview = None`
- `write`: overwrite existing → diff preview; new file → full-content preview

**Proto round-trip**
- `PermissionItem` with `preview` set survives `events::permission_item` → proto → JSON → back

**UI**
- `prompt_permission` with a `preview` present renders the block above the patterns (mirror the
  existing permission UI tests)

## Out of Scope (YAGNI)

- `shell` previews — the command is already the prompt subject.
- Truncation / size caps — show the full diff/content.
- Intra-line (word-level) highlighting — line-level `-`/`+` only.
- Syntax coloring of the diff body.

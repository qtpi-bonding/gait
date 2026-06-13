# Preview Policy Split — Design

## Goal

Today the diff preview is computed inside `Tool::permission()`, which `turn.rs` calls for **every**
tool call to evaluate the deny/allow/ask tiers — so in `--auto` mode (and for allow-listed tools) the
edit/write diff is computed and then thrown away. This:

1. Splits preview computation off the gating path into its own `Tool::preview()` method that
   `turn.rs` calls **only when it actually needs the preview**.
2. Adds a configurable `preview` policy (`gait.jsonc`) with three modes — default keeps previews on
   the ask path only; `always` also captures previews for auto-run tools in the durable log.

## Background

- `turn.rs` gating loop (`src/turn.rs:859-924`) calls `tool.permission(&args)` once per tool call to
  classify it: Tier 1 deny → Tier 2 allow (`is_allowed = auto || allow-listed`) → Tier 3 ask.
- Tier 3 builds a `PermissionItem` (with the preview) and the turn exits awaiting approval.
- Tier 2 (auto / allow-listed) calls `continue` — the tool runs later via `execute_tool_call`
  (`src/turn.rs:176-245`), which emits the `ToolCall` event. The preview computed in `permission()`
  is discarded here.
- `execute_tool_call(tc, tools, emitter, approval_prefix)` is the single place a `ToolCall` event is
  emitted. It is called from 4 sites: the resume/approved path (`:604`), the parallel batch (`:1006`),
  and two serial paths (`:1113`, `:1216`).
- The gating loop already threads per-call metadata to execution via a `call_id → String` map
  (`tool_approval_prefixes`). The preview map mirrors this exactly.
- Config: `GaitConfig` (`src/config.rs:92-148`) is parsed from `gait.jsonc`. Per-invocation values
  reach the turn through `TurnInput { task, auto, max_iter }` (`src/turn.rs:49-54`), constructed by
  `bin/gait.rs`. `ResolvedProfile` holds resolved provider/tools, not scalar config knobs.

## Architecture

**The split.** `Tool::permission()` returns only what gating needs (`display` + `patterns`). A new
`Tool::preview(&self, args) -> Option<String>` (default `None`) owns the rendered preview. `turn.rs`
decides *when* to call `preview()` based on the policy; the tool still owns the *content* (the boundary
rule is preserved — the preview string is produced by the tool, never by `turn.rs` or the UI).

**The policy.** A `PreviewPolicy` enum threaded through `TurnInput` into the gating loop:

| Mode | At the ask prompt (Tier 3) | For auto / allow-listed tools (Tier 2) |
|------|----------------------------|----------------------------------------|
| `on-ask` (default) | preview shown | nothing |
| `always` | preview shown | preview attached to the `ToolCall` event in the log |
| `off` | nothing | nothing |

This is non-duplicative: a gated tool's preview lives in its `PermissionItem`; an auto-run tool's
preview lives on its `ToolCall` event. `always` captures both paths; `on-ask` captures only the gated
path; `off` captures neither.

## Components

### 1. `Tool` trait split (`src/types.rs`)

- `permission()` default impl: drop the `preview` field from the `PermissionRequest` it builds.
- Add:
  ```rust
  /// A rendered preview of what this tool will do (e.g. a diff), shown in the
  /// permission prompt or attached to the ToolCall event per the preview policy.
  /// Default: no preview. Cheap tools return None; edit/write override this.
  fn preview(&self, _args: &serde_json::Value) -> Option<String> { None }
  ```

### 2. Remove `preview` from `PermissionRequest` (`src/types.rs`)

`PermissionRequest` reverts to `{ display, patterns }`. Every construction site (~24) drops
`preview: None`. The field moved to a method; the struct is pure gating identity again.

### 3. `EditTool` / `WriteTool` (`src/tools/edit.rs`, `src/tools/write.rs`)

- `permission()` returns `{ display, patterns }` only (no preview computation).
- Move the preview computation into `preview()`:
  - **edit:** `serde_json::from_value::<HashlineEditParams>(args).ok().and_then(|p| compute_edit(&self.ctx, &p).ok()).and_then(|(_, old, new)| diff_preview(&old, &new))` — same best-effort chain, now in `preview()`.
  - **write:** existing file → `diff_preview(&old, &content)`; new file → `Some(content)`; read error → `None` — same logic, now in `preview()`.
- `compute_edit` and `crate::tools::preview::diff_preview` are unchanged.

### 4. `PreviewPolicy` enum + config (`src/config.rs`)

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum PreviewPolicy {
    #[default]
    OnAsk,
    Always,
    Off,
}
```
Add to `GaitConfig`:
```rust
#[serde(default)]
pub preview: PreviewPolicy,
```
`PreviewPolicy::default()` is `OnAsk`, so absent config = current default behavior. Add `preview:
PreviewPolicy::OnAsk` to `GaitConfig::default()` for explicitness.

### 5. Thread the policy into the turn (`src/turn.rs`, `src/bin/gait.rs`)

- Add `pub preview_policy: PreviewPolicy` to `TurnInput`.
- `bin/gait.rs` populates it from `GaitConfig.preview` when constructing `TurnInput` (next to where
  `max_iter` is wired).
- `run_turn` extracts `let preview_policy = input.preview_policy;` alongside `max_iter`.

### 6. Gating loop (`src/turn.rs:859-924`)

- **Tier 3 (ask):** when building the `PermissionItem`, set
  `preview: if preview_policy == PreviewPolicy::Off { None } else { tool.preview(&tc.args) }`.
  (`tool` = the resolved tool for `tc.tool_name`; if not found, `None`.)
- **Tier 2 (allowed):** in the `is_allowed` branch, when `preview_policy == PreviewPolicy::Always`,
  compute `tool.preview(&tc.args)` and, if `Some`, insert into a new
  `tool_previews: HashMap<String, String>` keyed by `tc.tool_call_id` (mirrors
  `tool_approval_prefixes`).

### 7. Attach preview to `ToolCall` on the execute path (`src/turn.rs`, proto, `src/proto/events.rs`)

- Proto (`/Users/aicoder/Documents/ideation/gait/proto/log.proto`): add `optional string preview = 4;`
  to the `ToolCall` message (currently ends at field 3).
- `events::tool_call` (`src/proto/events.rs:81-88`): add a `preview: Option<String>` parameter, set on
  the `pb::ToolCall`.
- `execute_tool_call` (`src/turn.rs:176`): add a `preview: Option<String>` parameter; pass it to
  `events::tool_call(...)`. The in-memory `AssistantPart::ToolCall` does **not** carry preview (the LLM
  never needs the diff — preview is human-facing durable metadata only).
- At each of the 4 `execute_tool_call` call sites:
  - Auto / allow-listed execution paths (`:1006`, `:1113`, `:1216`): look up
    `tool_previews.get(&tc.tool_call_id).cloned()` and pass it.
  - Resume/approved path (`:604`): pass `None` — the preview is already in the `PermissionItem` from
    when it was asked, so re-attaching to `ToolCall` would duplicate it.

### 8. Read-back / rendering

- `retrace.rs` `ToolCall` projection (`:307`) feeds the LLM message reconstruction and must keep
  ignoring `preview` (the model doesn't see it).
- The `preview` on the `ToolCall` event is durable, human-facing metadata for `gait status` /
  transcript replay. The plan will locate the path that renders tool calls for humans and surface
  `preview` there (a dim block under the tool-call line). If no such dedicated human render exists yet,
  the minimum bar is that the field round-trips in the log (verified by a proto round-trip test) — a
  richer status renderer can follow.

### 9. Docs

- Update `CLAUDE.md` / `docs/architecture.md` boundary rule: preview content is produced by
  `Tool::preview()`; `turn.rs` decides *when* to call it per the `preview` policy; the UI only renders.
  (Supersedes the old "preview is populated in `permission()`" wording.)

## Data Flow

```
gait.jsonc { "preview": "always" } ──▶ GaitConfig.preview ──▶ TurnInput.preview_policy ──▶ run_turn
                                                                                              │
  gating loop, per tool call:                                                                 ▼
   ├─ Tier 3 ask:   PermissionItem.preview = (policy≠Off) ? tool.preview(args) : None  ──emit──▶ log
   └─ Tier 2 allow: if policy==Always { tool_previews[call_id] = tool.preview(args) }
                                                       │
        execution phase: execute_tool_call(tc, …, preview = tool_previews[call_id]) ──emit──▶ ToolCall{preview} in log
```

## Error Handling

`tool.preview()` is best-effort and infallible (`-> Option<String>`); any internal IO/parse failure
returns `None`. The gating and execution paths never block or fail on a missing preview.

## Testing

**Tool trait / tools**
- `EditTool::preview()` returns `Some` diff for a valid edit, `None` for a stale anchor.
- `WriteTool::preview()`: existing→diff, new→full content, read-error→None.
- `EditTool::permission()` / `WriteTool::permission()` now return `{display, patterns}` with no preview
  (the field no longer exists on `PermissionRequest`).

**Config**
- `gait.jsonc` with `"preview": "always"` / `"off"` / absent parses to the right `PreviewPolicy`
  (absent → `OnAsk`). Unknown string → parse error surfaced as a warning, falls back to default.

**Proto round-trip**
- `ToolCall` with `preview: Some(...)` survives `events::tool_call` → log line → parse.

**Turn behavior (integration)**
- Policy `on-ask`: an auto-run edit emits a `ToolCall` with `preview == None`.
- Policy `always`: an auto-run edit emits a `ToolCall` with `preview == Some(diff)`.
- Policy `off`: a gated edit's `PermissionItem.preview == None`.
- Policy `on-ask`: a gated edit's `PermissionItem.preview == Some(diff)` (unchanged from today).

## Out of Scope (YAGNI)

- Per-tool preview policy (one global policy is enough).
- Showing previews to the LLM (preview is human-facing only).
- A rich syntax-highlighted status renderer — a plain dim block is enough; durable round-trip is the bar.
- Truncation / size caps (consistent with the existing preview feature).

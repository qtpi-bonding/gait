# Preview Policy Split — Design

## Goal

Today the diff preview is computed inside `Tool::permission()`, which `turn.rs` calls for **every**
tool call to evaluate the deny/allow/ask tiers — so in `--auto` mode (and for allow-listed tools) the
edit/write diff is computed and then thrown away. This:

1. Splits preview computation off the gating path into its own `Tool::preview()` method that
   `turn.rs` calls **only when it actually needs the preview**.
2. Adds a configurable `preview` policy (`gait.jsonc`) with three modes — default keeps previews on
   the ask path only; `always` also streams previews live for auto-run tools (ephemeral, not logged).

## Background

- `turn.rs` gating loop (`src/turn.rs:859-924`) calls `tool.permission(&args)` once per tool call to
  classify it: Tier 1 deny → Tier 2 allow (`is_allowed = auto || allow-listed`) → Tier 3 ask.
- Tier 3 builds a `PermissionItem` (with the preview) and the turn exits awaiting approval.
- Tier 2 (auto / allow-listed) calls `continue` — the tool runs later via `execute_tool_call`. The
  preview computed in `permission()` is discarded here.
- The `Emitter` (`src/proto/emitter.rs`) has two channels: `emit(body)` writes a durable Event to
  `log.jsonl` **and** streams it; `delta(content)` streams an ephemeral `TextDelta` with **no
  transcript write**. `delta` is the existing mechanism for "show the user, don't persist" and renders
  in every mode (it's the same channel that streams the model's text).
- Config: `GaitConfig` (`src/config.rs:92-148`) is parsed from `gait.jsonc`. Per-invocation values
  reach the turn through `TurnInput { task, auto, max_iter }` (`src/turn.rs:49-54`), constructed by
  `bin/gait.rs`. `ResolvedProfile` holds resolved provider/tools, not scalar config knobs.

## Why ephemeral (not durable) for `always`

We persisted the **prompt** preview into the log because a turn can pause awaiting approval and a later
invocation must re-render that prompt without file I/O — the preview had to be frozen in the log. That
justification **does not exist for an auto-run tool**: it executes immediately, and nothing ever
re-renders it. Persisting it would mean mutating the durable log format and bloating `log.jsonl` with
(potentially large) diffs, for data that is purely presentational.

And we lose nothing by not storing it: the `ToolCall` event already records the edit args durably, and
the pre-edit file state is recoverable from the checkpoint/codebase git — so the diff stays
**recomputable** after the fact. The durable record of what changed lives in the codebase, not the log.
`always` therefore streams the diff live (ephemeral) and a future `gait status --diffs` could recompute
it on read if after-the-fact rendering is ever wanted.

## Architecture

**The split.** `Tool::permission()` returns only what gating needs (`display` + `patterns`). A new
`Tool::preview(&self, args) -> Option<String>` (default `None`) owns the rendered preview. `turn.rs`
decides *when* to call `preview()` based on the policy; the tool still owns the *content* (the boundary
rule is preserved — the preview string is produced by the tool, never by `turn.rs` or the UI).

**The policy.** A `PreviewPolicy` enum threaded through `TurnInput` into the gating loop:

| Mode | At the ask prompt (Tier 3) | For auto / allow-listed tools (Tier 2) |
|------|----------------------------|----------------------------------------|
| `on-ask` (default) | preview shown in prompt | nothing |
| `always` | preview shown in prompt | diff streamed live via `emitter.delta` (ephemeral) |
| `off` | nothing | nothing |

## Components

### 1. `Tool` trait split (`src/types.rs`)

- `permission()` default impl: drop the `preview` field from the `PermissionRequest` it builds.
- Add:
  ```rust
  /// A rendered preview of what this tool will do (e.g. a diff), shown in the
  /// permission prompt or streamed live per the preview policy. Default: no
  /// preview. Cheap tools return None; edit/write override this.
  fn preview(&self, _args: &serde_json::Value) -> Option<String> { None }
  ```

### 2. Remove `preview` from `PermissionRequest` (`src/types.rs`)

`PermissionRequest` reverts to `{ display, patterns }`. Every construction site (~24) drops
`preview: None`. The field moved to a method; the struct is pure gating identity again.
(`PermissionItem.preview` in `transcript.rs` and the proto `PermissionItem.preview` field **stay** —
the prompt preview is still persisted for re-render on resume; only `PermissionRequest` loses it.)

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
  `max_iter` is wired). Update any other `TurnInput { .. }` construction sites (tests) accordingly.
- `run_turn` extracts `let preview_policy = input.preview_policy;` alongside `max_iter`.

### 6. Gating loop (`src/turn.rs:859-924`)

- **Tier 3 (ask):** when building the `PermissionItem`, set
  `preview: if preview_policy == PreviewPolicy::Off { None } else { tool.preview(&tc.args) }`.
  (`tool` = the resolved tool for `tc.tool_name`; if not found, `None`.) This replaces the current
  `preview: req.preview.clone()` (which no longer exists once §2 lands).
- **Tier 2 (allowed):** in the `is_allowed` branch, when `preview_policy == PreviewPolicy::Always`,
  compute `tool.preview(&tc.args)` and, if `Some(p)`, stream it ephemerally:
  `emitter.delta(format_preview_delta(&p));` — no map, no `execute_tool_call` change, no log write.

`format_preview_delta` formats the diff as a dim, framed block string for the stream. Reuse
`crate::ui::permission::preview_block` (returns the framed `Vec<String>`) joined with `\n`, or a small
sibling helper. The exact ANSI styling is a plan detail; it must render cleanly as streamed text.

### 7. Read-back / rendering

No proto change. `execute_tool_call` and the `ToolCall` event are untouched. `retrace.rs`'s `ToolCall`
projection stays preview-blind (the model never sees the diff). The `always`-mode diff is purely an
ephemeral stream delta — nothing to persist or render from the log.

### 8. Docs

Update `CLAUDE.md` / `docs/architecture.md` boundary rule: preview content is produced by
`Tool::preview()`; `turn.rs` decides *when* to call it per the `preview` policy (prompt, or ephemeral
`delta` in `always` mode); the UI only renders. (Supersedes the old "preview is populated in
`permission()`" wording.)

## Data Flow

```
gait.jsonc { "preview": "always" } ─▶ GaitConfig.preview ─▶ TurnInput.preview_policy ─▶ run_turn
                                                                                            │
  gating loop, per tool call:                                                               ▼
   ├─ Tier 3 ask:   PermissionItem.preview = (policy≠Off) ? tool.preview(args) : None ─emit─▶ log (durable)
   └─ Tier 2 allow: if policy==Always { emitter.delta(format(tool.preview(args))) }  ─delta─▶ stream (ephemeral)
```

## Error Handling

`tool.preview()` is best-effort and infallible (`-> Option<String>`); any internal IO/parse failure
returns `None`. The gating and execution paths never block or fail on a missing preview. A `None`
preview in `always` mode simply streams nothing for that call.

## Testing

**Tool trait / tools**
- `EditTool::preview()` returns `Some` diff for a valid edit, `None` for a stale anchor.
- `WriteTool::preview()`: existing→diff, new→full content, read-error→None.
- `EditTool::permission()` / `WriteTool::permission()` return `{display, patterns}` and compile without
  any `preview` field on `PermissionRequest`.

**Config**
- `gait.jsonc` with `"preview": "always"` / `"off"` / absent parses to the right `PreviewPolicy`
  (absent → `OnAsk`). Unknown string → parse warning, falls back to default (existing `load_config`
  behavior).

**Turn behavior (integration, via the `Emitter`'s stream receiver — mirror `emitter.rs` tests)**
- Policy `always`: an auto-run edit produces a `StreamItem::Delta` whose content contains the diff
  (`-old`/`+new`), and NO preview is written to `log.jsonl`.
- Policy `on-ask`: an auto-run edit produces no such delta.
- Policy `off`: a gated edit's `PermissionItem.preview == None`.
- Policy `on-ask`: a gated edit's `PermissionItem.preview == Some(diff)` (unchanged from today).

## Out of Scope (YAGNI)

- Per-tool preview policy (one global policy is enough).
- Showing previews to the LLM (preview is human-facing only).
- Durable auto-run previews / `gait status --diffs` recomputation (recomputable later from edit args +
  codebase git; not built now).
- Truncation / size caps (consistent with the existing preview feature).

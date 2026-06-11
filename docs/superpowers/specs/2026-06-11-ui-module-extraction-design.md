# UI Module Extraction Design

**Date:** 2026-06-11
**Status:** approved

## Goal

Extract terminal UI concerns from `chat.rs` and `render.rs` into a focused `ui/` module, and pull pure input-processing helpers into `repl_input.rs`. The result is a `chat.rs` that is an orchestration loop, a `ui/` module that owns all terminal interaction, and a `repl_input.rs` that owns all pure input transforms.

## File Layout

```
src/
  ui/
    mod.rs          — pub use of all widgets
    renderer.rs     — moved from render.rs (rename only, no behavior change)
    timer.rs        — NEW: Timer, replaces inline braille spinner in chat.rs
    permission.rs   — extract from chat.rs: prompt_permission + parse_permission_response
    clarify.rs      — extract from chat.rs: prompt_clarify
    inform.rs       — extract from chat.rs: prompt_inform
  repl_input.rs     — extract from chat.rs: all pure input transforms
  render.rs         — DELETED (replaced by ui/renderer.rs)
  chat.rs           — keeps: run_chat, run_and_capture, seed_history_from_transcripts,
                      new_session_id (~400 lines after extraction)
  lib.rs            — add pub mod ui, pub mod repl_input; remove pub mod render
```

## Section 1: `ui/` module

### `ui/renderer.rs`

Move `render.rs` verbatim. No behavior changes. `lib.rs` replaces `pub mod render` with `pub mod ui`, and `ui/mod.rs` re-exports `Renderer` and `RenderAction`. Call sites in `chat.rs` update from `crate::render::` to `crate::ui::`.

### `ui/timer.rs`

Replaces the inline `AtomicBool` + braille-frame thread in `chat.rs`.

```rust
pub struct Timer {
    stop_flag: Arc<AtomicBool>,
    handle: JoinHandle<()>,
}

impl Timer {
    pub fn start() -> Self
    pub fn stop(self)
}
```

`start()` spawns a background thread that prints `\r  thinking… [Ns]` to stderr once per second (N = elapsed seconds). `stop()` sets the flag, joins the thread, then prints `\r\x1b[2K` to clear the line before any output appears.

`chat.rs` replaces the ~30-line spinner block with:
```rust
let timer = Timer::start();
// ... render loop ...
timer.stop();
```

### `ui/permission.rs`

Extracted from `chat.rs` — no behavior change:

```rust
pub fn prompt_permission(item: &PermissionItem, label: Option<&str>, theme: &Theme) -> PermissionChoice
fn parse_permission_response(input: &str, patterns: &[String]) -> Option<PermissionChoice>
```

The permission loop in `chat.rs` (iterating items, calling `save_choice`, building `reply_items`) stays in `chat.rs` — it is orchestration, not UI.

### `ui/clarify.rs`

```rust
pub fn prompt_clarify(items: &[ClarifyItem], theme: &Theme) -> Vec<ClarifyReplyItem>
```

### `ui/inform.rs`

```rust
pub fn prompt_inform(items: &[InformItem], theme: &Theme) -> Vec<InformReplyItem>
```

## Section 2: `repl_input.rs`

Pure functions, moved verbatim from `chat.rs`. No behavior changes.

```
expand_quote_reply(input, blocks) -> String
resolve_quote_range(range, blocks) -> Option<String>
expand_at_markers(input, history_path, codebase) -> anyhow::Result<String>
expand_shell_marker(inner, history_path, codebase) -> String
is_bang_command(s) -> bool
bang_command_str(s) -> &str
looks_like_path(token) -> bool
run_and_capture_pty_raw(cmd, cwd) -> CaptureResult   // primary consumer is expand_shell_marker
CaptureResult enum
```

`run_and_capture_pty_raw` moves here because `expand_shell_marker` is its primary caller. `chat.rs` imports it from `repl_input` for the `!(cmd)` bang path.

All tests for these functions move with them.

## Section 3: `chat.rs` after extraction

Keeps:
- `run_chat` — main REPL loop
- `run_and_capture` — simple subprocess capture, tightly coupled to the bang-command REPL flow
- `seed_history_from_transcripts` — loads history at startup
- `new_session_id` — session ID generation

Expected size: ~400 lines (down from 1033).

## What does NOT change

- `render.rs`/`ui/renderer.rs` behavior — rename only
- Permission loop logic in `chat.rs` — orchestration stays put
- All public function signatures — call sites in `bin/gait.rs` and tests are unaffected
- `repl_helper.rs` — not touched

## Testing

- All existing tests move with their functions (no orphaned tests)
- New test for `Timer`: verifies `stop()` clears the line (checks stderr output contains `\r\x1b[2K`)
- No behavior changes → existing test suite passes without modification

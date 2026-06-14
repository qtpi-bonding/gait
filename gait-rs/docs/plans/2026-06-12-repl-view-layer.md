---
status: done
---
# ReplBuilder Layer Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Introduce a principled three-layer terminal UI architecture — `ReplState` (Cubit / state holder), `ReplRenderer` (StatelessWidget / pure painter), `ReplBuilder` (BlocBuilder / coordinator) — eliminating the stdin-vs-rustyline ownership conflict and giving each piece a single clear responsibility.

**Architecture:**

```
Log  ──────────────────────────►  Projection  ──────────────────────────►  ReplBuilder
(log.jsonl)                       (retrace.rs)                              (this plan)
data model                        LLM view model                            terminal UI layer

                    analogous to Flutter/Bloc:
                    State ≈ log.jsonl
                    Cubit<S> ≈ ReplState     (pure state + dirty queue)
                    StatelessWidget ≈ ReplRenderer  (stateless painter, only fields: theme + skin)
                    BlocBuilder ≈ ReplBuilder       (thin coordinator)
                    ThemeData ≈ Theme        (immutable color/style config, owned by ReplRenderer)
```

`ReplState` is the Cubit — holds all mutable terminal UI state, exposes `apply_event`/`push_text` to mutate it, never does I/O. The dirty queue (`pending: Vec<PendingItem>`) is the `setState`→`build` analog: `apply_event` pushes items, `ReplRenderer::paint` drains them.

`ReplRenderer` is the StatelessWidget — fields are `theme: Theme` (the `ThemeData` analog, immutable color/style config) and `skin: MadSkin` (derived from theme at construction, also immutable). Its `paint(state)` method takes `&self` (no mutation of self).

`ReplBuilder` is the BlocBuilder — owns `rl: Editor` (rustyline, for input), `state: ReplState`, `renderer: ReplRenderer`. Sequences them: events → `state.apply_event` + `renderer.paint`, user input → `rl.readline()`. Owns `run_interaction` for clarify/inform/permission.

**Tech Stack:** Rust, `rustyline` (existing), `tokio` channels, `termimad`, existing `Theme`/`Timer` from `src/`.

---

## File Map

| File | Change | Why |
|------|--------|-----|
| `src/ui/state.rs` | **Create** | `ReplState`, `PendingItem`, `Pending`, `ViewSignal` |
| `src/ui/renderer.rs` | **Refactor** | Rename struct `ReplRenderer`; strip state fields; `paint` drains state's dirty queue |
| `src/ui/builder.rs` | **Create** | `ReplBuilder`, `InteractionOutcome` |
| `src/ui/mod.rs` | **Modify** | Export new types; drop old prompt exports |
| `src/ui/clarify.rs` | **Delete** | Absorbed into `ReplBuilder::run_interaction` |
| `src/ui/inform.rs` | **Delete** | Absorbed into `ReplBuilder::run_interaction` |
| `src/ui/permission.rs` | **Modify** | Keep `parse_permission_response` (pure, tested); delete `prompt_permission` |
| `src/chat.rs` | **Modify** | Replace drain+prompt loop with `builder.*` calls |

---

## Task 1: `ReplState` — the Cubit

**Files:**
- Create: `src/ui/state.rs`

### Context

**`PendingItem`** is the dirty-queue unit. It covers all things the renderer needs to paint — not just text blocks but also tool call spinners, tool results, and error lines. Since terminal output is append-only (unlike a display framebuffer), the dirty queue is a `Vec` that gets drained rather than a flag. The renderer paints only new items, preserving the append-only contract.

```rust
pub(crate) enum PendingItem {
    Block { content: String, block_num: usize },
    Spacing,                                     // blank line between paragraphs
    ToolCall { display: String },                // spinner line "  ⟳ name target(…)"
    ToolResult { ok: bool, name: String, elapsed_ms: Option<u64>, output: String },
    Error { message: String },
}
```

**Block-splitting logic:** port `push_text`/`push_line`/`append_to_block` exactly from `renderer.rs:236–243` and `renderer.rs:175–200`. The split is on single `\n` (one newline per line), with fence tracking. A blank line triggers `complete_block` (pushes `PendingItem::Block`) then `PendingItem::Spacing`. A fence-close triggers `complete_block` without spacing. This is the existing behavior — do not replace with double-newline splitting.

**Proto→transcript converters:** move `perm_to_tx`, `clarify_to_tx`, `inform_to_tx` from `renderer.rs:18–50` into `state.rs` as `pub(crate)` functions. Use them unchanged in `apply_event`.

**ToolCall.input:** `tc.input` is `Option<pbjson_types::Struct>`, not a JSON string. Use `tc.input.as_ref().and_then(|s| serde_json::to_value(s).ok())` (matches `renderer.rs:305–306`).

**Timer:** `ReplState` owns the `Timer`. Started in `begin_turn`, stopped (taken) when the first event arrives in `apply_event`. Stopping on the first event matches the existing `chat.rs` behavior (`timer.stop()` on every `rx.recv()`).

- [ ] **Step 1: Write failing tests**

Create `src/ui/state.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn view_signal_variants_are_distinct() {
        assert!(!matches!(ViewSignal::Continue, ViewSignal::TurnDone(_)));
        assert!(matches!(ViewSignal::TurnDone(vec![]), ViewSignal::TurnDone(_)));
    }

    #[test]
    fn pending_starts_none() {
        let state = ReplState::new();
        assert!(matches!(state.interaction_pending, Pending::None));
    }

    #[test]
    fn pending_items_empty_initially() {
        let state = ReplState::new();
        assert!(state.pending.is_empty());
    }

    #[test]
    fn push_text_single_newline_produces_block() {
        let mut state = ReplState::new();
        state.push_text("hello world\n");
        state.push_text("\n");   // blank line → complete_block + Spacing
        assert!(state.pending.iter().any(|p| matches!(p, PendingItem::Block { .. })));
        assert!(state.pending.iter().any(|p| matches!(p, PendingItem::Spacing)));
    }

    #[test]
    fn flush_buf_drains_partial_content() {
        let mut state = ReplState::new();
        state.push_text("partial");
        assert!(state.pending.is_empty(), "not flushed yet — no newline");
        state.flush_buf();
        assert!(state.pending.iter().any(|p| matches!(p, PendingItem::Block { .. })));
    }

    #[test]
    fn fence_block_treated_as_single_block() {
        let mut state = ReplState::new();
        state.push_text("```\ncode line\n```\n");
        let blocks: Vec<_> = state.pending.iter().filter(|p| matches!(p, PendingItem::Block { .. })).collect();
        assert_eq!(blocks.len(), 1, "fence should produce exactly one block");
    }

    #[test]
    fn has_interaction_false_initially() {
        let state = ReplState::new();
        assert!(!state.has_interaction());
    }

    #[test]
    fn clarify_kind_maps_correctly() {
        use crate::proto::gait::log::v1 as pb;
        use crate::proto::envelope;
        use crate::proto::events;
        use crate::session::transcript::ClarifyItem as txCI;
        let mut state = ReplState::new();
        let body = events::clarify(&[
            txCI { id: "1".into(), content: "c1".into(), why: "w1".into(), kind: "is".into() },
            txCI { id: "2".into(), content: "c2".into(), why: "w2".into(), kind: "ought".into() },
        ]);
        let ev = envelope::mint(body, None);
        state.apply_event(&ev);
        if let Pending::Clarify(items) = &state.interaction_pending {
            assert_eq!(items[0].kind, "is");
            assert_eq!(items[1].kind, "ought");
        } else {
            panic!("expected Pending::Clarify");
        }
    }
}
```

- [ ] **Step 2: Run to verify they fail**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test ui::state 2>&1 | tail -5
```
Expected: compile error — module doesn't exist yet.

- [ ] **Step 3: Implement `ReplState`**

```rust
use crate::proto::gait::log::v1 as pb;
use crate::session::transcript::{self as tx, ClarifyItem, InformItem, PermissionItem};
use crate::ui::timer::Timer;

// ── Dirty-queue item — the setState→build analog ──────────────────────────────

pub(crate) enum PendingItem {
    Block { content: String, block_num: usize },
    Spacing,
    ToolCall { display: String },
    ToolResult { ok: bool, name: String, elapsed_ms: Option<u64>, output: String },
    Error { message: String },
}

// ── Interaction pending (one-shot per turn) ───────────────────────────────────

pub(crate) enum Pending {
    None,
    Clarify(Vec<ClarifyItem>),
    Inform(Vec<InformItem>),
    Permission(Vec<PermissionItem>),
}

// ── ViewSignal ────────────────────────────────────────────────────────────────

pub enum ViewSignal {
    Continue,
    TurnDone(Vec<String>),
}

// ── Proto → transcript converters (moved from renderer.rs) ───────────────────

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
    }
}

pub(crate) fn clarify_to_tx(c: &pb::ClarifyItem) -> tx::ClarifyItem {
    tx::ClarifyItem {
        id: c.id.clone(),
        content: c.content.clone(),
        why: c.why.clone(),
        kind: match pb::ClarifyKind::try_from(c.kind) {
            Ok(pb::ClarifyKind::Is) => "is".to_string(),
            Ok(pb::ClarifyKind::Ought) => "ought".to_string(),
            _ => String::new(),
        },
    }
}

pub(crate) fn inform_to_tx(i: &pb::InformItem) -> tx::InformItem {
    tx::InformItem {
        id: i.id.clone(),
        content: i.content.clone(),
        context: i.context.clone(),
    }
}

// ── ReplState ─────────────────────────────────────────────────────────────────

pub struct ReplState {
    /// Dirty queue — drained by ReplRenderer::paint. The setState→build analog.
    pub(crate) pending: Vec<PendingItem>,
    /// All text blocks for the current turn (returned on TurnDone).
    pub(crate) all_blocks: Vec<String>,
    pub(crate) block_num: usize,
    // Streaming text buffers — matches Renderer.line_buf / block_buf exactly.
    pub(crate) line_buf: String,
    pub(crate) block_buf: String,
    pub(crate) in_fence: bool,
    pub(crate) pending_tool: Option<String>,
    /// One-shot interaction pending for this turn.
    pub(crate) interaction_pending: Pending,
    /// Thinking timer — started in begin_turn, stopped on first apply_event call.
    pub(crate) timer: Option<Timer>,
}

impl ReplState {
    pub fn new() -> Self {
        Self {
            pending: Vec::new(),
            all_blocks: Vec::new(),
            block_num: 0,
            line_buf: String::new(),
            block_buf: String::new(),
            in_fence: false,
            pending_tool: None,
            interaction_pending: Pending::None,
            timer: None,
        }
    }

    pub fn begin_turn(&mut self) {
        *self = Self::new();
        self.timer = Some(Timer::start());
    }

    // ── Streaming text — ported exactly from Renderer::push_text/push_line ────

    pub fn push_text(&mut self, content: &str) {
        self.line_buf.push_str(content);
        while let Some(pos) = self.line_buf.find('\n') {
            let line = self.line_buf[..pos].to_string();
            self.line_buf = self.line_buf[pos + 1..].to_string();
            self.push_line(&line);
        }
    }

    fn push_line(&mut self, line: &str) {
        let is_fence_marker = line.trim_start().starts_with("```");
        if self.in_fence {
            self.append_to_block(line);
            if is_fence_marker {
                self.in_fence = false;
                self.complete_block(false);
            }
        } else if line.is_empty() {
            self.complete_block(true); // true = add Spacing after
        } else {
            self.append_to_block(line);
            if is_fence_marker { self.in_fence = true; }
        }
    }

    fn append_to_block(&mut self, line: &str) {
        if !self.block_buf.is_empty() { self.block_buf.push('\n'); }
        self.block_buf.push_str(line);
    }

    fn complete_block(&mut self, with_spacing: bool) {
        if !self.block_buf.is_empty() {
            let block = std::mem::take(&mut self.block_buf);
            self.block_num += 1;
            self.all_blocks.push(block.clone());
            self.pending.push(PendingItem::Block { content: block, block_num: self.block_num });
        }
        if with_spacing {
            self.pending.push(PendingItem::Spacing);
        }
    }

    pub fn flush_buf(&mut self) {
        if !self.line_buf.is_empty() {
            let partial = std::mem::take(&mut self.line_buf);
            self.append_to_block(&partial);
        }
        self.complete_block(false);
        self.in_fence = false;
    }

    // ── Event dispatch ────────────────────────────────────────────────────────

    pub fn apply_event(&mut self, ev: &pb::Event) -> ViewSignal {
        // Stop the thinking timer on the first event.
        if let Some(mut t) = self.timer.take() { t.stop(); }

        let Some(pb::event::Body::Payload(p)) = &ev.body else {
            return ViewSignal::Continue;
        };
        match &p.kind {
            // AssistantText is the full accumulated turn text — already painted
            // via TextDelta stream items. Re-rendering here would double the output.
            Some(pb::payload::Kind::AssistantText(_)) => ViewSignal::Continue,

            Some(pb::payload::Kind::ToolCall(tc)) => {
                let args = tc.input.as_ref()
                    .and_then(|s| serde_json::to_value(s).ok());
                let target = args.as_ref()
                    .and_then(|a| crate::ui::renderer::tool_target(&tc.name, a))
                    .map(|t| format!(" {}", t))
                    .unwrap_or_default();
                let display = format!("{}{}", tc.name, target);
                self.pending_tool = Some(display.clone());
                self.pending.push(PendingItem::ToolCall { display });
                ViewSignal::Continue
            }
            Some(pb::payload::Kind::ToolResult(tr)) => {
                let elapsed_ms = tr.duration.as_ref()
                    .map(|d| (d.seconds as u64) * 1000 + (d.nanos as u64) / 1_000_000);
                let ok = tr.process.as_ref().map_or(true, |pr| pr.exit_code == 0);
                let name = self.pending_tool.take().unwrap_or_else(|| "?".to_string());
                self.pending.push(PendingItem::ToolResult { ok, name, elapsed_ms, output: tr.output.clone() });
                ViewSignal::Continue
            }
            Some(pb::payload::Kind::Permissions(pm)) => {
                self.flush_buf();
                self.interaction_pending = Pending::Permission(pm.items.iter().map(perm_to_tx).collect());
                ViewSignal::Continue
            }
            Some(pb::payload::Kind::Clarify(c)) => {
                self.flush_buf();
                self.interaction_pending = Pending::Clarify(c.items.iter().map(clarify_to_tx).collect());
                ViewSignal::Continue
            }
            Some(pb::payload::Kind::Inform(inf)) => {
                self.flush_buf();
                self.interaction_pending = Pending::Inform(inf.items.iter().map(inform_to_tx).collect());
                ViewSignal::Continue
            }
            Some(pb::payload::Kind::TurnComplete(tc)) => {
                self.flush_buf();
                let blocks = if tc.blocks.is_empty() {
                    self.all_blocks.clone()
                } else {
                    tc.blocks.clone()
                };
                ViewSignal::TurnDone(blocks)
            }
            Some(pb::payload::Kind::Error(e)) => {
                self.flush_buf();
                self.pending.push(PendingItem::Error { message: e.message.clone() });
                ViewSignal::Continue
            }
            _ => ViewSignal::Continue,
        }
    }

    pub fn has_interaction(&self) -> bool {
        !matches!(self.interaction_pending, Pending::None)
    }

    pub fn take_pending(&mut self) -> Pending {
        std::mem::replace(&mut self.interaction_pending, Pending::None)
    }
}
```

- [ ] **Step 4: Add `state` to `src/ui/mod.rs`**

Add (do not remove existing entries yet):
```rust
pub mod state;
pub use state::{ReplState, ViewSignal};
pub(crate) use state::Pending;
```

- [ ] **Step 5: Run tests**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test 2>&1 > /tmp/gait-test-output.txt; echo "exit: $?"
grep "FAILED\|test result\|^error" /tmp/gait-test-output.txt
```

- [ ] **Step 6: Commit**

```bash
git add src/ui/state.rs src/ui/mod.rs
git commit -m "feat(ui): add ReplState Cubit — PendingItem dirty queue, apply_event, push_text/push_line"
```

---

## Task 2: Refactor `Renderer` → `ReplRenderer` (stateless painter)

**Files:**
- Modify: `src/ui/renderer.rs`

### Context

**`ReplRenderer` fields:** `theme: Theme` and `skin: MadSkin`. Both are immutable after construction. `skin` is derived from `theme` via `themed_skin()` at construction — it is config, not state, so two fields is correct and the StatelessWidget analogy holds.

**What stays in `renderer.rs`:**
- `themed_skin` / `to_ccolor` (private helpers)
- `tool_target(name, &Value) -> Option<String>` — make `pub(crate)` (called from `state.rs`)
- `split_into_blocks` — keep as a standalone `pub` function (used by other callers)
- `render_tool_call`/`render_tool_result` logic — port into `paint` as private helpers, taking `&self` only

**What is deleted from `renderer.rs`:**
- `Renderer` struct (renamed `ReplRenderer`, state fields removed)
- `RenderAction` enum (replaced by `ViewSignal` in `state.rs`)
- `handle_event`, `push_text`, `push_line`, `append_to_block`, `render_block`, `flush` methods (state logic moved to `ReplState`)

**Test migration:** Before deleting old methods, migrate the `renderer.rs` tests that test `handle_event`/`RenderAction` to `state.rs` tests calling `ReplState::apply_event`. The tests that test `split_into_blocks` and `tool_target` stay in `renderer.rs`.

**`paint` implementation:** drains `state.pending` and dispatches each `PendingItem` variant. Port the exact formatting logic from `Renderer::render_block` (gutter + termimad, `renderer.rs:202–231`) and `render_tool_call`/`render_tool_result` (`renderer.rs:248–289`) — do not rewrite.

- [ ] **Step 1: Write failing tests for `ReplRenderer`**

Add to `src/ui/renderer.rs`:
```rust
#[cfg(test)]
mod replrenderer_tests {
    use super::*;
    use crate::ui::state::{ReplState, PendingItem};
    use crate::theme::Theme;

    #[test]
    fn paint_drains_pending() {
        let r = ReplRenderer::new(Theme::detect());
        let mut state = ReplState::new();
        state.pending.push(PendingItem::Block { content: "hello".into(), block_num: 1 });
        state.pending.push(PendingItem::Spacing);
        r.paint(&mut state);
        assert!(state.pending.is_empty(), "paint must drain all pending items");
    }

    #[test]
    fn paint_empty_state_is_noop() {
        let r = ReplRenderer::new(Theme::detect());
        let mut state = ReplState::new();
        r.paint(&mut state); // must not panic
        assert!(state.pending.is_empty());
    }

    #[test]
    fn paint_takes_ref_self_not_mut() {
        // Verify &self (not &mut self) by calling twice on same renderer.
        let r = ReplRenderer::new(Theme::detect());
        let mut state = ReplState::new();
        state.pending.push(PendingItem::Block { content: "a".into(), block_num: 1 });
        r.paint(&mut state);
        state.pending.push(PendingItem::Block { content: "b".into(), block_num: 2 });
        r.paint(&mut state);
        assert!(state.pending.is_empty());
    }
}
```

- [ ] **Step 2: Run to verify they fail**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test replrenderer_tests 2>&1 | tail -5
```

- [ ] **Step 3: Migrate `handle_event` tests to `state.rs`**

Move these tests from `renderer.rs` to `src/ui/state.rs` `mod tests`, updating them to call `ReplState::apply_event` (using `envelope::mint(body, None)` to construct events — the same pattern as the existing `renderer.rs` `ev()` helper):

- `handle_event_permissions_returns_permission_needed` → `apply_event_permissions_sets_pending`
- `handle_event_turn_complete_returns_blocks` → `apply_event_turn_complete_returns_done`
- `handle_event_tool_call_sets_pending` → `apply_event_tool_call_pushes_pending_item`
- `handle_event_tool_result_renders` → `apply_event_tool_result_pushes_pending_item`
- `handle_event_assistant_text_is_noop` → `apply_event_assistant_text_is_noop`
- `turn_blocks_populated_on_paragraph` → `push_text_paragraph_populates_all_blocks`
- `turn_blocks_numbered_sequentially` → `blocks_numbered_sequentially`

Keep in `renderer.rs`: `tool_target_extracts_per_tool`, `text_delta_accumulates_and_flushes_on_newline`, `split_into_blocks` tests, `turn_complete_flushes_remaining_buffer`.

- [ ] **Step 4: Rename struct and implement stateless `paint`**

Replace `pub struct Renderer` with:

```rust
use crate::ui::state::{ReplState, PendingItem};

/// Stateless terminal painter — the StatelessWidget in the Flutter/Bloc analogy.
/// Fields: theme (ThemeData analog) and skin (derived from theme at construction).
/// paint() takes &self — no mutable fields.
pub struct ReplRenderer {
    pub theme: Theme,
    skin: MadSkin,
}

impl ReplRenderer {
    pub fn new(theme: Theme) -> Self {
        let skin = themed_skin(&theme);
        Self { theme, skin }
    }

    pub fn theme(&self) -> &Theme { &self.theme }

    /// Drain all pending items from state and paint them to the terminal.
    pub fn paint(&self, state: &mut ReplState) {
        for item in state.pending.drain(..) {
            match item {
                PendingItem::Block { content, block_num } => {
                    self.paint_block(&content, block_num);
                }
                PendingItem::Spacing => { println!(); }
                PendingItem::ToolCall { display } => {
                    eprintln!("{}  \u{27f3} {}(\u{2026}){}", DIM_FG, display, RESET);
                }
                PendingItem::ToolResult { ok, name, elapsed_ms, output } => {
                    self.paint_tool_result(ok, &name, elapsed_ms, &output);
                }
                PendingItem::Error { message } => {
                    eprintln!("{}[error] {}{}", self.theme.danger.fg(), message, RESET);
                }
            }
        }
    }

    fn paint_block(&self, block: &str, block_num: usize) {
        // Port exactly from Renderer::render_block (renderer.rs:202–231).
        let term_w = termimad::terminal_size().0 as usize;
        let content_w = term_w.saturating_sub(6);
        let fmt = self.skin.text(block, Some(content_w));
        let rendered = format!("{}", fmt);
        let mut first = true;
        for line in rendered.lines() {
            if first {
                print!("{}{:>3}\u{2502}{} {}", DIM_FG, block_num, RESET, line);
                first = false;
            } else {
                print!("     {}", line);
            }
            println!();
        }
        if first {
            println!("{}{:>3}\u{2502}{}", DIM_FG, block_num, RESET);
        }
        let _ = std::io::stdout().flush();
    }

    fn paint_tool_result(&self, ok: bool, name: &str, elapsed_ms: Option<u64>, output: &str) {
        // Port exactly from Renderer::render_tool_result (renderer.rs:260–289).
        // Note: the erase-spinner line ("\x1b[1A\x1b[2K") is omitted here — the
        // spinner was printed as a PendingItem::ToolCall; clearing it requires
        // tracking whether the spinner was the last thing printed. For now, the
        // spinner is just overprinted. A follow-up can add cursor-up erase if desired.
        let (color, mark) = if ok {
            (self.theme.success.fg(), '\u{2713}')
        } else {
            (self.theme.danger.fg(), '\u{2717}')
        };
        let timing = elapsed_ms.map(|ms| format!(" \u{b7} {}ms", ms)).unwrap_or_default();
        eprintln!("{}  {} {}{}{}", color, mark, name, timing, RESET);
        let lines: Vec<&str> = output.lines().filter(|l| !l.trim().is_empty()).collect();
        match lines.len() {
            0 => {}
            1 => eprintln!("{}     {}{}", DIM_FG, lines[0], RESET),
            2 => {
                eprintln!("{}     {}{}", DIM_FG, lines[0], RESET);
                eprintln!("{}     {}{}", DIM_FG, lines[1], RESET);
            }
            n => {
                eprintln!("{}     {}{}", DIM_FG, lines[0], RESET);
                eprintln!("{}     \u{2026} ({} lines){}", DIM_FG, n - 2, RESET);
                eprintln!("{}     {}{}", DIM_FG, lines[n - 1], RESET);
            }
        }
    }
}
```

Make `tool_target` `pub(crate)`:
```rust
pub(crate) fn tool_target(name: &str, args: &serde_json::Value) -> Option<String> {
    // unchanged body
}
```

Delete: `Renderer` struct, `RenderAction` enum, `handle_event`, `push_text`, `push_line`, `append_to_block`, `render_block`, `render_tool_call`, `render_tool_result`, `flush` methods.

Keep: `themed_skin`, `to_ccolor`, `split_into_blocks`, `tool_target`.

- [ ] **Step 5: Run all tests**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test 2>&1 > /tmp/gait-test-output.txt; echo "exit: $?"
grep "FAILED\|test result\|^error" /tmp/gait-test-output.txt
```
Expected: all pass.

- [ ] **Step 6: Commit**

```bash
git add src/ui/renderer.rs src/ui/state.rs
git commit -m "refactor(ui): ReplRenderer stateless — theme+skin only; state in ReplState"
```

---

## Task 3: `ReplBuilder` — the BlocBuilder coordinator

**Files:**
- Create: `src/ui/builder.rs`

### Context

`ReplBuilder` is the thin coordinator. It owns three things that take turns with the terminal:
- `rl: Editor<ReplHelper, DefaultHistory>` — active only during `readline()` (rustyline raw mode)
- `state: ReplState` — mutated by `handle_stream_item` during turn streaming
- `renderer: ReplRenderer` — called by `handle_stream_item` and `flush()` to drain `state.pending`

rustyline and `ReplRenderer` are orthogonal — rustyline owns the terminal for input (`readline()`), renderer writes to stderr/stdout for output between readline calls. They never overlap: turn streaming happens after `readline()` returns and before the next `readline()` call.

`InteractionOutcome` carries the reply event body back to `chat.rs` for appending to the log. `chat.rs` calls `wire::append` — `ReplBuilder` never writes to the log.

- [ ] **Step 1: Write failing tests**

Create `src/ui/builder.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interaction_outcome_fields() {
        let outcome = InteractionOutcome {
            event_body: crate::proto::events::inform_reply(&[] as &[crate::session::transcript::InformReplyItem]),
            respawn: false,
        };
        assert!(!outcome.respawn);
    }

    #[test]
    fn has_interaction_false_initially() {
        let state = crate::ui::state::ReplState::new();
        assert!(!state.has_interaction());
    }
}
```

- [ ] **Step 2: Run to verify they fail**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test ui::builder 2>&1 | tail -5
```

- [ ] **Step 3: Implement `ReplBuilder` skeleton**

```rust
use rustyline::Editor;
use rustyline::history::DefaultHistory;
use rustyline::error::ReadlineError;

use crate::repl_helper::ReplHelper;
use crate::proto::gait::log::v1 as pb;
use crate::ui::state::{ReplState, ViewSignal};
use crate::ui::renderer::ReplRenderer;
use crate::theme::Theme;

pub struct InteractionOutcome {
    pub event_body: pb::event::Body,
    pub respawn: bool,
}

pub struct ReplBuilder {
    pub rl: Editor<ReplHelper, DefaultHistory>,
    state: ReplState,
    renderer: ReplRenderer,
}

impl ReplBuilder {
    pub fn new(rl: Editor<ReplHelper, DefaultHistory>, theme: Theme) -> Self {
        Self {
            rl,
            state: ReplState::new(),
            renderer: ReplRenderer::new(theme),
        }
    }

    pub fn begin_turn(&mut self) {
        self.state.begin_turn();
    }

    pub fn handle_stream_item(&mut self, item: pb::StreamItem) -> ViewSignal {
        use pb::stream_item::Item;
        let signal = match item.item {
            Some(Item::Event(ev)) => self.state.apply_event(&ev),
            Some(Item::Delta(d)) => {
                self.state.push_text(&d.content);
                ViewSignal::Continue
            }
            None => ViewSignal::Continue,
        };
        self.renderer.paint(&mut self.state);
        signal
    }

    pub fn flush(&mut self) {
        self.state.flush_buf();
        self.renderer.paint(&mut self.state);
    }

    pub fn has_interaction(&self) -> bool {
        self.state.has_interaction()
    }

    pub fn take_turn_blocks(&mut self) -> Vec<String> {
        std::mem::take(&mut self.state.all_blocks)
    }

    pub fn readline(&mut self, prompt: &str) -> Result<String, ReadlineError> {
        self.rl.readline(prompt)
    }

    pub fn add_history_entry(&mut self, line: &str) {
        let _ = self.rl.add_history_entry(line);
    }
}
```

- [ ] **Step 4: Export from `src/ui/mod.rs`**

Add (do not remove existing entries yet):
```rust
pub mod builder;
pub use builder::{ReplBuilder, InteractionOutcome};
```

- [ ] **Step 5: Run all tests**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test 2>&1 | tail -10
```

- [ ] **Step 6: Commit**

```bash
git add src/ui/builder.rs src/ui/mod.rs
git commit -m "feat(ui): add ReplBuilder skeleton (BlocBuilder coordinator)"
```

---

## Task 4: `run_interaction` — Clarify and Inform

**Files:**
- Modify: `src/ui/builder.rs`

### Context

All user input during a turn must go through `self.rl.readline()` — never raw `stdin`. This is the core bug fix. `Theme` colors for prompts come from `self.renderer.theme()` — the theme lives on the renderer as the `ThemeData` analog.

Use `events::clarify_reply` and `events::inform_reply` from `src/proto/events.rs` directly. Do not hand-roll proto fields.

- [ ] **Step 1: Write failing tests**

Add to `src/ui/builder.rs` `mod tests`:
```rust
#[test]
fn clarify_reply_event_has_correct_kind() {
    use crate::session::transcript::ClarifyReplyItem;
    use crate::proto::gait::log::v1::payload;
    let replies = vec![ClarifyReplyItem { id: "q1".into(), content: "answer".into(), reasoning: None }];
    let body = crate::proto::events::clarify_reply(&replies);
    if let pb::event::Body::Payload(p) = body {
        assert!(matches!(p.kind, Some(payload::Kind::ClarifyReply(_))));
    } else { panic!("expected Payload body"); }
}

#[test]
fn inform_reply_event_has_correct_kind() {
    use crate::session::transcript::InformReplyItem;
    use crate::proto::gait::log::v1::payload;
    let replies = vec![InformReplyItem { id: "i1".into(), content: "ok".into() }];
    let body = crate::proto::events::inform_reply(&replies);
    if let pb::event::Body::Payload(p) = body {
        assert!(matches!(p.kind, Some(payload::Kind::InformReply(_))));
    } else { panic!("expected Payload body"); }
}
```

- [ ] **Step 2: Run to verify they pass (pure builders, no rustyline needed)**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test clarify_reply_event inform_reply_event 2>&1 | tail -5
```

- [ ] **Step 3: Implement `run_interaction`, `run_clarify`, `run_inform`**

Add to `src/ui/builder.rs`:
```rust
use crate::session::transcript::{
    ClarifyItem, ClarifyReplyItem, InformItem, InformReplyItem,
    PermissionItem, PermissionsReplyItem,
};
use crate::theme::{RESET, DIM_FG};
use crate::proto::events;
use crate::ui::state::Pending;

impl ReplBuilder {
    pub fn run_interaction(
        &mut self,
        gait_dir: &std::path::Path,
        session_id: &str,
        agent_path: Option<&std::path::Path>,
    ) -> anyhow::Result<InteractionOutcome> {
        match self.state.take_pending() {
            Pending::Clarify(items)    => self.run_clarify(items),
            Pending::Inform(items)     => self.run_inform(items),
            Pending::Permission(items) => self.run_permission(items, gait_dir, session_id, agent_path),
            Pending::None => anyhow::bail!("run_interaction called with no pending interaction"),
        }
    }

    fn run_clarify(&mut self, items: Vec<ClarifyItem>) -> anyhow::Result<InteractionOutcome> {
        let prompt = format!("{}>{} ", self.renderer.theme().accent.fg(), RESET);
        let mut replies = Vec::new();
        for item in &items {
            eprintln!();
            eprintln!("{}? [{}] {}{}", self.renderer.theme().accent.fg(), item.kind, item.content, RESET);
            eprintln!("  {}why: {}{}", DIM_FG, item.why, RESET);
            let input = self.rl.readline(&prompt).unwrap_or_default().trim().to_string();
            replies.push(ClarifyReplyItem { id: item.id.clone(), content: input, reasoning: None });
        }
        Ok(InteractionOutcome { event_body: events::clarify_reply(&replies), respawn: true })
    }

    fn run_inform(&mut self, items: Vec<InformItem>) -> anyhow::Result<InteractionOutcome> {
        let prompt = format!("{}>{} ", self.renderer.theme().accent.fg(), RESET);
        let mut replies = Vec::new();
        for item in &items {
            eprintln!();
            eprintln!("{}→ {}{}", self.renderer.theme().accent.fg(), item.content, RESET);
            if let Some(ctx) = &item.context {
                eprintln!("  {}context: {}{}", DIM_FG, ctx, RESET);
            }
            eprintln!("  {}(Enter to continue, or type a correction){}", DIM_FG, RESET);
            let input = self.rl.readline(&prompt).unwrap_or_default().trim().to_string();
            let content = if input.is_empty() { "understood".into() } else { input };
            replies.push(InformReplyItem { id: item.id.clone(), content });
        }
        Ok(InteractionOutcome { event_body: events::inform_reply(&replies), respawn: true })
    }

    fn run_permission(
        &mut self,
        _items: Vec<PermissionItem>,
        _gait_dir: &std::path::Path,
        _session_id: &str,
        _agent_path: Option<&std::path::Path>,
    ) -> anyhow::Result<InteractionOutcome> {
        anyhow::bail!("run_permission not yet implemented — see Task 5")
    }
}
```

- [ ] **Step 4: Run all tests**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test 2>&1 > /tmp/gait-test-output.txt; echo "exit: $?"
grep "FAILED\|test result\|^error" /tmp/gait-test-output.txt
```

- [ ] **Step 5: Commit**

```bash
git add src/ui/builder.rs src/ui/renderer.rs
git commit -m "feat(ui): ReplBuilder::run_interaction for clarify and inform"
```

---

## Task 5: `run_permission`

**Files:**
- Modify: `src/ui/builder.rs`
- Modify: `src/ui/permission.rs`

### Context

Keep `parse_permission_response` in `permission.rs` (pure function, well-tested). Delete only `prompt_permission` (uses raw `stdin`).

**Use `events::permissions_reply`** from `src/proto/events.rs`:
```rust
pub fn permissions_reply(from: String, items: &[tx::PermissionsReplyItem]) -> pb::event::Body
```
Do NOT hand-roll proto fields — `decision`/`scope` are `i32` enums in the proto; this function handles the conversion.

**Single write path:** return the event body in `InteractionOutcome`. `chat.rs` calls `wire::append`. Do NOT call `transcript::write_permissions_reply` inside `run_permission` — that produces a duplicate event.

- [ ] **Step 1: Write failing test**

Add to `src/ui/builder.rs` `mod tests`:
```rust
#[test]
fn permissions_reply_event_has_correct_kind() {
    use crate::session::transcript::PermissionsReplyItem;
    use crate::proto::gait::log::v1::payload;
    let replies = vec![PermissionsReplyItem {
        id: "tc-1".into(), decision: "deny".into(), text: None, scope: None,
    }];
    let body = crate::proto::events::permissions_reply("user".into(), &replies);
    if let pb::event::Body::Payload(p) = body {
        assert!(matches!(p.kind, Some(payload::Kind::PermissionsReply(_))));
    } else { panic!("expected Payload body"); }
}
```

- [ ] **Step 2: Run to verify it passes (tests existing builder)**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test permissions_reply_event 2>&1 | tail -5
```

- [ ] **Step 3: Implement `run_permission`**

Replace the stub in `src/ui/builder.rs`:
```rust
fn run_permission(
    &mut self,
    items: Vec<PermissionItem>,
    gait_dir: &std::path::Path,
    session_id: &str,
    agent_path: Option<&std::path::Path>,
) -> anyhow::Result<InteractionOutcome> {
    use crate::ui::permission::parse_permission_response;
    let prompt = format!("  {}>{} ", self.renderer.theme().accent.fg(), RESET);
    let mut reply_items: Vec<PermissionsReplyItem> = vec![];

    for (idx, item) in items.iter().enumerate() {
        eprintln!();
        if items.len() > 1 {
            eprintln!("{}⟳ {}  {}({} of {}){}", self.renderer.theme().accent.fg(), item.cmd, DIM_FG, idx + 1, items.len(), RESET);
        } else {
            eprintln!("{}⟳ {}{}", self.renderer.theme().accent.fg(), item.cmd, RESET);
        }
        eprintln!();
        for (i, pat) in item.patterns.iter().enumerate() {
            eprintln!("  {}[{}] {}{}", DIM_FG, i + 1, pat, RESET);
        }
        eprintln!();
        eprintln!("  {}number = once · s = session · a = agent · r = reject{}", DIM_FG, RESET);

        let choice = loop {
            match self.rl.readline(&prompt) {
                Ok(input) => match parse_permission_response(input.trim(), &item.patterns) {
                    Some(c) => break c,
                    None => eprintln!("  unrecognized — e.g. 1, s, s1, 1a, r"),
                },
                Err(_) => break crate::permission::PermissionChoice::Reject,
            }
        };

        if let Err(e) = crate::permission::save_choice(&choice, gait_dir, session_id, agent_path) {
            eprintln!("[gait] warning: could not save permission: {e}");
        }

        let perm = match &choice {
            crate::permission::PermissionChoice::Once(p) =>
                PermissionsReplyItem { id: item.id.clone(), decision: "approve".into(), text: Some(p.clone()), scope: Some("once".into()) },
            crate::permission::PermissionChoice::Session(p) =>
                PermissionsReplyItem { id: item.id.clone(), decision: "approve".into(), text: Some(p.clone()), scope: Some("session".into()) },
            crate::permission::PermissionChoice::Agent(p) =>
                PermissionsReplyItem { id: item.id.clone(), decision: "approve".into(), text: Some(p.clone()), scope: Some("agent".into()) },
            crate::permission::PermissionChoice::Global(p) =>
                PermissionsReplyItem { id: item.id.clone(), decision: "approve".into(), text: Some(p.clone()), scope: Some("global".into()) },
            crate::permission::PermissionChoice::Reject =>
                PermissionsReplyItem { id: item.id.clone(), decision: "deny".into(), text: None, scope: None },
        };
        reply_items.push(perm);
    }

    // Always respawn — model must see both approvals and denials.
    Ok(InteractionOutcome {
        event_body: events::permissions_reply("user".into(), &reply_items),
        respawn: true,
    })
}
```

- [ ] **Step 4: Make `parse_permission_response` `pub(crate)` in `src/ui/permission.rs`**

The function is currently private (`fn parse_permission_response`). It's called cross-module from `builder.rs` and re-exported from `mod.rs`, so it must be visible:

```rust
pub(crate) fn parse_permission_response(input: &str, patterns: &[String]) -> Option<PermissionChoice> {
    // unchanged body
}
```

Also delete the `prompt_permission` function. Keep `parse_permission_response` and its tests.

- [ ] **Step 5: Run all tests**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test 2>&1 > /tmp/gait-test-output.txt; echo "exit: $?"
grep "FAILED\|test result\|^error" /tmp/gait-test-output.txt
```

- [ ] **Step 6: Commit**

```bash
git add src/ui/builder.rs src/ui/permission.rs
git commit -m "feat(ui): run_permission via rustyline; delete prompt_permission"
```

---

## Task 6: Wire `ReplBuilder` into `chat.rs`

**Files:**
- Modify: `src/chat.rs`

### Context

rustyline is constructed in `chat.rs` (the `nav`/`bind_sequence` wiring is REPL-specific). `seed_history_from_transcripts` runs before `ReplBuilder::new(rl, theme)` consumes `rl`. After construction, all rustyline calls go through `builder.readline()` / `builder.add_history_entry()`.

`chat.rs` still owns `theme` directly for the REPL prompt (`theme.accent.fg()`) — this is fine; `Theme` is `Clone`.

Single write path: `wire::append(&tx_path, outcome.event_body)` for all reply types (clarify, inform, permission). No other log-write call in this path.

- [ ] **Step 1: Verify `new_session_id` test still passes**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test chat::tests 2>&1 | tail -5
```

- [ ] **Step 2: Replace the drain+prompt section of `chat.rs`**

Find the existing rustyline construction and `'spawn: loop` in `chat.rs`. Replace:

```rust
// Before REPL loop — seed then hand to builder:
let mut rl = Editor::<ReplHelper, DefaultHistory>::new()?;
seed_history_from_transcripts(&mut rl, &gait_dir);
rl.set_helper(Some(ReplHelper { nav: nav.clone() }));
rl.bind_sequence(KeyEvent(KeyCode::Up, Modifiers::SHIFT),
    EventHandler::Conditional(Box::new(ShellNavHandler { nav: nav.clone(), up: true })));
rl.bind_sequence(KeyEvent(KeyCode::Down, Modifiers::SHIFT),
    EventHandler::Conditional(Box::new(ShellNavHandler { nav: nav.clone(), up: false })));
let mut builder = ReplBuilder::new(rl, theme.clone());
let mut last_turn_blocks: Vec<String> = Vec::new();

// Outer REPL loop:
loop {
    let prompt = format!("{}>{} {}", theme.accent.fg(), RESET, theme.user_msg.fg());
    let line = match builder.readline(&prompt) {
        Ok(l) => { eprint!("{}", RESET); l.trim().to_string() }
        Err(ReadlineError::Eof) | Err(ReadlineError::Interrupted) => break,
        Err(e) => return Err(e.into()),
    };
    if line.is_empty() { continue; }
    // ... slash commands, bang commands, undo (unchanged) ...

    builder.add_history_entry(&line);
    // ... expand_input (unchanged) ...

    'spawn: loop {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<pb::StreamItem>();
        // ... set up emitter, profile, caps, turn_input (unchanged) ...
        let turn_handle = tokio::spawn(async move { run_turn(...).await });

        builder.begin_turn();
        let mut turn_done_blocks: Option<Vec<String>> = None;

        while let Some(item) = rx.recv().await {
            match builder.handle_stream_item(item) {
                ViewSignal::TurnDone(blocks) => { turn_done_blocks = Some(blocks); }
                ViewSignal::Continue => {}
            }
        }
        builder.flush();

        match turn_handle.await {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => { eprintln!("[gait] turn error: {e}"); break 'spawn; }
            Err(e)    => { eprintln!("[gait] turn panicked: {e}"); break 'spawn; }
        }

        last_turn_blocks = turn_done_blocks
            .unwrap_or_else(|| builder.take_turn_blocks());

        if builder.has_interaction() {
            let tx_path = gait_dir.join("sessions").join(&session_id).join("log.jsonl");
            let outcome = builder.run_interaction(&gait_dir, &session_id, agent_path.as_deref())?;
            wire::append(&tx_path, outcome.event_body)?;
            if outcome.respawn { continue 'spawn; }
        }

        println!();
        break 'spawn;
    }
}
```

- [ ] **Step 3: Run all tests**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test 2>&1 > /tmp/gait-test-output.txt; echo "exit: $?"
grep "FAILED\|test result\|^error" /tmp/gait-test-output.txt
```

- [ ] **Step 4: Commit**

```bash
git add src/chat.rs
git commit -m "refactor(chat): delegate drain and interactions to ReplBuilder"
```

---

## Task 7: Delete `clarify.rs`, `inform.rs`; finalize exports

**Files:**
- Delete: `src/ui/clarify.rs`
- Delete: `src/ui/inform.rs`
- Modify: `src/ui/mod.rs`

- [ ] **Step 1: Verify no remaining references**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && grep -r "ui::clarify\|ui::inform\|prompt_clarify\|prompt_inform" src/ --include="*.rs"
```
Expected: no matches.

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && grep -r "RenderAction\|use crate::ui::renderer::Renderer\b" src/ --include="*.rs"
```
Expected: no matches (confirm chat.rs and any other callers have been updated).

- [ ] **Step 2: Delete the files**

```bash
rm /Users/aicoder/Documents/ideation/gait/gait-rs/src/ui/clarify.rs
rm /Users/aicoder/Documents/ideation/gait/gait-rs/src/ui/inform.rs
```

- [ ] **Step 3: Final `src/ui/mod.rs`**

```rust
pub mod state;
pub mod renderer;
pub mod builder;
pub mod timer;
pub mod permission;

pub use state::{ReplState, ViewSignal};
pub use renderer::{ReplRenderer, split_into_blocks};
pub use builder::{ReplBuilder, InteractionOutcome};
pub use timer::Timer;
pub use permission::parse_permission_response;
```

- [ ] **Step 4: Run all tests**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test 2>&1 > /tmp/gait-test-output.txt; echo "exit: $?"
grep "FAILED\|test result\|^error" /tmp/gait-test-output.txt
```

- [ ] **Step 5: Commit**

```bash
git add -A src/ui/
git commit -m "refactor(ui): delete clarify.rs inform.rs; finalize mod.rs exports"
```

---

## Self-Review

**Architecture:**
- ✅ `ReplState` (Cubit) — pure state, no I/O, unit-testable; `PendingItem` dirty queue is `setState`→`build` analog
- ✅ `ReplRenderer` (StatelessWidget) — fields: `theme: Theme` (ThemeData) + `skin: MadSkin` (derived from theme, immutable config); `paint` takes `&self`
- ✅ `ReplBuilder` (BlocBuilder) — sequences rustyline (input) + state (mutation) + renderer (output)
- ✅ `Theme` = Flutter `ThemeData` — immutable, detected once, owned by `ReplRenderer`
- ✅ Timer in `ReplState` — started in `begin_turn`, stopped on first `apply_event` call
- ✅ Block-splitting: `push_text`/`push_line`/fence tracking ported exactly from `Renderer` (single `\n`, not `\n\n`)
- ✅ Proto converters: `perm_to_tx`/`clarify_to_tx`/`inform_to_tx` moved to `state.rs` unchanged
- ✅ `ToolCall.input` handled correctly: `tc.input.as_ref().and_then(|s| serde_json::to_value(s).ok())`
- ✅ `events::permissions_reply` used — no hand-rolled proto fields
- ✅ Single write path — `wire::append` in `chat.rs` only
- ✅ Spinner erase omitted intentionally — spinner and timer animations are not currently functional; plain ✓/✗ output is correct behavior for now

**Verification points:**
- `tool_target` in `renderer.rs` must be `pub(crate)` before `state.rs` calls it
- `split_into_blocks` stays `pub` — other callers (`retrace.rs` or tests) may use it
- `ReplBuilder.rl` is `pub` — only needed so `seed_history_from_transcripts` can call `rl.add_history_entry` before construction. After `ReplBuilder::new` consumes it, all access is through `builder.readline()`. Can be made `pub(crate)` if `seed_history_from_transcripts` moves inside `builder.rs`.

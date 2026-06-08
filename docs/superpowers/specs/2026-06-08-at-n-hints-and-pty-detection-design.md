# Design: @(N) Ghost-Text Hints + PTY Interactive Command Detection

**Date:** 2026-06-08  
**Branch:** feat/at-n-hints-pty-detection  
**Status:** Approved

## Overview

Two related improvements to the `@(...)` expansion system in the gait REPL:

1. **@(N) ghost-text hint** — while composing a prompt, show which shell history entries `@(1)`, `@(2)` etc. refer to, as Fish-shell-style dim ghost text after the cursor.
2. **PTY interactive command detection** — when `@(command)` is used with a non-numeric command, spawn it inside a PTY and detect alternate-screen entry (`\x1b[?1049h`) to safely abort TUI programs before they take over the terminal.

## Feature 1: @(N) Ghost-Text Hint

### Goal

The user types a prompt like `explain @(2) output` and sees ghost text after the cursor showing what `@(2)` is — without having to remember which shell command they ran recently.

### Architecture

**New struct: `ReplHelper`**

Located in `chat.rs` or extracted to `repl_helper.rs`. Holds:
- `shell_history_path: PathBuf`

Implements the full rustyline helper suite:
- `rustyline::Helper` (marker trait)
- `rustyline::hint::Hinter` (the active trait)
- `rustyline::completion::Completer` (no-op)
- `rustyline::highlight::Highlighter` (no-op)
- `rustyline::validate::Validator` (no-op)

**Editor change:**

```rust
// Before
let mut rl = DefaultEditor::new()?;

// After
let helper = ReplHelper { shell_history_path: shell_history_path.clone() };
let mut rl = Editor::<ReplHelper, DefaultHistory>::new()?;
rl.set_helper(Some(helper));
```

**Hinter logic (`hint()`):**

Fires on every keypress. Scans the current line for complete `@(\d+)` patterns (char-by-char, same approach as `expand_at_markers`). For each found N, calls `shell_history::recall(&self.shell_history_path, n)`. Collects the `cmd` field from each entry, deduplicates while preserving order, and returns:

```
  ← cargo test · cat src/server.ts
```

Only returns `Some(hint)` when at least one complete `@(N)` exists in the line. Returns `None` otherwise (no ghost text).

Rustyline renders the hint string dim/gray automatically — no manual ANSI needed.

**Trigger condition:**

Hint appears after the closing `)` of a complete `@(N)` token. Partial tokens like `@(2` produce no hint — this avoids flickering when typing multi-digit indices like `@(22)`.

**Shell history path timing:**

The `shell_history_path` can rotate mid-session (new session ID on `/new`). When the session rotates, call `rl.helper_mut().map(|h| h.shell_history_path = new_path)` to update the helper in place — no shared state needed.

### What does not change

- `shell_history` module — read-only from the hint, no writes
- `expand_at_markers` — unchanged
- `@(N)` numeric expansion — unchanged
- `@path` file expansion — unchanged

---

## Feature 2: PTY + Alternate Screen Detection

### Goal

`@(top)`, `@(vim foo.rs)`, `@(less log.txt)` should fail gracefully with a clear message instead of hanging or flooding the REPL with escape codes.

### Detection Approach

The definitive signal that a program is a full-screen TUI is the alternate screen entry escape sequence:

```
\x1b[?1049h
```

This is emitted by vim, nvim, top, htop, less, tmux, fzf, and every ncurses-based app in their first output. Regular commands (`cargo test`, `grep`, `ls`, `cat`) never emit it.

**Limitation (accepted):** Some interactive programs that don't use alternate screen (`python` REPL, `psql`, `ssh` password prompt) won't be caught. These are handled by Ctrl+C — documented behavior, not a bug.

### Architecture

**New fn `run_and_capture_pty()`:**

```rust
enum CaptureResult {
    Output(String, Option<i32>),
    Interactive,
}

fn run_and_capture_pty(cmd: &str, cwd: &Path) -> CaptureResult
```

Uses `portable-pty` crate to spawn the child with a real PTY (so `isatty()` returns true and the program behaves normally). Reads output bytes in a loop. If `\x1b[?1049h` appears within the first 4096 bytes of output, kills the child and returns `CaptureResult::Interactive`. Otherwise captures all output and returns `CaptureResult::Output`.

Output is still streamed to stdout line-by-line as with the existing `run_and_capture`.

**`expand_paren_marker` change:**

Non-numeric tokens currently call `run_and_capture`. They will call `run_and_capture_pty` instead. On `Interactive`:

```
[interactive TUI — run @(top) directly in your terminal]
```

Numeric tokens (`@(1)`, `@(2)`) are unaffected — they're pure history lookups, no execution.

**New dependency:**

```toml
portable-pty = "0.8"
```

### What does not change

- `run_and_capture` — kept for internal use / tests
- `shell_history` module — unchanged
- `expand_at_markers` — unchanged

---

## Out of Scope

- Timeout for long-running commands (user Ctrl+Cs if needed)
- Denylist of known interactive command names
- Override syntax (`@!(cmd)` / `@tui(cmd)`)
- seccomp/DTrace syscall-level detection

---

## Files Affected

| File | Change |
|------|--------|
| `gait-rs/src/chat.rs` | Add `ReplHelper`, swap `DefaultEditor`, add `run_and_capture_pty` |
| `gait-rs/src/repl_helper.rs` | New file (if extracted from chat.rs) |
| `gait-rs/Cargo.toml` | Add `portable-pty = "0.8"` |

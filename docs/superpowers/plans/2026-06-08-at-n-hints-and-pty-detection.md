# @(N) Ghost-Text Hints + PTY Interactive Command Detection — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Show Fish-shell-style ghost text while composing prompts with `@(N)` references, and safely abort TUI commands (top, vim, less) when invoked via `@(command)`.

**Architecture:** A new `ReplHelper` struct implements rustyline's `Hinter` trait to show dim ghost text listing which shell commands `@(1)`, `@(2)` etc. refer to. A new `run_and_capture_pty` function spawns commands inside a PTY and aborts if it detects `\x1b[?1049h` (alternate screen entry) in the first 4096 bytes of output.

**Tech Stack:** Rust, rustyline 14, portable-pty 0.8

---

## File Map

| File | Action | Responsibility |
|------|--------|----------------|
| `gait-rs/src/repl_helper.rs` | Create | `ReplHelper` struct + all rustyline helper trait impls + `collect_at_refs` |
| `gait-rs/src/chat.rs` | Modify | Swap `DefaultEditor` → `Editor<ReplHelper, DefaultHistory>`, update path on `/new`, add `run_and_capture_pty`, update `expand_paren_marker` |
| `gait-rs/Cargo.toml` | Modify | Add `portable-pty = "0.8"` |

---

## Task 1: Create branch and worktree

**Files:** none

- [ ] **Step 1: Create branch and worktree**

```bash
git checkout -b feat/at-n-hints-pty-detection
git worktree add ../gait-hints feat/at-n-hints-pty-detection
cd ../gait-hints
```

- [ ] **Step 2: Verify worktree is clean**

```bash
git status
```
Expected: `On branch feat/at-n-hints-pty-detection`, nothing to commit.

---

## Task 2: `collect_at_refs` — parse complete `@(N)` tokens from a line

**Files:**
- Create: `gait-rs/src/repl_helper.rs`

- [ ] **Step 1: Create `repl_helper.rs` with the parsing function and a test**

```rust
// gait-rs/src/repl_helper.rs
use std::collections::HashSet;
use std::path::PathBuf;

pub struct ReplHelper {
    pub shell_history_path: PathBuf,
}

/// Returns numeric @(N) indices found in `line`, in order, deduplicated.
/// Only complete tokens (with closing `)`) are returned.
pub fn collect_at_refs(line: &str) -> Vec<usize> {
    let chars: Vec<char> = line.chars().collect();
    let mut result = Vec::new();
    let mut seen = HashSet::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '@'
            && i + 1 < chars.len()
            && chars[i + 1] == '('
        {
            let inner_start = i + 2;
            if let Some(rel) = chars[inner_start..].iter().position(|&c| c == ')') {
                let inner: String = chars[inner_start..inner_start + rel].iter().collect();
                if let Ok(n) = inner.trim().parse::<usize>() {
                    if n >= 1 && !seen.contains(&n) {
                        result.push(n);
                        seen.insert(n);
                    }
                }
                i = inner_start + rel + 1;
                continue;
            }
        }
        i += 1;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_refs_returns_empty() {
        assert_eq!(collect_at_refs("explain this error"), vec![]);
    }

    #[test]
    fn single_ref_returned() {
        assert_eq!(collect_at_refs("explain @(1)"), vec![1]);
    }

    #[test]
    fn multiple_refs_in_order() {
        assert_eq!(collect_at_refs("diff @(1) and @(2)"), vec![1, 2]);
    }

    #[test]
    fn duplicate_ref_deduplicated() {
        assert_eq!(collect_at_refs("@(1) vs @(1)"), vec![1]);
    }

    #[test]
    fn partial_token_ignored() {
        // @(2 without closing ) — incomplete, must not appear
        assert_eq!(collect_at_refs("@(2 is incomplete"), vec![]);
    }

    #[test]
    fn non_numeric_token_ignored() {
        // @(echo hi) is a command, not a history ref
        assert_eq!(collect_at_refs("@(echo hi)"), vec![]);
    }

    #[test]
    fn zero_ref_ignored() {
        assert_eq!(collect_at_refs("@(0) is invalid"), vec![]);
    }

    #[test]
    fn two_digit_ref() {
        assert_eq!(collect_at_refs("@(12)"), vec![12]);
    }
}
```

- [ ] **Step 2: Run the tests — expect them to pass**

```bash
cd gait-rs && cargo test repl_helper 2>&1
```
Expected: `test result: ok. 8 passed`

- [ ] **Step 3: Commit**

```bash
git add gait-rs/src/repl_helper.rs
git commit -m "feat: add repl_helper with collect_at_refs for @(N) token parsing"
```

---

## Task 3: Implement `Hinter` on `ReplHelper` + remaining rustyline traits

**Files:**
- Modify: `gait-rs/src/repl_helper.rs`

- [ ] **Step 1: Add the rustyline trait impls**

Append to `repl_helper.rs` (below `collect_at_refs` and before the `#[cfg(test)]` block):

```rust
use std::borrow::Cow;
use rustyline::completion::Completer;
use rustyline::highlight::Highlighter;
use rustyline::hint::{Hint, Hinter};
use rustyline::validate::Validator;
use rustyline::{Context, Helper};

// ── Marker trait ────────────────────────────────────────────────────────────
impl Helper for ReplHelper {}

// ── Completer (no-op) ───────────────────────────────────────────────────────
impl Completer for ReplHelper {
    type Candidate = String;
}

// ── Validator (no-op) ───────────────────────────────────────────────────────
impl Validator for ReplHelper {}

// ── Highlighter: dim the ghost text ─────────────────────────────────────────
impl Highlighter for ReplHelper {
    fn highlight_hint<'h>(&self, hint: &'h str) -> Cow<'h, str> {
        Cow::Owned(format!("\x1b[2m{}\x1b[0m", hint))
    }
}

// ── Hint wrapper ─────────────────────────────────────────────────────────────
pub struct ReplHint(pub String);

impl Hint for ReplHint {
    fn text(&self) -> &str {
        &self.0
    }
    fn completion(&self) -> Option<&str> {
        None
    }
}

// ── Hinter ───────────────────────────────────────────────────────────────────
impl Hinter for ReplHelper {
    type Hint = ReplHint;

    fn hint(&self, line: &str, _pos: usize, _ctx: &Context<'_>) -> Option<ReplHint> {
        let refs = collect_at_refs(line);
        if refs.is_empty() {
            return None;
        }
        let cmds: Vec<String> = refs
            .iter()
            .filter_map(|&n| {
                crate::shell_history::recall(&self.shell_history_path, n)
                    .ok()
                    .flatten()
            })
            .map(|e| e.cmd)
            .collect();
        if cmds.is_empty() {
            return None;
        }
        Some(ReplHint(format!("  ← {}", cmds.join(" · "))))
    }
}
```

- [ ] **Step 2: Add a test for `hint()` returning the right text**

Inside the existing `#[cfg(test)]` block in `repl_helper.rs`, add:

```rust
    #[test]
    fn hint_shows_command_names() {
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let path = dir.path().join("shell.jsonl");
        crate::shell_history::append(&path, "cargo test", "ok", 1).unwrap();
        crate::shell_history::append(&path, "cat src/main.rs", "fn main", 2).unwrap();

        let helper = ReplHelper { shell_history_path: path };
        let ctx = rustyline::Context::new(&rustyline::history::DefaultHistory::new());
        // @(1) = most recent = cat src/main.rs, @(2) = cargo test
        let hint = helper.hint("diff @(1) and @(2)", 18, &ctx).unwrap();
        assert!(hint.text().contains("cat src/main.rs"), "got: {}", hint.text());
        assert!(hint.text().contains("cargo test"), "got: {}", hint.text());
    }

    #[test]
    fn hint_none_when_no_refs() {
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let path = dir.path().join("shell.jsonl");
        let helper = ReplHelper { shell_history_path: path };
        let ctx = rustyline::Context::new(&rustyline::history::DefaultHistory::new());
        assert!(helper.hint("plain prompt no refs", 20, &ctx).is_none());
    }

    #[test]
    fn hint_none_when_history_empty() {
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let path = dir.path().join("shell.jsonl");
        let helper = ReplHelper { shell_history_path: path };
        let ctx = rustyline::Context::new(&rustyline::history::DefaultHistory::new());
        assert!(helper.hint("explain @(1)", 12, &ctx).is_none());
    }
```

- [ ] **Step 3: Run tests**

```bash
cd gait-rs && cargo test repl_helper 2>&1
```
Expected: `test result: ok. 11 passed`

- [ ] **Step 4: Commit**

```bash
git add gait-rs/src/repl_helper.rs
git commit -m "feat: implement Hinter on ReplHelper — ghost text for @(N) references"
```

---

## Task 4: Wire `ReplHelper` into the REPL

**Files:**
- Modify: `gait-rs/src/chat.rs`

- [ ] **Step 1: Add the module and swap `DefaultEditor`**

In `chat.rs`, replace:
```rust
use rustyline::DefaultEditor;
```
with:
```rust
use rustyline::Editor;
use rustyline::history::DefaultHistory;

mod repl_helper;
use repl_helper::ReplHelper;
```

- [ ] **Step 2: Replace the editor instantiation (around line 321)**

Replace:
```rust
    let mut rl = DefaultEditor::new()?;
    let _ = rl.load_history(&history_path);
```
with:
```rust
    let mut rl = Editor::<ReplHelper, DefaultHistory>::new()?;
    let _ = rl.load_history(&history_path);
    rl.set_helper(Some(ReplHelper {
        shell_history_path: shell_history_path.clone(),
    }));
```

- [ ] **Step 3: Update helper path on `/new` (around line 364)**

Replace:
```rust
            "/new" => {
                session_id = new_session_id();
                shell_history_path = workspace.join("tmp").join(format!("shell-{}.jsonl", session_id));
                eprintln!("new session {}", session_id);
                continue;
            }
```
with:
```rust
            "/new" => {
                session_id = new_session_id();
                shell_history_path = workspace.join("tmp").join(format!("shell-{}.jsonl", session_id));
                if let Some(h) = rl.helper_mut() {
                    h.shell_history_path = shell_history_path.clone();
                }
                eprintln!("new session {}", session_id);
                continue;
            }
```

- [ ] **Step 4: Build to check compile errors**

```bash
cd gait-rs && cargo build 2>&1
```
Expected: `Finished` with no errors.

- [ ] **Step 5: Run full test suite**

```bash
cd gait-rs && cargo test 2>&1
```
Expected: all tests pass.

- [ ] **Step 6: Commit**

```bash
git add gait-rs/src/chat.rs
git commit -m "feat: wire ReplHelper into REPL — @(N) ghost-text hint active"
```

---

## Task 5: Add `portable-pty` dependency

**Files:**
- Modify: `gait-rs/Cargo.toml`

- [ ] **Step 1: Add the dependency**

In `gait-rs/Cargo.toml`, inside `[dependencies]`, add:
```toml
portable-pty = "0.8"
```

- [ ] **Step 2: Verify it resolves**

```bash
cd gait-rs && cargo fetch 2>&1
```
Expected: no errors.

- [ ] **Step 3: Commit**

```bash
git add gait-rs/Cargo.toml gait-rs/Cargo.lock
git commit -m "chore: add portable-pty dependency for PTY-based command spawning"
```

---

## Task 6: Implement `run_and_capture_pty` with alternate-screen detection

**Files:**
- Modify: `gait-rs/src/chat.rs`

- [ ] **Step 1: Write the failing test first**

Inside the `#[cfg(test)]` block at the bottom of `chat.rs`, add:

```rust
    #[test]
    fn pty_capture_normal_command() {
        let (result, code) = run_and_capture_pty("echo hello", std::path::Path::new("."));
        assert_eq!(result.trim(), "hello");
        assert_eq!(code, Some(0));
    }

    #[test]
    fn pty_capture_detects_alt_screen() {
        // printf the alternate-screen sequence; must return Interactive
        let result = run_and_capture_pty_raw(
            "printf '\\033[?1049h'",
            std::path::Path::new("."),
        );
        assert!(
            matches!(result, CaptureResult::Interactive),
            "expected Interactive"
        );
    }
```

Note: two separate functions are exposed — `run_and_capture_pty` returns `(String, Option<i32>)` for the normal path (used by `expand_paren_marker`), and `run_and_capture_pty_raw` returns `CaptureResult` (used in tests for the interactive branch). See implementation step.

- [ ] **Step 2: Run the test to confirm it fails**

```bash
cd gait-rs && cargo test pty_capture 2>&1
```
Expected: compile error — `run_and_capture_pty` not found.

- [ ] **Step 3: Add imports and `CaptureResult` enum at the top of `chat.rs`**

After the existing `use std::process::{Command, Stdio};` line, add:

```rust
use std::io::Read;
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
```

After the existing `fn looks_like_path` function (around line 206), add:

```rust
#[derive(Debug)]
enum CaptureResult {
    Output(String, Option<i32>),
    Interactive,
}
```

- [ ] **Step 4: Implement `run_and_capture_pty_raw` and `run_and_capture_pty`**

Add these two functions after the `CaptureResult` definition:

```rust
fn run_and_capture_pty_raw(cmd: &str, cwd: &std::path::Path) -> CaptureResult {
    let pty_system = native_pty_system();
    let pair = match pty_system.openpty(PtySize {
        rows: 24,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
    }) {
        Ok(p) => p,
        Err(_) => {
            let (out, code) = run_and_capture(cmd, cwd);
            return CaptureResult::Output(out, code);
        }
    };

    let mut builder = CommandBuilder::new("sh");
    builder.args(["-c", cmd]);
    builder.cwd(cwd);

    let mut child = match pair.slave.spawn_command(builder) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("\x1b[31m[!] error: {}\x1b[0m", e);
            return CaptureResult::Output(String::new(), None);
        }
    };

    // Drop slave in parent so master sees EOF when child exits
    drop(pair.slave);

    let mut reader = match pair.master.try_clone_reader() {
        Ok(r) => r,
        Err(_) => return CaptureResult::Output(String::new(), None),
    };

    const ALT_SCREEN: &[u8] = b"\x1b[?1049h";
    const SCAN_LIMIT: usize = 4096;
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];

    while buf.len() < SCAN_LIMIT {
        match reader.read(&mut byte) {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                buf.push(byte[0]);
                if buf.ends_with(ALT_SCREEN) {
                    let _ = child.kill();
                    let _ = child.wait();
                    return CaptureResult::Interactive;
                }
            }
        }
    }

    let mut rest = Vec::new();
    let _ = reader.read_to_end(&mut rest);
    let code = child.wait().ok().and_then(|s| s.exit_code()).map(|c| c as i32);

    let all = [buf, rest].concat();
    let output = String::from_utf8_lossy(&all).into_owned();
    for line in output.lines() {
        println!("{}", line);
    }
    CaptureResult::Output(output, code)
}

fn run_and_capture_pty(cmd: &str, cwd: &std::path::Path) -> (String, Option<i32>) {
    match run_and_capture_pty_raw(cmd, cwd) {
        CaptureResult::Output(out, code) => (out, code),
        CaptureResult::Interactive => (String::new(), Some(1)),
    }
}
```

- [ ] **Step 5: Run the tests**

```bash
cd gait-rs && cargo test pty_capture 2>&1
```
Expected: `test result: ok. 2 passed`

- [ ] **Step 6: Commit**

```bash
git add gait-rs/src/chat.rs
git commit -m "feat: add run_and_capture_pty with alternate-screen TUI detection"
```

---

## Task 7: Wire PTY capture into `expand_paren_marker`

**Files:**
- Modify: `gait-rs/src/chat.rs`

- [ ] **Step 1: Update `expand_paren_marker` to use `run_and_capture_pty_raw`**

Replace the existing function body (currently ending with `format!("[shell: {}]\n{}", inner, output.trim_end())`):

```rust
fn expand_paren_marker(
    inner: &str,
    history_path: &std::path::Path,
    codebase: &std::path::Path,
) -> String {
    if let Ok(n) = inner.trim().parse::<usize>() {
        if n >= 1 {
            return match crate::shell_history::recall(history_path, n) {
                Ok(Some(entry)) => format!("[shell: {}]\n{}", entry.cmd, entry.output.trim_end()),
                Ok(None) => format!("[history @({n}) — no entry]"),
                Err(e) => format!("[history @({n}) — error: {e}]"),
            };
        }
    }
    match run_and_capture_pty_raw(inner, codebase) {
        CaptureResult::Output(output, code) => {
            if code != Some(0) {
                eprintln!("[!] @({inner}) exit {}", code.unwrap_or(1));
            }
            format!("[shell: {}]\n{}", inner, output.trim_end())
        }
        CaptureResult::Interactive => {
            format!("[interactive TUI — run @({inner}) directly in your terminal]")
        }
    }
}
```

- [ ] **Step 2: Run full test suite**

```bash
cd gait-rs && cargo test 2>&1
```
Expected: all tests pass (existing `expand_at_markers` tests continue to pass since they use numeric `@(N)` or simple shell commands that produce no escape codes).

- [ ] **Step 3: Commit**

```bash
git add gait-rs/src/chat.rs
git commit -m "feat: use PTY capture in expand_paren_marker — TUI commands abort cleanly"
```

---

## Task 8: Manual smoke test

- [ ] **Step 1: Build and run gait**

```bash
cd gait-rs && cargo build && ../target/debug/gait
```

- [ ] **Step 2: Run a shell command and test @(N) hint**

```
> ! ls -la
> explain @(1)
```
Expected: ghost text `  ← ls -la` appears dim after the cursor while typing `@(1)`.

- [ ] **Step 3: Test TUI detection**

```
> @(top)
```
Expected: output line `[interactive TUI — run @(top) directly in your terminal]` — gait does not hang or corrupt the terminal.

- [ ] **Step 4: Test /new rotates hint path**

```
> ! echo first
> /new
> ! echo second
> show @(1)
```
Expected: hint shows `second` (from new session), not `first`.

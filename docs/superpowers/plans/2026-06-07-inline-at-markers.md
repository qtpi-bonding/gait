---
status: draft
---

# Inline `@(cmd)` / `@file` Substitution

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the whole-line `@cmd` / `@-N` syntax with inline `@(cmd)` / `@(N)` / `@path` markers that can appear anywhere in a message.

**Architecture:** Add an `expand_at_markers()` function in `chat.rs` that scans the input string for `@(...)` and `@path` tokens and substitutes them before passing the message to the agent. Remove the old whole-line `@` handling entirely.

**Tech Stack:** Rust, existing `shell_history` module, `std::fs`

---

## Current state (to remove)

`gait-rs/src/chat.rs` lines 25–49: `AtCommand` enum, `is_at_command()`, `parse_at_command()`  
`gait-rs/src/chat.rs` lines 302–337: whole-line `@` branch in REPL loop

---

## New syntax

| Input | Behaviour |
|-------|-----------|
| `@(ls .)` | Run `ls .` in codebase dir, substitute output |
| `@(1)` | Inject most recent `!` history entry (1 = most recent, 2 = one before) |
| `@src/main.rs` | Read file relative to codebase, substitute contents |
| `@/abs/path` | Read absolute path |
| `!cmd` | Unchanged — run shell, save to history, not sent to agent |

Each substitution is wrapped with a header:
- Command: `[shell: ls .]\n<output>`
- History recall: `[shell: cargo test]\n<output>` (using original cmd from history)
- File: `[file: src/main.rs]\n<contents>`

`@path` is recognised when the token after `@` starts with `/`, `~/`, `./`, `../`, or contains a `/` or a `.` with a file extension (e.g. `README.md`, `src/lib.rs`). Plain words like `@alice` are passed through unchanged.

---

## Task 1: Remove old whole-line `@` handling

**Files:**
- Modify: `gait-rs/src/chat.rs`

- [ ] **Step 1: Delete the AtCommand infrastructure**

Remove lines 25–49 from `chat.rs` (the `AtCommand` enum, `is_at_command()`, and `parse_at_command()` functions).

- [ ] **Step 2: Delete the whole-line @ branch in the REPL loop**

Replace lines 302–337 (the `if is_at_command(&line) { ... } else { line.clone() }` block) with:

```rust
let task: String = line.clone();
```

(This will be replaced properly in Task 3 — just a placeholder to keep it compiling.)

- [ ] **Step 3: Run tests**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test 2>&1 | tail -20
```

Expected: all tests pass (old `@` tests will be deleted; no new failures).

- [ ] **Step 4: Commit**

```bash
git add gait-rs/src/chat.rs
git commit -m "refactor: remove whole-line @cmd syntax"
```

---

## Task 2: Add `expand_at_markers()`

**Files:**
- Modify: `gait-rs/src/chat.rs`

- [ ] **Step 1: Write failing tests**

Add a `#[cfg(test)]` block at the bottom of `chat.rs`:

```rust
#[cfg(test)]
mod expand_tests {
    use super::*;
    use tempfile::TempDir;

    fn make_history(dir: &std::path::Path) -> std::path::PathBuf {
        let p = dir.join("hist.jsonl");
        crate::shell_history::append(&p, "ls .", "file_a\nfile_b", 1).unwrap();
        p
    }

    #[test]
    fn passthrough_plain_text() {
        let dir = TempDir::new().unwrap();
        let hist = dir.path().join("hist.jsonl");
        let result = expand_at_markers("hello world", &hist, dir.path()).unwrap();
        assert_eq!(result, "hello world");
    }

    #[test]
    fn expands_command_inline() {
        let dir = TempDir::new().unwrap();
        let hist = dir.path().join("hist.jsonl");
        let result = expand_at_markers("files: @(echo hi)", &hist, dir.path()).unwrap();
        assert!(result.contains("[shell: echo hi]"), "got: {result}");
        assert!(result.contains("hi"), "got: {result}");
    }

    #[test]
    fn expands_history_recall() {
        let dir = TempDir::new().unwrap();
        let hist = make_history(dir.path());
        let result = expand_at_markers("prev: @(1)", &hist, dir.path()).unwrap();
        assert!(result.contains("[shell: ls .]"), "got: {result}");
        assert!(result.contains("file_a"), "got: {result}");
    }

    #[test]
    fn expands_file_path() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("notes.txt"), "hello file").unwrap();
        let hist = dir.path().join("hist.jsonl");
        let input = format!("read this: @notes.txt");
        let result = expand_at_markers(&input, &hist, dir.path()).unwrap();
        assert!(result.contains("[file: notes.txt]"), "got: {result}");
        assert!(result.contains("hello file"), "got: {result}");
    }

    #[test]
    fn ignores_at_username() {
        let dir = TempDir::new().unwrap();
        let hist = dir.path().join("hist.jsonl");
        let result = expand_at_markers("ping @alice today", &hist, dir.path()).unwrap();
        assert_eq!(result, "ping @alice today");
    }

    #[test]
    fn multiple_expansions() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.txt"), "aaa").unwrap();
        let hist = dir.path().join("hist.jsonl");
        let result = expand_at_markers("x=@(echo x) y=@a.txt", &hist, dir.path()).unwrap();
        assert!(result.contains("[shell: echo x]"), "got: {result}");
        assert!(result.contains("[file: a.txt]"), "got: {result}");
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test expand_tests 2>&1 | tail -20
```

Expected: compile error (function doesn't exist yet).

- [ ] **Step 3: Implement `expand_at_markers()`**

Add above the test block in `chat.rs`:

```rust
/// Expand inline @(cmd), @(N), and @path markers in `input`.
/// - @(cmd)  → run cmd in codebase dir, substitute [shell: cmd]\n<output>
/// - @(N)    → recall Nth history entry, substitute [shell: original_cmd]\n<output>
/// - @path   → read file (relative to codebase), substitute [file: path]\n<contents>
/// Tokens that don't match are passed through unchanged.
pub fn expand_at_markers(
    input: &str,
    history_path: &std::path::Path,
    codebase: &std::path::Path,
) -> anyhow::Result<String> {
    let mut result = String::with_capacity(input.len());
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        if chars[i] != '@' {
            result.push(chars[i]);
            i += 1;
            continue;
        }
        // @ found — peek ahead
        let rest_start = i + 1;
        if rest_start >= chars.len() {
            result.push('@');
            i += 1;
            continue;
        }

        if chars[rest_start] == '(' {
            // @(...)  — find closing paren
            let inner_start = rest_start + 1;
            let close = chars[inner_start..].iter().position(|&c| c == ')');
            if let Some(rel) = close {
                let inner: String = chars[inner_start..inner_start + rel].iter().collect();
                let expanded = expand_paren_marker(&inner, history_path, codebase);
                result.push_str(&expanded);
                i = inner_start + rel + 1;
                continue;
            }
            // no closing paren — pass through
            result.push('@');
            i += 1;
            continue;
        }

        // Try @path — read until whitespace
        let token_end = chars[rest_start..]
            .iter()
            .position(|c| c.is_whitespace())
            .map(|p| rest_start + p)
            .unwrap_or(chars.len());
        let token: String = chars[rest_start..token_end].iter().collect();

        if looks_like_path(&token) {
            let path = if token.starts_with('/') || token.starts_with('~') {
                std::path::PathBuf::from(&token)
            } else {
                codebase.join(&token)
            };
            match std::fs::read_to_string(&path) {
                Ok(contents) => {
                    result.push_str(&format!("[file: {}]\n{}", token, contents.trim_end()));
                    i = token_end;
                    continue;
                }
                Err(e) => {
                    result.push_str(&format!("[file: {} — error: {}]", token, e));
                    i = token_end;
                    continue;
                }
            }
        }

        // Not a recognised marker — pass through
        result.push('@');
        i += 1;
    }

    Ok(result)
}

fn expand_paren_marker(
    inner: &str,
    history_path: &std::path::Path,
    codebase: &std::path::Path,
) -> String {
    // Pure integer → history recall
    if let Ok(n) = inner.trim().parse::<usize>() {
        if n >= 1 {
            return match crate::shell_history::recall(history_path, n) {
                Ok(Some(entry)) => format!("[shell: {}]\n{}", entry.cmd, entry.output.trim_end()),
                Ok(None) => format!("[history @({n}) — no entry]"),
                Err(e) => format!("[history @({n}) — error: {e}]"),
            };
        }
    }
    // Shell command
    let (output, code) = run_and_capture(inner, codebase);
    if code != Some(0) {
        eprintln!("[!] @({inner}) exit {}", code.unwrap_or(1));
    }
    format!("[shell: {}]\n{}", inner, output.trim_end())
}

fn looks_like_path(token: &str) -> bool {
    if token.is_empty() { return false; }
    // Absolute or home-relative
    if token.starts_with('/') || token.starts_with("~/") { return true; }
    // Relative with explicit ./  ../
    if token.starts_with("./") || token.starts_with("../") { return true; }
    // Contains a path separator
    if token.contains('/') { return true; }
    // Has a dot-extension like README.md, config.toml
    if let Some(dot) = token.rfind('.') {
        let ext = &token[dot + 1..];
        return !ext.is_empty() && ext.chars().all(|c| c.is_alphanumeric());
    }
    false
}
```

- [ ] **Step 4: Run tests**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test expand_tests 2>&1 | tail -30
```

Expected: all 6 tests pass.

- [ ] **Step 5: Commit**

```bash
git add gait-rs/src/chat.rs
git commit -m "feat: add expand_at_markers for inline @(cmd)/@(N)/@path substitution"
```

---

## Task 3: Wire into REPL loop

**Files:**
- Modify: `gait-rs/src/chat.rs`

- [ ] **Step 1: Replace the placeholder with the real call**

Find the placeholder from Task 1:
```rust
let task: String = line.clone();
```

Replace with:
```rust
let task: String = match expand_at_markers(&line, &shell_history_path, &codebase) {
    Ok(expanded) => expanded,
    Err(e) => {
        eprintln!("[!] @ expand error: {e}");
        continue;
    }
};
```

- [ ] **Step 2: Run all tests**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test 2>&1 | tail -20
```

Expected: all tests pass.

- [ ] **Step 3: Commit**

```bash
git add gait-rs/src/chat.rs
git commit -m "feat: wire expand_at_markers into REPL — inline @(cmd) and @file now live"
```

---

## Verification

Manual smoke test in the REPL:

```
! cargo build 2>&1 | tail -5       # save some output to history
Fix this build: @(cargo build 2>&1 | tail -10)
The last build output was: @(1)
Here is the source: @src/lib.rs
This should pass through: @alice said hello
```

Confirm:
- `@(cargo build ...)` is replaced with `[shell: cargo build ...]\n<output>` before reaching the agent
- `@(1)` recalls the `cargo build` from `!` history
- `@src/lib.rs` is replaced with `[file: src/lib.rs]\n<contents>`
- `@alice` is passed through unchanged

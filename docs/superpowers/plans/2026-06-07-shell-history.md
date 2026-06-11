---
status: done
---

# Shell History (`!` capture + `@` inject) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let users capture `!` shell output to a per-session JSONL file and inject past outputs into the AI conversation via `@ cmd` (run + inject) and `@-N` (recall Nth most recent).

**Architecture:** A new `shell_history` module handles append/recall against `<workspace>/tmp/shell-<session-id>.jsonl`. A shared `run_and_capture` helper in `chat.rs` replaces the current `.status()` call for `!` commands so output is both printed and stored. `@` commands are parsed into an `AtCommand` enum and either run-and-inject or recall-and-inject before spawning gait.

**Tech Stack:** Rust, `serde_json`, existing `crate::session::transcript::now_ms()`, `rustyline` (unchanged).

---

## File Map

| File | Action | Purpose |
|------|--------|---------|
| `gait-rs/src/shell_history.rs` | Create | `append` / `recall` backed by JSONL file |
| `gait-rs/src/lib.rs` | Modify | expose `pub mod shell_history` |
| `gait-rs/src/chat.rs` | Modify | `run_and_capture`, `!` capture, `@` parse + execute, `/new` path update |

---

## Task 1: `shell_history` module

**Files:**
- Create: `gait-rs/src/shell_history.rs`
- Test: `gait-rs/src/shell_history.rs` (inline `#[cfg(test)]`)

- [ ] **Step 1: Write failing tests**

```rust
// bottom of gait-rs/src/shell_history.rs (add after the impl)

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn ts() -> u64 { 1_000_000 }

    #[test]
    fn recall_from_missing_file_returns_none() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("shell.jsonl");
        assert!(recall(&path, 1).unwrap().is_none());
    }

    #[test]
    fn append_and_recall_single_entry() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("shell.jsonl");
        append(&path, "ls -la", "total 8\nfoo", ts()).unwrap();
        let entry = recall(&path, 1).unwrap().unwrap();
        assert_eq!(entry.cmd, "ls -la");
        assert_eq!(entry.output, "total 8\nfoo");
    }

    #[test]
    fn recall_nth_entry() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("shell.jsonl");
        append(&path, "cmd1", "out1", ts()).unwrap();
        append(&path, "cmd2", "out2", ts()).unwrap();
        append(&path, "cmd3", "out3", ts()).unwrap();
        // @-1 = most recent
        assert_eq!(recall(&path, 1).unwrap().unwrap().cmd, "cmd3");
        // @-2 = second most recent
        assert_eq!(recall(&path, 2).unwrap().unwrap().cmd, "cmd2");
        // @-3 = oldest
        assert_eq!(recall(&path, 3).unwrap().unwrap().cmd, "cmd1");
    }

    #[test]
    fn recall_beyond_history_returns_none() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("shell.jsonl");
        append(&path, "cmd1", "out1", ts()).unwrap();
        assert!(recall(&path, 2).unwrap().is_none());
    }

    #[test]
    fn append_creates_parent_dirs() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nested/deep/shell.jsonl");
        append(&path, "pwd", "/home", ts()).unwrap();
        assert!(path.exists());
    }
}
```

- [ ] **Step 2: Run to verify it fails**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test shell_history 2>&1 | tail -20
```

Expected: compile error — `shell_history` module doesn't exist yet.

- [ ] **Step 3: Implement `shell_history.rs`**

Create `gait-rs/src/shell_history.rs`:

```rust
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct ShellEntry {
    pub cmd: String,
    pub output: String,
    pub ts: u64,
}

pub fn append(path: &Path, cmd: &str, output: &str, ts: u64) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let entry = ShellEntry {
        cmd: cmd.to_string(),
        output: output.to_string(),
        ts,
    };
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(f, "{}", serde_json::to_string(&entry)?)?;
    Ok(())
}

/// Recall the Nth most recent entry (n=1 = most recent).
/// Returns None if the file doesn't exist or n exceeds history length.
pub fn recall(path: &Path, n: usize) -> anyhow::Result<Option<ShellEntry>> {
    if !path.exists() {
        return Ok(None);
    }
    let f = std::fs::File::open(path)?;
    let reader = BufReader::new(f);
    let lines: Vec<String> = reader
        .lines()
        .filter_map(|l| l.ok())
        .filter(|l| !l.is_empty())
        .collect();
    if n == 0 || n > lines.len() {
        return Ok(None);
    }
    let line = &lines[lines.len() - n];
    let entry: ShellEntry = serde_json::from_str(line)?;
    Ok(Some(entry))
}

#[cfg(test)]
mod tests {
    // ... (the tests from step 1 go here)
}
```

- [ ] **Step 4: Add module to `lib.rs`**

In `gait-rs/src/lib.rs`, add after `pub mod chat;`:

```rust
pub mod shell_history;
```

- [ ] **Step 5: Run tests**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test shell_history 2>&1 | tail -20
```

Expected: `5 passed`.

- [ ] **Step 6: Commit**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && git add src/shell_history.rs src/lib.rs && git commit -m "feat: add shell_history module with append/recall"
```

---

## Task 2: `run_and_capture` helper + capture `!` output

**Files:**
- Modify: `gait-rs/src/chat.rs`

The current `!` handler uses `.status()` — output goes to the terminal but is not captured. Replace it with a `run_and_capture` helper that pipes stdout+stderr, prints each line, and returns the accumulated output.

- [ ] **Step 1: Write failing tests for `run_and_capture`**

Add to the `#[cfg(test)]` block at the bottom of `chat.rs`:

```rust
#[test]
fn run_and_capture_captures_stdout() {
    let (out, code) = run_and_capture("echo hello", std::path::Path::new("."));
    assert_eq!(out.trim(), "hello");
    assert_eq!(code, Some(0));
}

#[test]
fn run_and_capture_captures_stderr() {
    // Nested sh so the inner >&2 sees fd2 already pointing to the pipe (not terminal).
    // Plain "echo err >&2" would NOT work — shell-level redirects interact badly with
    // the appended 2>&1 when both are in the same sh -c string.
    let (out, code) = run_and_capture("sh -c 'echo err >&2'", std::path::Path::new("."));
    assert_eq!(out.trim(), "err");
    assert_eq!(code, Some(0));
}

#[test]
fn run_and_capture_nonzero_exit() {
    let (_, code) = run_and_capture("exit 42", std::path::Path::new("."));
    assert_eq!(code, Some(42));
}

#[test]
fn run_and_capture_bad_command_returns_none_code() {
    // A command that doesn't exist — sh returns exit 127
    let (_, code) = run_and_capture("_no_such_cmd_xyz_", std::path::Path::new("."));
    assert!(code != Some(0));
}
```

- [ ] **Step 2: Run to verify they fail**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test run_and_capture 2>&1 | tail -10
```

Expected: compile error — `run_and_capture` not defined.

- [ ] **Step 3: Add `run_and_capture` to `chat.rs`**

Add this function before `run_chat` in `gait-rs/src/chat.rs`:

```rust
/// Run `cmd` via `sh -c`, merging stderr into stdout (2>&1).
/// Prints each line to terminal as it arrives.
/// Returns (accumulated_output, exit_code). exit_code is None if spawn fails.
fn run_and_capture(cmd: &str, cwd: &std::path::Path) -> (String, Option<i32>) {
    let merged = format!("{} 2>&1", cmd);
    let mut child = match Command::new("sh")
        .args(["-c", &merged])
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[!] error: {e}");
            return (String::new(), None);
        }
    };
    let stdout = child.stdout.take().expect("piped stdout");
    let reader = BufReader::new(stdout);
    let mut lines: Vec<String> = Vec::new();
    for line in reader.lines() {
        match line {
            Ok(l) => {
                println!("{}", l);
                lines.push(l);
            }
            Err(_) => break,
        }
    }
    let code = child
        .wait()
        .ok()
        .and_then(|s| s.code());
    (lines.join("\n"), code)
}
```

- [ ] **Step 4: Replace `!` handler in `run_chat` to use `run_and_capture` and store output**

Locate the `is_bang_command` block in `run_chat` (around line 209). Also add `shell_history_path` initialization just before the `loop {` in `run_chat`. The existing code:

```rust
    let mut session_id = new_session_id();
    eprintln!(
        "{}gait{}  {}session {}{}  {}(ctrl+d to exit, /new for fresh session){}",
        theme.accent.fg(), RESET,
        theme.meta.fg(), session_id, RESET,
        DIM, RESET,
    );

    loop {
```

Replace with:

```rust
    let mut session_id = new_session_id();
    eprintln!(
        "{}gait{}  {}session {}{}  {}(ctrl+d to exit, /new for fresh session){}",
        theme.accent.fg(), RESET,
        theme.meta.fg(), session_id, RESET,
        DIM, RESET,
    );
    let mut shell_history_path = workspace.join("tmp").join(format!("shell-{}.jsonl", session_id));

    loop {
```

Then replace the existing `is_bang_command` block:

```rust
        if is_bang_command(&line) {
            let cmd = bang_command_str(&line);
            match Command::new("sh")
                .args(["-c", cmd])
                .current_dir(&codebase)
                .status()
            {
                Ok(s) if !s.success() => eprintln!("[!] exit {}", s.code().unwrap_or(1)),
                Err(e) => eprintln!("[!] error: {e}"),
                _ => {}
            }
            continue;
        }
```

With:

```rust
        if is_bang_command(&line) {
            let cmd = bang_command_str(&line);
            let (output, code) = run_and_capture(cmd, &codebase);
            if code != Some(0) {
                eprintln!("[!] exit {}", code.unwrap_or(1));
            }
            let _ = crate::shell_history::append(
                &shell_history_path,
                cmd,
                &output,
                crate::session::transcript::now_ms(),
            );
            continue;
        }
```

- [ ] **Step 5: Run all tests**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test 2>&1 | tail -20
```

Expected: all tests pass including the new `run_and_capture_*` tests.

- [ ] **Step 6: Commit**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && git add src/chat.rs && git commit -m "feat: capture ! output to shell history file"
```

---

## Task 3: `@` command parsing

**Files:**
- Modify: `gait-rs/src/chat.rs`

Add `is_at_command`, `AtCommand` enum, and `parse_at_command` — pure functions with no side effects, fully unit-testable.

- [ ] **Step 1: Write failing tests**

Add to the `#[cfg(test)]` block in `chat.rs`:

```rust
#[test]
fn at_command_detected() {
    assert!(is_at_command("@ls"));
    assert!(is_at_command("@ ls -la"));
    assert!(is_at_command("@-1"));
    assert!(is_at_command("@-10"));
}

#[test]
fn at_command_not_detected() {
    assert!(!is_at_command("!ls"));
    assert!(!is_at_command("ls"));
    assert!(!is_at_command("/new"));
    assert!(!is_at_command(""));
}

#[test]
fn parse_at_command_run() {
    assert!(matches!(parse_at_command("@ ls -la"), Some(AtCommand::Run(s)) if s == "ls -la"));
    assert!(matches!(parse_at_command("@pwd"), Some(AtCommand::Run(s)) if s == "pwd"));
    assert!(matches!(parse_at_command("@cargo test"), Some(AtCommand::Run(s)) if s == "cargo test"));
}

#[test]
fn parse_at_command_recall() {
    assert!(matches!(parse_at_command("@-1"), Some(AtCommand::Recall(1))));
    assert!(matches!(parse_at_command("@-5"), Some(AtCommand::Recall(5))));
    assert!(matches!(parse_at_command("@-10"), Some(AtCommand::Recall(10))));
}

#[test]
fn parse_at_command_invalid() {
    assert!(parse_at_command("@").is_none());       // empty after @
    assert!(parse_at_command("@-0").is_none());     // zero index not valid
    assert!(parse_at_command("@--1").is_none());    // double dash
    assert!(parse_at_command("@ ").is_none());      // space only
}
```

- [ ] **Step 2: Run to verify failure**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test at_command 2>&1 | tail -10
```

Expected: compile error — `is_at_command`, `AtCommand`, `parse_at_command` not defined.

- [ ] **Step 3: Add the types and functions to `chat.rs`**

Add after the `bang_command_str` function (around line 23):

```rust
#[derive(Debug, PartialEq)]
enum AtCommand {
    Run(String),    // @ <cmd>  — run and inject
    Recall(usize),  // @-N     — inject Nth most recent ! output
}

fn is_at_command(s: &str) -> bool {
    s.starts_with('@')
}

fn parse_at_command(s: &str) -> Option<AtCommand> {
    let rest = s.strip_prefix('@')?;
    // Strip single leading space if present (handles "@ cmd" vs "@cmd")
    let rest = rest.strip_prefix(' ').unwrap_or(rest);
    if rest.is_empty() {
        return None;
    }
    if let Some(neg) = rest.strip_prefix('-') {
        match neg.parse::<usize>() {
            Ok(n) if n >= 1 => return Some(AtCommand::Recall(n)),
            _ => return None,
        }
    }
    Some(AtCommand::Run(rest.to_string()))
}
```

- [ ] **Step 4: Run tests**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test at_command 2>&1 | tail -10
```

Expected: `9 passed`.

- [ ] **Step 5: Commit**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && git add src/chat.rs && git commit -m "feat: add AtCommand parsing for @ inject syntax"
```

---

## Task 4: Wire `@` commands into the REPL loop

**Files:**
- Modify: `gait-rs/src/chat.rs`

Connect the parsed `AtCommand` to the gait spawn loop. Also update `shell_history_path` when `/new` resets the session.

- [ ] **Step 1: Locate the splice point in `run_chat`**

The current loop structure after the `is_bang_command` block is:

```rust
        match line.as_str() {
            "/new" => {
                session_id = new_session_id();
                eprintln!("new session {}", session_id);
                continue;
            }
            "/exit" | "exit" | "quit" => break,
            _ => {}
        }

        let task = line.clone();

        'spawn: loop {
```

- [ ] **Step 2: Replace that block**

Replace the block above with:

```rust
        match line.as_str() {
            "/new" => {
                session_id = new_session_id();
                shell_history_path = workspace.join("tmp").join(format!("shell-{}.jsonl", session_id));
                eprintln!("new session {}", session_id);
                continue;
            }
            "/exit" | "exit" | "quit" => break,
            _ => {}
        }

        let task: String = if is_at_command(&line) {
            match parse_at_command(&line) {
                Some(AtCommand::Run(cmd)) => {
                    let (output, code) = run_and_capture(&cmd, &codebase);
                    if code != Some(0) {
                        eprintln!("[!] exit {}", code.unwrap_or(1));
                    }
                    let _ = crate::shell_history::append(
                        &shell_history_path,
                        &cmd,
                        &output,
                        crate::session::transcript::now_ms(),
                    );
                    format!("[shell: {}]\n{}", cmd, output)
                }
                Some(AtCommand::Recall(n)) => {
                    match crate::shell_history::recall(&shell_history_path, n) {
                        Ok(Some(entry)) => format!("[shell: {}]\n{}", entry.cmd, entry.output),
                        Ok(None) => {
                            eprintln!("[!] no shell history at @-{}", n);
                            continue;
                        }
                        Err(e) => {
                            eprintln!("[!] shell history error: {e}");
                            continue;
                        }
                    }
                }
                None => {
                    eprintln!("[!] invalid @ command — use `@ <cmd>` or `@-N`");
                    continue;
                }
            }
        } else {
            line.clone()
        };

        'spawn: loop {
```

- [ ] **Step 3: Build**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo build 2>&1 | tail -20
```

Expected: clean build, 0 errors.

- [ ] **Step 4: Run all tests**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test 2>&1 | tail -20
```

Expected: all tests pass.

- [ ] **Step 5: Smoke test**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo build --release 2>&1 | tail -5
```

Then in a separate terminal, run `gait chat` and verify:
- `! echo hello` prints `hello` and creates `tmp/shell-<id>.jsonl`
- `@-1` sends `[shell: echo hello]\nhello` to gait
- `@ echo world` runs `echo world`, stores it, sends it to gait
- `@-0` prints `[!] invalid @ command`
- `/new` resets the history path (new file on next `!`)

- [ ] **Step 6: Commit**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && git add src/chat.rs && git commit -m "feat: wire @ inject commands into REPL — run+inject and @-N recall"
```

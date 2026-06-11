# UI Module Extraction Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Extract terminal UI widgets into `src/ui/`, pure input-processing helpers into `src/repl_input.rs`, and reduce `chat.rs` to an orchestration loop (~400 lines).

**Architecture:** Six tasks create the new modules without touching the originals; a final task wires everything in and deletes the dead code. The only behavioral change is Timer replacing the braille spinner — all other moves are verbatim.

**Tech Stack:** Rust, existing deps (`termimad`, `rustyline`, `portable_pty`, `uuid`, `serde_json`)

---

## File Map

| File | Change |
|---|---|
| `gait-rs/src/ui/mod.rs` | New — re-exports from all ui submodules |
| `gait-rs/src/ui/timer.rs` | New — Timer struct (replaces inline braille spinner) |
| `gait-rs/src/ui/renderer.rs` | Moved verbatim from `render.rs` |
| `gait-rs/src/ui/permission.rs` | Extracted verbatim from `chat.rs` |
| `gait-rs/src/ui/clarify.rs` | Extracted verbatim from `chat.rs` |
| `gait-rs/src/ui/inform.rs` | Extracted verbatim from `chat.rs` |
| `gait-rs/src/repl_input.rs` | Extracted verbatim from `chat.rs` |
| `gait-rs/src/render.rs` | Deleted |
| `gait-rs/src/chat.rs` | Remove extracted functions, replace spinner, update call sites |
| `gait-rs/src/lib.rs` | Remove `pub mod render`, add `pub mod ui`, `pub mod repl_input` |

**Do NOT change:** `src/bin/gait.rs`, `src/repl_helper.rs`, `src/permission.rs`, `src/theme.rs`, anything under `src/session/`.

---

## Task 1: `ui/timer.rs` — new Timer (replaces braille spinner)

**Files:**
- Create: `gait-rs/src/ui/timer.rs`
- Create (stub): `gait-rs/src/ui/mod.rs`
- Modify: `gait-rs/src/lib.rs`

- [ ] **Step 1: Write failing test**

Create `gait-rs/src/ui/timer.rs` with only the test module:

```rust
fn format_elapsed(secs: u64) -> String {
    todo!()
}

pub struct Timer;

impl Timer {
    pub fn start() -> Self { todo!() }
    pub fn stop(&mut self) { todo!() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_elapsed_zero() {
        assert_eq!(format_elapsed(0), "  thinking… [0s]");
    }

    #[test]
    fn format_elapsed_nonzero() {
        assert_eq!(format_elapsed(42), "  thinking… [42s]");
    }

    #[test]
    fn timer_starts_and_stops_without_panic() {
        let mut t = Timer::start();
        t.stop();
    }

    #[test]
    fn timer_stop_is_idempotent() {
        let mut t = Timer::start();
        t.stop();
        t.stop();
    }

    #[test]
    fn timer_drops_without_hang() {
        let _t = Timer::start();
        // drop() must call stop() — verifies no hang
    }
}
```

Create `gait-rs/src/ui/mod.rs`:

```rust
pub mod timer;
pub use timer::Timer;
```

Add to `gait-rs/src/lib.rs` after `pub mod render;`:

```rust
pub mod ui;
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs
cargo test ui::timer 2>&1 > /tmp/gait-test-output.txt; echo "exit: $?"
grep "FAILED\|test result\|^error" /tmp/gait-test-output.txt | head -20
```

Expected: compile error or panics on `todo!()`.

- [ ] **Step 3: Implement Timer**

Replace `gait-rs/src/ui/timer.rs` with the full implementation:

```rust
use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

pub struct Timer {
    stop_flag: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

fn format_elapsed(secs: u64) -> String {
    format!("  thinking… [{}s]", secs)
}

impl Timer {
    pub fn start() -> Self {
        let stop_flag = Arc::new(AtomicBool::new(false));
        let flag = stop_flag.clone();
        let handle = thread::spawn(move || {
            let mut secs = 0u64;
            loop {
                eprint!("\r{}", format_elapsed(secs));
                let _ = std::io::stderr().flush();
                // Check flag every 50ms so stop() returns promptly.
                for _ in 0..20 {
                    if flag.load(Ordering::Relaxed) {
                        eprint!("\r\x1b[2K");
                        let _ = std::io::stderr().flush();
                        return;
                    }
                    thread::sleep(Duration::from_millis(50));
                }
                secs += 1;
            }
        });
        Self { stop_flag, handle: Some(handle) }
    }

    pub fn stop(&mut self) {
        if self.handle.is_none() { return; }
        self.stop_flag.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            h.join().ok();
        }
    }
}

impl Drop for Timer {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_elapsed_zero() {
        assert_eq!(format_elapsed(0), "  thinking… [0s]");
    }

    #[test]
    fn format_elapsed_nonzero() {
        assert_eq!(format_elapsed(42), "  thinking… [42s]");
    }

    #[test]
    fn timer_starts_and_stops_without_panic() {
        let mut t = Timer::start();
        t.stop();
    }

    #[test]
    fn timer_stop_is_idempotent() {
        let mut t = Timer::start();
        t.stop();
        t.stop();
    }

    #[test]
    fn timer_drops_without_hang() {
        let _t = Timer::start();
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs
cargo test ui::timer 2>&1 > /tmp/gait-test-output.txt; echo "exit: $?"
grep "FAILED\|test result" /tmp/gait-test-output.txt
```

Expected: `test result: ok. 5 passed`.

- [ ] **Step 5: Commit**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs
git add src/ui/timer.rs src/ui/mod.rs src/lib.rs
git commit -m "feat: ui/timer.rs — Timer replaces braille spinner with seconds counter"
```

---

## Task 2: `ui/permission.rs` — extract permission prompting

**Files:**
- Create: `gait-rs/src/ui/permission.rs`
- Modify: `gait-rs/src/ui/mod.rs`

- [ ] **Step 1: Create `ui/permission.rs`**

```rust
use std::io::{BufRead, Write};
use crate::session::transcript::PermissionItem;
use crate::theme::{Theme, RESET, DIM_FG};

pub fn prompt_permission(
    item: &PermissionItem,
    label: Option<&str>,
    theme: &Theme,
) -> crate::permission::PermissionChoice {
    eprintln!();
    if let Some(l) = label {
        eprintln!("{}⟳ {}  {}({}){}", theme.accent.fg(), item.cmd, DIM_FG, l, RESET);
    } else {
        eprintln!("{}⟳ {}{}", theme.accent.fg(), item.cmd, RESET);
    }
    eprintln!();
    for (i, pat) in item.patterns.iter().enumerate() {
        eprintln!("  {}[{}] {}{}", DIM_FG, i + 1, pat, RESET);
    }
    eprintln!();
    eprintln!("  {}[o] once   [r] reject{}", DIM_FG, RESET);
    loop {
        eprint!("  {}>{} {}", theme.accent.fg(), RESET, theme.user_msg.fg());
        let _ = std::io::stderr().flush();
        let mut input = String::new();
        match std::io::stdin().lock().read_line(&mut input) {
            Ok(0) | Err(_) => {
                eprint!("{}", RESET);
                return crate::permission::PermissionChoice::Reject;
            }
            Ok(_) => { eprint!("{}", RESET); }
        }
        match parse_permission_response(input.trim(), &item.patterns) {
            Some(c) => return c,
            None => eprintln!("  unrecognized — try 1..{}, o, or r", item.patterns.len()),
        }
    }
}

fn parse_permission_response(
    input: &str,
    patterns: &[String],
) -> Option<crate::permission::PermissionChoice> {
    use crate::permission::PermissionChoice;
    match input.trim() {
        "r" | "reject" => return Some(PermissionChoice::Reject),
        "o" | "once" => {
            let pat = patterns.last()?;
            return Some(PermissionChoice::Once(pat.clone()));
        }
        _ => {}
    }
    if let Ok(n) = input.trim().parse::<usize>() {
        if n >= 1 && n <= patterns.len() {
            return Some(PermissionChoice::Agent(patterns[n - 1].clone()));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_permission_response_number() {
        let patterns = vec!["shell:cargo *".to_string(), "shell:cargo test *".to_string()];
        assert_eq!(
            parse_permission_response("1", &patterns),
            Some(crate::permission::PermissionChoice::Agent("shell:cargo *".to_string()))
        );
        assert_eq!(
            parse_permission_response("2", &patterns),
            Some(crate::permission::PermissionChoice::Agent("shell:cargo test *".to_string()))
        );
    }

    #[test]
    fn parse_permission_response_once() {
        let patterns = vec!["shell:cargo *".to_string(), "shell:cargo test".to_string()];
        assert_eq!(
            parse_permission_response("o", &patterns),
            Some(crate::permission::PermissionChoice::Once("shell:cargo test".to_string()))
        );
    }

    #[test]
    fn parse_permission_response_reject() {
        assert_eq!(
            parse_permission_response("r", &[]),
            Some(crate::permission::PermissionChoice::Reject)
        );
    }

    #[test]
    fn parse_permission_response_invalid() {
        let p = vec!["shell:cargo *".to_string()];
        assert_eq!(parse_permission_response("99", &p), None);
        assert_eq!(parse_permission_response("xyz", &p), None);
        assert_eq!(parse_permission_response("", &p), None);
    }
}
```

- [ ] **Step 2: Register in `ui/mod.rs`**

```rust
pub mod timer;
pub mod permission;

pub use timer::Timer;
pub use permission::prompt_permission;
```

- [ ] **Step 3: Run tests**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs
cargo test ui::permission 2>&1 > /tmp/gait-test-output.txt; echo "exit: $?"
grep "FAILED\|test result\|^error" /tmp/gait-test-output.txt | head -20
```

Expected: `test result: ok. 4 passed`.

- [ ] **Step 4: Commit**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs
git add src/ui/permission.rs src/ui/mod.rs
git commit -m "feat: ui/permission.rs — extract permission prompting from chat.rs"
```

---

## Task 3: `ui/clarify.rs` and `ui/inform.rs`

**Files:**
- Create: `gait-rs/src/ui/clarify.rs`
- Create: `gait-rs/src/ui/inform.rs`
- Modify: `gait-rs/src/ui/mod.rs`

- [ ] **Step 1: Create `ui/clarify.rs`**

```rust
use std::io::{BufRead, Write};
use crate::session::transcript::{ClarifyItem, ClarifyReplyItem};
use crate::theme::{Theme, RESET, DIM_FG};

pub fn prompt_clarify(items: &[ClarifyItem], theme: &Theme) -> Vec<ClarifyReplyItem> {
    let mut replies = Vec::new();
    for item in items {
        eprintln!();
        eprintln!("{}? [{}] {}{}", theme.accent.fg(), item.kind, item.content, RESET);
        eprintln!("  {}why: {}{}", DIM_FG, item.why, RESET);
        eprint!("{}>{} {}", theme.accent.fg(), RESET, theme.user_msg.fg());
        let _ = std::io::stderr().flush();
        let mut input = String::new();
        let _ = std::io::stdin().lock().read_line(&mut input);
        eprint!("{}", RESET);
        replies.push(ClarifyReplyItem {
            id: item.id.clone(),
            content: input.trim().to_string(),
            reasoning: None,
        });
    }
    replies
}
```

- [ ] **Step 2: Create `ui/inform.rs`**

```rust
use std::io::{BufRead, Write};
use crate::session::transcript::{InformItem, InformReplyItem};
use crate::theme::{Theme, RESET, DIM_FG};

pub fn prompt_inform(items: &[InformItem], theme: &Theme) -> Vec<InformReplyItem> {
    let mut replies = Vec::new();
    for item in items {
        eprintln!();
        eprintln!("{}\u{2192} {}{}", theme.accent.fg(), item.content, RESET);
        if let Some(ctx) = &item.context {
            eprintln!("  {}context: {}{}", DIM_FG, ctx, RESET);
        }
        eprintln!("  {}Press Enter to confirm, or type a correction:{}", DIM_FG, RESET);
        eprint!("{}>{} {}", theme.accent.fg(), RESET, theme.user_msg.fg());
        let _ = std::io::stderr().flush();
        let mut input = String::new();
        let _ = std::io::stdin().lock().read_line(&mut input);
        eprint!("{}", RESET);
        let response = input.trim().to_string();
        replies.push(InformReplyItem {
            id: item.id.clone(),
            content: if response.is_empty() { "understood".into() } else { response },
        });
    }
    replies
}
```

- [ ] **Step 3: Register in `ui/mod.rs`**

```rust
pub mod timer;
pub mod permission;
pub mod clarify;
pub mod inform;

pub use timer::Timer;
pub use permission::prompt_permission;
pub use clarify::prompt_clarify;
pub use inform::prompt_inform;
```

- [ ] **Step 4: Run full test suite to confirm no breakage**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs
cargo test 2>&1 > /tmp/gait-test-output.txt; echo "exit: $?"
grep "FAILED\|test result\|^error" /tmp/gait-test-output.txt | head -20
```

Expected: all tests pass.

- [ ] **Step 5: Commit**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs
git add src/ui/clarify.rs src/ui/inform.rs src/ui/mod.rs
git commit -m "feat: ui/clarify.rs + ui/inform.rs — extract clarify/inform prompts from chat.rs"
```

---

## Task 4: `repl_input.rs` — extract input-processing helpers

**Files:**
- Create: `gait-rs/src/repl_input.rs`
- Modify: `gait-rs/src/lib.rs`

These are verbatim moves from `chat.rs`. The functions and their tests move together.

- [ ] **Step 1: Create `gait-rs/src/repl_input.rs`**

Copy the following from `chat.rs` exactly, including the test modules:

```rust
use std::io::{BufRead, BufReader, Read};
use std::process::{Command, Stdio};

pub fn is_bang_command(s: &str) -> bool {
    s.starts_with('!')
}

pub fn bang_command_str(s: &str) -> &str {
    s.strip_prefix("! ").or_else(|| s.strip_prefix('!')).unwrap_or(s)
}

pub fn expand_quote_reply(input: &str, blocks: &[String]) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut result = String::with_capacity(input.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '>' && chars.get(i + 1) == Some(&'(') {
            let inner_start = i + 2;
            if let Some(rel) = chars[inner_start..].iter().position(|&c| c == ')') {
                let range: String = chars[inner_start..inner_start + rel].iter().collect();
                let range_str = range.trim();
                let looks_numeric = range_str.chars().all(|c| c.is_ascii_digit() || c == '-');
                if looks_numeric {
                    match resolve_quote_range(range_str, blocks) {
                        Some(expanded) => {
                            result.push_str(&expanded);
                            i = inner_start + rel + 1;
                            continue;
                        }
                        None => {
                            eprintln!("[gait] >({}): block out of range (last response had {} block{})",
                                range_str, blocks.len(),
                                if blocks.len() == 1 { "" } else { "s" });
                        }
                    }
                }
            }
        }
        result.push(chars[i]);
        i += 1;
    }
    result
}

pub fn resolve_quote_range(range: &str, blocks: &[String]) -> Option<String> {
    let selected: Vec<&str> = if let Some((s, e)) = range.split_once('-') {
        let start: usize = s.trim().parse().ok().filter(|&n: &usize| n > 0)?;
        let end: usize = e.trim().parse().ok().filter(|&n: &usize| n > 0)?;
        if end < start { return None; }
        let v: Vec<&str> = blocks.iter()
            .enumerate()
            .filter(|(i, _)| *i + 1 >= start && *i + 1 <= end)
            .map(|(_, b)| b.as_str())
            .collect();
        if v.is_empty() { return None; }
        v
    } else {
        let n: usize = range.trim().parse().ok().filter(|&n: &usize| n > 0)?;
        vec![blocks.get(n - 1)?]
    };
    let quoted = selected.iter()
        .flat_map(|b| b.lines().map(|l| format!("> {}", l)))
        .collect::<Vec<_>>()
        .join("\n");
    Some(quoted)
}

fn looks_like_path(token: &str) -> bool {
    if token.is_empty() { return false; }
    if token.starts_with('/') || token.starts_with("~/") { return true; }
    if token.starts_with("./") || token.starts_with("../") { return true; }
    if token.contains('/') { return true; }
    if let Some(dot) = token.rfind('.') {
        let ext = &token[dot + 1..];
        return !ext.is_empty() && ext.chars().all(|c| c.is_alphanumeric());
    }
    false
}

#[derive(Debug)]
pub enum CaptureResult {
    Output(String, Option<i32>),
    Interactive,
}

pub fn run_and_capture_pty_raw(cmd: &str, cwd: &std::path::Path) -> CaptureResult {
    use portable_pty::{CommandBuilder, PtySize, native_pty_system};
    let pty_system = native_pty_system();
    let pair = match pty_system.openpty(PtySize {
        rows: 24, cols: 80, pixel_width: 0, pixel_height: 0,
    }) {
        Ok(p) => p,
        Err(_) => {
            let (out, code) = run_and_capture_simple(cmd, cwd);
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
    let code = child.wait().ok().map(|s| s.exit_code() as i32);

    let all = [buf, rest].concat();
    let output = String::from_utf8_lossy(&all).into_owned();
    for line in output.lines() {
        println!("{}", line);
    }
    CaptureResult::Output(output, code)
}

/// Run `cmd` via `sh -c`, merging stderr into stdout. Used by expand_shell_marker.
fn run_and_capture_simple(cmd: &str, cwd: &std::path::Path) -> (String, Option<i32>) {
    let merged = format!("{} 2>&1", cmd);
    let mut child = match Command::new("sh")
        .args(["-c", &merged])
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("\x1b[31m[!] error: {}\x1b[0m", e);
            return (String::new(), None);
        }
    };
    let stdout = child.stdout.take().expect("piped stdout");
    let reader = BufReader::new(stdout);
    let mut lines: Vec<String> = Vec::new();
    for line in reader.lines() {
        match line {
            Ok(l) => { println!("{}", l); lines.push(l); }
            Err(_) => break,
        }
    }
    let code = child.wait().ok().and_then(|s| s.code());
    (lines.join("\n"), code)
}

pub fn expand_shell_marker(
    inner: &str,
    history_path: &std::path::Path,
    codebase: &std::path::Path,
) -> String {
    if let Ok(n) = inner.trim().parse::<usize>() {
        if n >= 1 {
            return match crate::shell_history::recall(history_path, n) {
                Ok(Some(entry)) => format!("[shell: {}]\n{}", entry.cmd, entry.output.trim_end()),
                Ok(None) => format!("[no shell history entry $({n}): use !cmd in the REPL to run commands and build history]"),
                Err(e) => format!("[shell history $({n}) error: {e}]"),
            };
        }
    }
    match run_and_capture_pty_raw(inner, codebase) {
        CaptureResult::Output(output, code) => {
            if code != Some(0) {
                eprintln!("[!] $({inner}) exit {}", code.unwrap_or(1));
            }
            format!("[shell: {}]\n{}", inner, output.trim_end())
        }
        CaptureResult::Interactive => {
            format!("[interactive TUI — run $({inner}) directly in your terminal]")
        }
    }
}

pub fn expand_at_markers(
    input: &str,
    history_path: &std::path::Path,
    codebase: &std::path::Path,
) -> anyhow::Result<String> {
    let mut result = String::with_capacity(input.len());
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '$' if chars.get(i + 1) == Some(&'(') => {
                let inner_start = i + 2;
                if let Some(rel) = chars[inner_start..].iter().position(|&c| c == ')') {
                    let inner: String = chars[inner_start..inner_start + rel].iter().collect();
                    result.push_str(&expand_shell_marker(&inner, history_path, codebase));
                    i = inner_start + rel + 1;
                    continue;
                }
                result.push('$');
                i += 1;
            }
            '@' => {
                let token_start = i + 1;
                if token_start < chars.len() {
                    let token_end = chars[token_start..]
                        .iter()
                        .position(|c| c.is_whitespace())
                        .map(|p| token_start + p)
                        .unwrap_or(chars.len());
                    let token: String = chars[token_start..token_end].iter().collect();
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
                }
                result.push('@');
                i += 1;
            }
            c => { result.push(c); i += 1; }
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bang_command_detected() {
        assert!(is_bang_command("! ls"));
        assert!(is_bang_command("!ls"));
        assert!(!is_bang_command("ls"));
        assert!(!is_bang_command("/new"));
    }

    #[test]
    fn bang_command_extracts_cmd() {
        assert_eq!(bang_command_str("! ls -la"), "ls -la");
        assert_eq!(bang_command_str("!pwd"), "pwd");
    }

    #[test]
    fn pty_capture_normal_command() {
        let result = run_and_capture_pty_raw("echo hello", std::path::Path::new("."));
        match result {
            CaptureResult::Output(out, code) => {
                assert_eq!(out.trim(), "hello");
                assert_eq!(code, Some(0));
            }
            CaptureResult::Interactive => panic!("echo should not be Interactive"),
        }
    }

    #[test]
    fn pty_capture_detects_alt_screen() {
        let result = run_and_capture_pty_raw(
            r#"printf "\033[?1049h""#,
            std::path::Path::new("."),
        );
        assert!(matches!(result, CaptureResult::Interactive), "expected Interactive");
    }
}

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
        let result = expand_at_markers("files: $(echo hi)", &hist, dir.path()).unwrap();
        assert!(result.contains("[shell: echo hi]"), "got: {result}");
        assert!(result.contains("hi"), "got: {result}");
    }

    #[test]
    fn expands_history_recall() {
        let dir = TempDir::new().unwrap();
        let hist = make_history(dir.path());
        let result = expand_at_markers("prev: $(1)", &hist, dir.path()).unwrap();
        assert!(result.contains("[shell: ls .]"), "got: {result}");
        assert!(result.contains("file_a"), "got: {result}");
    }

    #[test]
    fn expands_file_path() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("notes.txt"), "hello file").unwrap();
        let hist = dir.path().join("hist.jsonl");
        let result = expand_at_markers("read this: @notes.txt", &hist, dir.path()).unwrap();
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
    fn at_paren_no_longer_expands() {
        let dir = TempDir::new().unwrap();
        let hist = dir.path().join("hist.jsonl");
        let result = expand_at_markers("old: @(echo hi)", &hist, dir.path()).unwrap();
        assert!(result.contains("@(echo hi)"), "got: {result}");
    }

    #[test]
    fn multiple_expansions() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.txt"), "aaa").unwrap();
        let hist = dir.path().join("hist.jsonl");
        let result = expand_at_markers("x=$(echo x) y=@a.txt", &hist, dir.path()).unwrap();
        assert!(result.contains("[shell: echo x]"), "got: {result}");
        assert!(result.contains("[file: a.txt]"), "got: {result}");
    }
}

#[cfg(test)]
mod quote_reply_tests {
    use super::expand_quote_reply;

    fn blocks() -> Vec<String> {
        vec![
            "First paragraph here.".into(),
            "Second paragraph here.".into(),
            "Third paragraph here.".into(),
        ]
    }

    #[test]
    fn no_prefix_passes_through() {
        assert_eq!(expand_quote_reply("hello", &blocks()), "hello");
    }

    #[test]
    fn bare_gt_passes_through() {
        assert_eq!(expand_quote_reply(">", &blocks()), ">");
    }

    #[test]
    fn gt_without_parens_passes_through() {
        assert_eq!(expand_quote_reply(">2", &blocks()), ">2");
    }

    #[test]
    fn gt_paren_non_number_passes_through() {
        assert_eq!(expand_quote_reply(">(foo)", &blocks()), ">(foo)");
    }

    #[test]
    fn unclosed_paren_passes_through() {
        assert_eq!(expand_quote_reply(">(2", &blocks()), ">(2");
    }

    #[test]
    fn single_block_quoted() {
        let result = expand_quote_reply(">(2)", &blocks());
        assert_eq!(result, "> Second paragraph here.");
    }

    #[test]
    fn inline_mid_sentence() {
        let result = expand_quote_reply("regarding >(2) — what did you mean?", &blocks());
        assert!(result.contains("> Second paragraph here."), "got: {result}");
        assert!(result.contains("regarding"), "got: {result}");
        assert!(result.contains("what did you mean?"), "got: {result}");
    }

    #[test]
    fn single_block_with_trailing_text() {
        let result = expand_quote_reply(">(1) follow up", &blocks());
        assert_eq!(result, "> First paragraph here. follow up");
    }

    #[test]
    fn range_quoted() {
        let result = expand_quote_reply(">(1-2)", &blocks());
        assert!(result.contains("> First paragraph here."), "got: {result}");
        assert!(result.contains("> Second paragraph here."), "got: {result}");
        assert!(!result.contains("Third"), "got: {result}");
    }

    #[test]
    fn out_of_range_passes_through() {
        assert_eq!(expand_quote_reply(">(99)", &blocks()), ">(99)");
    }

    #[test]
    fn zero_passes_through() {
        assert_eq!(expand_quote_reply(">(0)", &blocks()), ">(0)");
    }

    #[test]
    fn empty_blocks_passes_through() {
        assert_eq!(expand_quote_reply(">(1)", &[]), ">(1)");
    }
}
```

- [ ] **Step 2: Register in `lib.rs`**

Add after `pub mod ui;`:

```rust
pub mod repl_input;
```

- [ ] **Step 3: Run repl_input tests**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs
cargo test repl_input 2>&1 > /tmp/gait-test-output.txt; echo "exit: $?"
grep "FAILED\|test result\|^error" /tmp/gait-test-output.txt | head -20
```

Expected: all tests pass.

- [ ] **Step 4: Commit**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs
git add src/repl_input.rs src/lib.rs
git commit -m "feat: repl_input.rs — extract input-processing helpers from chat.rs"
```

---

## Task 5: `ui/renderer.rs` + finalize `ui/mod.rs`

**Files:**
- Create: `gait-rs/src/ui/renderer.rs`
- Modify: `gait-rs/src/ui/mod.rs`

- [ ] **Step 1: Copy `render.rs` to `ui/renderer.rs`**

```bash
cp /Users/aicoder/Documents/ideation/gait/gait-rs/src/render.rs \
   /Users/aicoder/Documents/ideation/gait/gait-rs/src/ui/renderer.rs
```

No content changes — verbatim copy.

- [ ] **Step 2: Update `ui/mod.rs` with renderer re-exports**

```rust
pub mod timer;
pub mod permission;
pub mod clarify;
pub mod inform;
pub mod renderer;

pub use timer::Timer;
pub use permission::prompt_permission;
pub use clarify::prompt_clarify;
pub use inform::prompt_inform;
pub use renderer::{Renderer, RenderAction, split_into_blocks};
```

- [ ] **Step 3: Run renderer tests via ui path**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs
cargo test ui::renderer 2>&1 > /tmp/gait-test-output.txt; echo "exit: $?"
grep "FAILED\|test result\|^error" /tmp/gait-test-output.txt | head -20
```

Expected: all renderer tests pass under the new path.

- [ ] **Step 4: Commit**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs
git add src/ui/renderer.rs src/ui/mod.rs
git commit -m "feat: ui/renderer.rs — copy Renderer into ui module"
```

---

## Task 6: Wire `chat.rs` and `lib.rs`, delete `render.rs`

**Files:**
- Modify: `gait-rs/src/chat.rs`
- Modify: `gait-rs/src/lib.rs`
- Delete: `gait-rs/src/render.rs`

- [ ] **Step 1: Update `lib.rs`**

Remove `pub mod render;` and confirm `pub mod ui;` and `pub mod repl_input;` are present (added in Tasks 1 and 4). The final relevant section of `lib.rs` should look like:

```rust
pub mod render;    // ← REMOVE this line
pub mod ui;        // already added in Task 1
pub mod repl_input; // already added in Task 4
pub mod chat;
```

- [ ] **Step 2: Update imports at top of `chat.rs`**

Replace the current import block at the top of `chat.rs` (lines 1–22) with:

```rust
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use rustyline::Editor;
use rustyline::history::DefaultHistory;
use rustyline::error::ReadlineError;
use rustyline::{EventHandler, KeyCode, KeyEvent, Modifiers};

use crate::repl_helper::{ReplHelper, ShellNavHandler, ShellNavState};
use crate::session::transcript::{
    self, PermissionItem, PermissionsReplyItem,
    ClarifyItem, InformItem,
};
use crate::proto::{events, wire};
use crate::theme::{Theme, RESET, DIM_FG};
use crate::ui::{Timer, Renderer, RenderAction, prompt_permission, prompt_clarify, prompt_inform};
use crate::repl_input::{
    is_bang_command, bang_command_str, expand_quote_reply,
    expand_at_markers, run_and_capture_pty_raw, CaptureResult,
};
```

- [ ] **Step 3: Remove extracted free functions from `chat.rs`**

Delete these functions entirely from `chat.rs` (they now live in their respective modules):
- `parse_permission_response` (lines 113–132)
- `prompt_clarify_items` (lines 134–152)
- `prompt_inform_items` (lines 154–175)
- `prompt_single_permission` (lines 177–209)
- `is_bang_command` (lines 38–40)
- `bang_command_str` (lines 42–44)
- `looks_like_path` (lines 318–328)
- `CaptureResult` enum (lines 247–250)
- `run_and_capture_pty_raw` (lines 253–316)
- `expand_shell_marker` (lines 330–355)
- `expand_at_markers` (lines 357–419)
- `expand_quote_reply` (lines 56–88)
- `resolve_quote_range` (lines 90–111)

Also delete these test modules from `chat.rs` (moved to `repl_input.rs`):
- `mod expand_tests` (lines 879–951)
- `mod quote_reply_tests` (lines 953–1032)

And remove these tests from `mod tests` (moved to `repl_input.rs`):
- `bang_command_detected`
- `bang_command_extracts_cmd`
- `pty_capture_normal_command`
- `pty_capture_detects_alt_screen`

And remove these tests from `mod tests` (moved to `ui/permission.rs`):
- `parse_permission_response_number`
- `parse_permission_response_once`
- `parse_permission_response_reject`
- `parse_permission_response_invalid`

- [ ] **Step 4: Replace the inline spinner with Timer in `run_chat`**

Find the `'spawn: loop` body in `run_chat`. Replace the spinner block and its usage (lines 576–610 approximately):

Current:
```rust
let mut turn_complete_blocks: Option<Vec<String>> = None;
let spinning = Arc::new(AtomicBool::new(true));
let spin_flag = spinning.clone();
let spin_thread = thread::spawn(move || {
    let frames = ['⠋','⠙','⠹','⠸','⠼','⠴','⠦','⠧','⠇','⠏'];
    let mut i = 0usize;
    while spin_flag.load(Ordering::Relaxed) {
        eprint!("\r  {} thinking…", frames[i % frames.len()]);
        let _ = std::io::stderr().flush();
        thread::sleep(Duration::from_millis(80));
        i += 1;
    }
    eprint!("\r\x1b[2K");
    let _ = std::io::stderr().flush();
});

let mut spinner_cleared = false;
for json_line in reader.lines() {
    if !spinner_cleared {
        spinning.store(false, Ordering::Relaxed);
        eprint!("\r\x1b[2K");
        let _ = std::io::stderr().flush();
        spinner_cleared = true;
    }
    match renderer.handle_line(&json_line?) {
        RenderAction::PermissionNeeded(items) => pending_permissions = items,
        RenderAction::ClarifyNeeded(items) => pending_clarify = items,
        RenderAction::InformNeeded(items) => pending_inform = items,
        RenderAction::TurnComplete(blocks) => turn_complete_blocks = Some(blocks),
        _ => {}
    }
}
renderer.flush();
spinning.store(false, Ordering::Relaxed);
spin_thread.join().ok();
```

Replace with:
```rust
let mut turn_complete_blocks: Option<Vec<String>> = None;
let mut timer = Timer::start();
for json_line in reader.lines() {
    timer.stop(); // idempotent — clears line on first output
    match renderer.handle_line(&json_line?) {
        RenderAction::PermissionNeeded(items) => pending_permissions = items,
        RenderAction::ClarifyNeeded(items) => pending_clarify = items,
        RenderAction::InformNeeded(items) => pending_inform = items,
        RenderAction::TurnComplete(blocks) => turn_complete_blocks = Some(blocks),
        _ => {}
    }
}
renderer.flush();
timer.stop(); // ensure cleared if no lines arrived
```

- [ ] **Step 5: Update `prompt_single_permission` call sites in `chat.rs`**

In the permission loop inside `run_chat`, replace `prompt_single_permission` with `prompt_permission`:

```rust
// before
let choice = prompt_single_permission(item, label.as_deref(), &theme);

// after
let choice = prompt_permission(item, label.as_deref(), &theme);
```

- [ ] **Step 6: Update clarify/inform call sites in `run_chat`**

```rust
// before
let reply_items = prompt_clarify_items(&pending_clarify, &theme);
// after
let reply_items = prompt_clarify(&pending_clarify, &theme);

// before
let reply_items = prompt_inform_items(&pending_inform, &theme);
// after
let reply_items = prompt_inform(&pending_inform, &theme);
```

- [ ] **Step 7: Update Renderer and RenderAction call sites in `run_chat`**

```rust
// before
let mut renderer = crate::render::Renderer::new(theme.clone());
// after
let mut renderer = Renderer::new(theme.clone());
```

The `RenderAction::*` variants are already in scope via the `use crate::ui::{..., RenderAction, ...}` import added in Step 2.

- [ ] **Step 8: Update `expand_at_markers` call site in `run_chat`**

Find the `expand_at_markers` call in the main input loop of `run_chat`. It currently calls the local function. After the import added in Step 2, it calls `expand_at_markers` from `repl_input` directly (already in scope via the `use crate::repl_input::...` import).

No call-site changes needed — same function name, now imported rather than local.

Same for `is_bang_command`, `bang_command_str`, `expand_quote_reply`.

- [ ] **Step 9: Delete `render.rs`**

```bash
rm /Users/aicoder/Documents/ideation/gait/gait-rs/src/render.rs
```

- [ ] **Step 10: Run full test suite**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs
cargo test 2>&1 > /tmp/gait-test-output.txt; echo "exit: $?"
grep "^error" /tmp/gait-test-output.txt | head -30
grep "FAILED\|test result" /tmp/gait-test-output.txt
```

Expected: all tests pass, no compile errors.

- [ ] **Step 11: Confirm `chat.rs` line count dropped**

```bash
wc -l /Users/aicoder/Documents/ideation/gait/gait-rs/src/chat.rs
```

Expected: under 450 lines.

- [ ] **Step 12: Commit**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs
git add src/chat.rs src/lib.rs
git rm src/render.rs
git commit -m "refactor: wire ui/ and repl_input into chat.rs; delete render.rs"
```

---

## Self-Review

**Spec coverage:**
- ✅ `ui/timer.rs` with `Timer` — Task 1
- ✅ `ui/renderer.rs` moved from `render.rs` — Task 5
- ✅ `ui/permission.rs` extracted — Task 2
- ✅ `ui/clarify.rs` extracted — Task 3
- ✅ `ui/inform.rs` extracted — Task 3
- ✅ `ui/mod.rs` with re-exports — Tasks 1–5
- ✅ `repl_input.rs` with all input helpers — Task 4
- ✅ `render.rs` deleted — Task 6
- ✅ `chat.rs` reduced to orchestration loop — Task 6
- ✅ `lib.rs` updated — Tasks 1, 4, 6
- ✅ Timer replaces braille spinner (new behavior) — Task 6 Step 4
- ✅ All tests move with their functions — Tasks 2–4

**Placeholder scan:** None found. All code blocks are complete.

**Type consistency:**
- `Timer::start() -> Timer`, `timer.stop(&mut self)` — consistent across Tasks 1 and 6
- `prompt_permission` — defined Task 2, called Task 6 Step 5
- `prompt_clarify` — defined Task 3, called Task 6 Step 6
- `prompt_inform` — defined Task 3, called Task 6 Step 6
- `Renderer`, `RenderAction` — re-exported from `ui/mod.rs` Task 5, imported Task 6 Step 2
- `expand_at_markers`, `is_bang_command`, `bang_command_str`, `expand_quote_reply` — defined Task 4, imported Task 6 Step 2
- `run_and_capture_pty_raw`, `CaptureResult` — defined Task 4, imported Task 6 Step 2

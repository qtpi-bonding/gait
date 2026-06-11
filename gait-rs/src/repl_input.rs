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

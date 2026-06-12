use std::io::{BufRead, BufReader, Read};
use std::process::{Command, Stdio};

pub fn is_bang_command(s: &str) -> bool {
    s.starts_with('!')
}

pub fn bang_command_str(s: &str) -> &str {
    s.strip_prefix("! ").or_else(|| s.strip_prefix('!')).unwrap_or(s)
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

/// Everything an expander needs, shared by the REPL and the CLI so both surfaces
/// expand input identically. `codebase` resolves `@path` + `$(cmd)` cwd;
/// `history` backs `$(N)` shell-history recall; `last_blocks` backs `>(N)`
/// quote-reply (empty when there is no prior turn).
pub struct ExpandCtx<'a> {
    pub codebase: &'a std::path::Path,
    pub history: &'a std::path::Path,
    pub last_blocks: &'a [String],
}

/// Single pass over `input`, dispatching on the first sigil at each position:
///   `$(cmd)` / `$(N)` → shell run / history recall
///   `>(N)`            → quote the prior turn's block N
///   `@path`           → inline a file's contents
/// Each matched region of the ORIGINAL input is replaced once and the scanner
/// advances past it — replacements are never re-scanned, so a `>(N)` expansion
/// (agent text) can never trigger `$(...)` shell execution (no cascade).
pub fn expand_input(input: &str, ctx: &ExpandCtx) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '$' if chars.get(i + 1) == Some(&'(') => {
                let start = i + 2;
                if let Some(rel) = chars[start..].iter().position(|&c| c == ')') {
                    let inner: String = chars[start..start + rel].iter().collect();
                    out.push_str(&expand_shell_marker(&inner, ctx.history, ctx.codebase));
                    i = start + rel + 1;
                    continue;
                }
                out.push('$');
                i += 1;
            }
            '>' if chars.get(i + 1) == Some(&'(') => {
                let start = i + 2;
                if let Some(rel) = chars[start..].iter().position(|&c| c == ')') {
                    let range: String = chars[start..start + rel].iter().collect();
                    let range = range.trim();
                    if range.chars().all(|c| c.is_ascii_digit() || c == '-') {
                        if let Some(q) = resolve_quote_range(range, ctx.last_blocks) {
                            out.push_str(&q);
                            i = start + rel + 1;
                            continue;
                        }
                    }
                }
                out.push('>');
                i += 1;
            }
            '@' => {
                let token_start = i + 1;
                let token_end = chars[token_start..]
                    .iter()
                    .position(|c| c.is_whitespace())
                    .map(|p| token_start + p)
                    .unwrap_or(chars.len());
                let token: String = chars[token_start..token_end].iter().collect();
                if looks_like_path(&token) {
                    out.push_str(&expand_file_marker(&token, ctx.codebase));
                    i = token_end;
                    continue;
                }
                out.push('@');
                i += 1;
            }
            c => { out.push(c); i += 1; }
        }
    }
    out
}

/// Inline a file referenced by `@token` (path relative to `codebase`, or
/// absolute / `~`-rooted as-is). Errors are surfaced inline, not fatal.
fn expand_file_marker(token: &str, codebase: &std::path::Path) -> String {
    let path = if token.starts_with('/') || token.starts_with('~') {
        std::path::PathBuf::from(token)
    } else {
        codebase.join(token)
    };
    match std::fs::read_to_string(&path) {
        Ok(contents) => format!("[file: {}]\n{}", token, contents.trim_end()),
        Err(e) => format!("[file: {} — error: {}]", token, e),
    }
}

/// Reserved REPL built-in prefixes that must NOT be treated as skill names.
const BUILTIN_PREFIXES: &[&str] = &["n", "new", "q", "exit", "quit", "u", "uc", "ut"];

/// Parse a REPL line as a skill-toggle command.
///
/// Returns `Some((skill_name, enabled))` when the line is exactly:
///   `/skillname`        → enable  (enabled = true)
///   `/skillname off`    → disable (enabled = false)
///
/// Returns `None` for any other input, including:
///   - lines not starting with `/`
///   - reserved built-in commands (`/n`, `/q`, `/u`, `/uc`, `/ut`, `/exit`)
///   - bare `/` with no name
///   - names containing `/` (path-like) or characters outside `[a-zA-Z0-9_-]`
///   - three or more whitespace-separated tokens
pub(crate) fn parse_skill_toggle(line: &str) -> Option<(String, bool)> {
    let rest = line.strip_prefix('/')?;
    // Bare slash
    if rest.is_empty() { return None; }

    // Split on whitespace: expect 1 or 2 tokens
    let mut tokens = rest.split_whitespace();
    let name = tokens.next()?;
    let modifier = tokens.next(); // "off" or absent
    // Reject any further tokens
    if tokens.next().is_some() { return None; }

    // Reserved built-in names
    if BUILTIN_PREFIXES.contains(&name) { return None; }

    // Validate name: alphanumeric, hyphen, underscore only
    if !name.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_') {
        return None;
    }

    let enabled = match modifier {
        None => true,
        Some(m) if m.eq_ignore_ascii_case("off") => false,
        _ => return None, // unrecognised modifier
    };

    Some((name.to_string(), enabled))
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

    /// Expand with no prior-turn blocks — the `$()`/`@` cases don't need them.
    fn expand_no_blocks(input: &str, history: &std::path::Path, codebase: &std::path::Path) -> String {
        expand_input(input, &ExpandCtx { codebase, history, last_blocks: &[] })
    }

    #[test]
    fn passthrough_plain_text() {
        let dir = TempDir::new().unwrap();
        let hist = dir.path().join("hist.jsonl");
        assert_eq!(expand_no_blocks("hello world", &hist, dir.path()), "hello world");
    }

    #[test]
    fn expands_command_inline() {
        let dir = TempDir::new().unwrap();
        let hist = dir.path().join("hist.jsonl");
        let result = expand_no_blocks("files: $(echo hi)", &hist, dir.path());
        assert!(result.contains("[shell: echo hi]"), "got: {result}");
        assert!(result.contains("hi"), "got: {result}");
    }

    #[test]
    fn expands_history_recall() {
        let dir = TempDir::new().unwrap();
        let hist = make_history(dir.path());
        let result = expand_no_blocks("prev: $(1)", &hist, dir.path());
        assert!(result.contains("[shell: ls .]"), "got: {result}");
        assert!(result.contains("file_a"), "got: {result}");
    }

    #[test]
    fn expands_file_path() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("notes.txt"), "hello file").unwrap();
        let hist = dir.path().join("hist.jsonl");
        let result = expand_no_blocks("read this: @notes.txt", &hist, dir.path());
        assert!(result.contains("[file: notes.txt]"), "got: {result}");
        assert!(result.contains("hello file"), "got: {result}");
    }

    #[test]
    fn ignores_at_username() {
        let dir = TempDir::new().unwrap();
        let hist = dir.path().join("hist.jsonl");
        assert_eq!(expand_no_blocks("ping @alice today", &hist, dir.path()), "ping @alice today");
    }

    #[test]
    fn at_paren_no_longer_expands() {
        let dir = TempDir::new().unwrap();
        let hist = dir.path().join("hist.jsonl");
        let result = expand_no_blocks("old: @(echo hi)", &hist, dir.path());
        assert!(result.contains("@(echo hi)"), "got: {result}");
    }

    #[test]
    fn multiple_expansions() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.txt"), "aaa").unwrap();
        let hist = dir.path().join("hist.jsonl");
        let result = expand_no_blocks("x=$(echo x) y=@a.txt", &hist, dir.path());
        assert!(result.contains("[shell: echo x]"), "got: {result}");
        assert!(result.contains("[file: a.txt]"), "got: {result}");
    }

    #[test]
    fn no_cascade_quote_then_shell() {
        // A prior block that contains a `$(...)` sigil must NOT be re-scanned or
        // executed when `>(1)` expands it — the replacement is emitted verbatim.
        let dir = TempDir::new().unwrap();
        let hist = dir.path().join("hist.jsonl");
        let blocks = vec!["$(echo pwned)".to_string()];
        let result = expand_input(">(1)",
            &ExpandCtx { codebase: dir.path(), history: &hist, last_blocks: &blocks });
        assert!(result.contains("$(echo pwned)"), "literal sigil preserved: {result}");
        assert!(!result.contains("[shell:"), "must not execute the quoted sigil: {result}");
    }
}

#[cfg(test)]
mod quote_reply_tests {
    use super::{expand_input, ExpandCtx};
    use std::path::Path;

    fn blocks() -> Vec<String> {
        vec![
            "First paragraph here.".into(),
            "Second paragraph here.".into(),
            "Third paragraph here.".into(),
        ]
    }

    /// Quote-reply inputs never use `$()`/`@`, so codebase/history are inert.
    fn q(input: &str, blocks: &[String]) -> String {
        expand_input(input, &ExpandCtx { codebase: Path::new("/x"), history: Path::new("/x/h"), last_blocks: blocks })
    }

    #[test]
    fn no_prefix_passes_through() {
        assert_eq!(q("hello", &blocks()), "hello");
    }

    #[test]
    fn bare_gt_passes_through() {
        assert_eq!(q(">", &blocks()), ">");
    }

    #[test]
    fn gt_without_parens_passes_through() {
        assert_eq!(q(">2", &blocks()), ">2");
    }

    #[test]
    fn gt_paren_non_number_passes_through() {
        assert_eq!(q(">(foo)", &blocks()), ">(foo)");
    }

    #[test]
    fn unclosed_paren_passes_through() {
        assert_eq!(q(">(2", &blocks()), ">(2");
    }

    #[test]
    fn single_block_quoted() {
        assert_eq!(q(">(2)", &blocks()), "> Second paragraph here.");
    }

    #[test]
    fn inline_mid_sentence() {
        let result = q("regarding >(2) — what did you mean?", &blocks());
        assert!(result.contains("> Second paragraph here."), "got: {result}");
        assert!(result.contains("regarding"), "got: {result}");
        assert!(result.contains("what did you mean?"), "got: {result}");
    }

    #[test]
    fn single_block_with_trailing_text() {
        assert_eq!(q(">(1) follow up", &blocks()), "> First paragraph here. follow up");
    }

    #[test]
    fn range_quoted() {
        let result = q(">(1-2)", &blocks());
        assert!(result.contains("> First paragraph here."), "got: {result}");
        assert!(result.contains("> Second paragraph here."), "got: {result}");
        assert!(!result.contains("Third"), "got: {result}");
    }

    #[test]
    fn out_of_range_passes_through() {
        assert_eq!(q(">(99)", &blocks()), ">(99)");
    }

    #[test]
    fn zero_passes_through() {
        assert_eq!(q(">(0)", &blocks()), ">(0)");
    }

    #[test]
    fn empty_blocks_passes_through() {
        assert_eq!(q(">(1)", &[]), ">(1)");
    }
}

#[cfg(test)]

mod skill_toggle_tests {
    use super::parse_skill_toggle;

    #[test]
    fn enable_bare_name() {
        assert_eq!(parse_skill_toggle("/tdd"), Some(("tdd".to_string(), true)));
    }

    #[test]
    fn enable_with_trailing_whitespace() {
        assert_eq!(parse_skill_toggle("/tdd  "), Some(("tdd".to_string(), true)));
    }

    #[test]
    fn disable_with_off() {
        assert_eq!(parse_skill_toggle("/tdd off"), Some(("tdd".to_string(), false)));
    }

    #[test]
    fn disable_with_off_uppercase() {
        assert_eq!(parse_skill_toggle("/tdd OFF"), Some(("tdd".to_string(), false)));
    }

    #[test]
    fn disable_with_extra_whitespace() {
        assert_eq!(parse_skill_toggle("/tdd   off"), Some(("tdd".to_string(), false)));
    }

    #[test]
    fn plain_message_returns_none() {
        assert_eq!(parse_skill_toggle("hello world"), None);
    }

    #[test]
    fn known_builtins_return_none() {
        assert_eq!(parse_skill_toggle("/n"), None);
        assert_eq!(parse_skill_toggle("/new"), None);
        assert_eq!(parse_skill_toggle("/q"), None);
        assert_eq!(parse_skill_toggle("/exit"), None);
        assert_eq!(parse_skill_toggle("/u"), None);
        assert_eq!(parse_skill_toggle("/uc"), None);
        assert_eq!(parse_skill_toggle("/ut"), None);
    }

    #[test]
    fn bare_slash_returns_none() {
        assert_eq!(parse_skill_toggle("/"), None);
    }

    #[test]
    fn invalid_chars_in_name_return_none() {
        assert_eq!(parse_skill_toggle("/foo/bar"), None);
        assert_eq!(parse_skill_toggle("/foo bar baz"), None);
    }

    #[test]
    fn hyphen_underscore_allowed() {
        assert_eq!(parse_skill_toggle("/my-skill"), Some(("my-skill".to_string(), true)));
        assert_eq!(parse_skill_toggle("/my_skill"), Some(("my_skill".to_string(), true)));
    }
}

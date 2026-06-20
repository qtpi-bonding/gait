use crate::pty::{run_and_capture_pty_raw, CaptureResult};

fn looks_like_path(token: &str) -> bool {
    if token.is_empty() {
        return false;
    }
    if token.starts_with('/') || token.starts_with("~/") {
        return true;
    }
    if token.starts_with("./") || token.starts_with("../") {
        return true;
    }
    if token.contains('/') {
        return true;
    }
    if let Some(dot) = token.rfind('.') {
        let ext = &token[dot + 1..];
        return !ext.is_empty() && ext.chars().all(|c| c.is_alphanumeric());
    }
    false
}

pub fn resolve_quote_range(range: &str, blocks: &[String]) -> Option<String> {
    let selected: Vec<&str> = if let Some((s, e)) = range.split_once('-') {
        let start: usize = s.trim().parse().ok().filter(|&n: &usize| n > 0)?;
        let end: usize = e.trim().parse().ok().filter(|&n: &usize| n > 0)?;
        if end < start {
            return None;
        }
        let v: Vec<&str> = blocks
            .iter()
            .enumerate()
            .filter(|(i, _)| *i + 1 >= start && *i < end)
            .map(|(_, b)| b.as_str())
            .collect();
        if v.is_empty() {
            return None;
        }
        v
    } else {
        let n: usize = range.trim().parse().ok().filter(|&n: &usize| n > 0)?;
        vec![blocks.get(n - 1)?]
    };
    let quoted = selected
        .iter()
        .flat_map(|b| b.lines().map(|l| format!("> {}", l)))
        .collect::<Vec<_>>()
        .join("\n");
    Some(quoted)
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
            c => {
                out.push(c);
                i += 1;
            }
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
    fn expand_no_blocks(
        input: &str,
        history: &std::path::Path,
        codebase: &std::path::Path,
    ) -> String {
        expand_input(
            input,
            &ExpandCtx {
                codebase,
                history,
                last_blocks: &[],
            },
        )
    }

    #[test]
    fn passthrough_plain_text() {
        let dir = TempDir::new().unwrap();
        let hist = dir.path().join("hist.jsonl");
        assert_eq!(
            expand_no_blocks("hello world", &hist, dir.path()),
            "hello world"
        );
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
        assert_eq!(
            expand_no_blocks("ping @alice today", &hist, dir.path()),
            "ping @alice today"
        );
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
        let result = expand_input(
            ">(1)",
            &ExpandCtx {
                codebase: dir.path(),
                history: &hist,
                last_blocks: &blocks,
            },
        );
        assert!(
            result.contains("$(echo pwned)"),
            "literal sigil preserved: {result}"
        );
        assert!(
            !result.contains("[shell:"),
            "must not execute the quoted sigil: {result}"
        );
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
        expand_input(
            input,
            &ExpandCtx {
                codebase: Path::new("/x"),
                history: Path::new("/x/h"),
                last_blocks: blocks,
            },
        )
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
        assert_eq!(
            q(">(1) follow up", &blocks()),
            "> First paragraph here. follow up"
        );
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

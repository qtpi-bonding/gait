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

use std::borrow::Cow;
use rustyline::completion::Completer;
use rustyline::highlight::Highlighter;
use rustyline::hint::{Hint, Hinter};
use rustyline::validate::Validator;
use rustyline::{Context, Helper};

impl Helper for ReplHelper {}

impl Completer for ReplHelper {
    type Candidate = String;
}

impl Validator for ReplHelper {}

impl Highlighter for ReplHelper {
    fn highlight_hint<'h>(&self, hint: &'h str) -> Cow<'h, str> {
        Cow::Owned(format!("\x1b[2m{}\x1b[0m", hint))
    }
    fn highlight_char(&self, _grapheme: &str, _pos: usize, _forced: bool) -> bool {
        true
    }
}

pub struct ReplHint(pub String);

impl Hint for ReplHint {
    fn display(&self) -> &str {
        &self.0
    }
    fn completion(&self) -> Option<&str> {
        None
    }
}

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
        Some(ReplHint(format!("  \u{2190} {}", cmds.join(" \u{00b7} "))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_refs_returns_empty() {
        assert_eq!(collect_at_refs("explain this error"), Vec::<usize>::new());
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
        assert_eq!(collect_at_refs("@(2 is incomplete"), Vec::<usize>::new());
    }

    #[test]
    fn non_numeric_token_ignored() {
        assert_eq!(collect_at_refs("@(echo hi)"), Vec::<usize>::new());
    }

    #[test]
    fn zero_ref_ignored() {
        assert_eq!(collect_at_refs("@(0) is invalid"), Vec::<usize>::new());
    }

    #[test]
    fn two_digit_ref() {
        assert_eq!(collect_at_refs("@(12)"), vec![12]);
    }

    #[test]
    fn hint_shows_command_names() {
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let path = dir.path().join("shell.jsonl");
        crate::shell_history::append(&path, "cargo test", "ok", 1).unwrap();
        crate::shell_history::append(&path, "cat src/main.rs", "fn main", 2).unwrap();
        let helper = ReplHelper { shell_history_path: path };
        let history = rustyline::history::DefaultHistory::new();
        let ctx = rustyline::Context::new(&history);
        let hint = helper.hint("diff @(1) and @(2)", 18, &ctx).unwrap();
        assert!(hint.display().contains("cat src/main.rs"), "got: {}", hint.display());
        assert!(hint.display().contains("cargo test"), "got: {}", hint.display());
    }

    #[test]
    fn hint_none_when_no_refs() {
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let path = dir.path().join("shell.jsonl");
        let helper = ReplHelper { shell_history_path: path };
        let history = rustyline::history::DefaultHistory::new();
        let ctx = rustyline::Context::new(&history);
        assert!(helper.hint("plain prompt no refs", 20, &ctx).is_none());
    }

    #[test]
    fn hint_none_when_history_empty() {
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let path = dir.path().join("shell.jsonl");
        let helper = ReplHelper { shell_history_path: path };
        let history = rustyline::history::DefaultHistory::new();
        let ctx = rustyline::Context::new(&history);
        assert!(helper.hint("explain @(1)", 12, &ctx).is_none());
    }
}

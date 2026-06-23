//! reedline hinter: `@(N)` shell-recall preview, falling back to fish-style
//! history autosuggestion.
use reedline::{Hinter, History, SearchQuery};
use std::path::PathBuf;

use crate::repl::helper::collect_at_refs;

pub struct GaitHinter {
    pub shell_history_path: PathBuf,
    pub current: String,
}

impl Hinter for GaitHinter {
    fn handle(
        &mut self,
        line: &str,
        _pos: usize,
        history: &dyn History,
        _use_ansi_coloring: bool,
        _cwd: &str,
    ) -> String {
        // 1. Complete `@(N)` token → preview the recalled command.
        let refs = collect_at_refs(line);
        if let Some(&n) = refs.last() {
            if let Ok(Some(entry)) = gait_core::shell_history::recall(&self.shell_history_path, n) {
                self.current = String::new();
                return format!("  \u{2190} {}", entry.cmd);
            }
        }
        // 2. Else fish-style: suggest the most recent history line with this prefix.
        if line.is_empty() {
            self.current = String::new();
            return String::new();
        }
        let query = SearchQuery::last_with_prefix(line.to_string(), history.session());
        if let Ok(items) = history.search(query) {
            if let Some(item) = items.first() {
                let suffix = item.command_line.get(line.len()..).unwrap_or("");
                self.current = suffix.to_string();
                return suffix.to_string();
            }
        }
        self.current = String::new();
        String::new()
    }

    fn complete_hint(&self) -> String {
        self.current.clone()
    }

    fn next_hint_token(&self) -> String {
        // Accept one whitespace-delimited token of the suggestion.
        let mut it = self.current.splitn(2, ' ');
        match it.next() {
            Some(tok) if !tok.is_empty() => tok.to_string(),
            _ => String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reedline::Hinter;

    #[test]
    fn at_ref_hint_shows_recalled_command() {
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let path = dir.path().join("shell.jsonl");
        gait_core::shell_history::append(&path, "cargo test", "ok", 1).unwrap();

        let mut hinter = GaitHinter {
            shell_history_path: path,
            current: String::new(),
        };
        let hist = reedline::FileBackedHistory::new(10).unwrap();
        let hint = hinter.handle("explain @(1)", 12, &hist, false, "");
        assert!(hint.contains("cargo test"), "got: {hint:?}");
    }

    #[test]
    fn no_at_ref_returns_empty_when_history_empty() {
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let path = dir.path().join("shell.jsonl");
        let mut hinter = GaitHinter {
            shell_history_path: path,
            current: String::new(),
        };
        let hist = reedline::FileBackedHistory::new(10).unwrap();
        assert_eq!(hinter.handle("plain prompt", 12, &hist, false, ""), "");
    }
}

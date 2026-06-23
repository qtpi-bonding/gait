//! reedline highlighter with gait-semantic validation feedback.
use nu_ansi_term::{Color, Style};
use reedline::{Highlighter, StyledText};
use std::path::{Path, PathBuf};

// Expose nu_ansi_term publicly so tests can access Color and Style if needed
pub use nu_ansi_term;

#[derive(Debug, PartialEq, Eq)]
pub enum Kind {
    Plain,
    Command,
    Bang,
    RefOk,
    RefBad,
}

/// Split `line` into classified spans. `@(N)` is RefOk if shell_history has an
/// entry at N, else RefBad. A leading `/word` is Command; a leading `!` is Bang.
pub(crate) fn classify(line: &str, shell_path: &Path, _codebase: &Path) -> Vec<(Kind, String)> {
    if let Some(rest) = line.strip_prefix('!') {
        return vec![
            (Kind::Bang, "!".to_string()),
            (Kind::Plain, rest.to_string()),
        ];
    }
    if line.starts_with('/') {
        let end = line.find(' ').unwrap_or(line.len());
        let mut out = vec![(Kind::Command, line[..end].to_string())];
        if end < line.len() {
            out.push((Kind::Plain, line[end..].to_string()));
        }
        return out;
    }
    // Walk, splitting out `@(N)` tokens and classifying by recall success.
    let mut out = Vec::new();
    let bytes = line.as_bytes();
    let mut i = 0;
    let mut plain_start = 0;
    while i < bytes.len() {
        if bytes[i] == b'@' && i + 1 < bytes.len() && bytes[i + 1] == b'(' {
            if let Some(close) = line[i + 2..].find(')') {
                let inner = &line[i + 2..i + 2 + close];
                if let Ok(n) = inner.trim().parse::<usize>() {
                    if plain_start < i {
                        out.push((Kind::Plain, line[plain_start..i].to_string()));
                    }
                    let tok = line[i..i + 2 + close + 1].to_string();
                    let ok = matches!(gait_core::shell_history::recall(shell_path, n), Ok(Some(_)));
                    out.push((if ok { Kind::RefOk } else { Kind::RefBad }, tok));
                    i = i + 2 + close + 1;
                    plain_start = i;
                    continue;
                }
            }
        }
        i += 1;
    }
    if plain_start < line.len() {
        out.push((Kind::Plain, line[plain_start..].to_string()));
    }
    if out.is_empty() {
        out.push((Kind::Plain, line.to_string()));
    }
    out
}

pub struct GaitHighlighter {
    pub shell_history_path: PathBuf,
    pub codebase: PathBuf,
}

impl Highlighter for GaitHighlighter {
    fn highlight(&self, line: &str, _cursor: usize) -> StyledText {
        let mut st = StyledText::new();
        for (kind, text) in classify(line, &self.shell_history_path, &self.codebase) {
            let style = match kind {
                Kind::Plain => Style::new(),
                Kind::Command => Style::new().fg(Color::Cyan),
                Kind::Bang => Style::new().fg(Color::Green),
                Kind::RefOk => Style::new().fg(Color::Blue),
                Kind::RefBad => Style::new().fg(Color::Red),
            };
            st.push((style, text));
        }
        st
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn out_of_range_ref_is_refbad() {
        use tempfile::tempdir;
        let d = tempdir().unwrap();
        let shell = d.path().join("shell.jsonl");
        gait_core::shell_history::append(&shell, "ls", "x", 1).unwrap();
        let segs = classify("see @(9)", &shell, d.path());
        assert!(
            segs.iter()
                .any(|(k, t)| *k == Kind::RefBad && t.contains("@(9)")),
            "got {segs:?}"
        );
    }

    #[test]
    fn in_range_ref_is_refok() {
        use tempfile::tempdir;
        let d = tempdir().unwrap();
        let shell = d.path().join("shell.jsonl");
        gait_core::shell_history::append(&shell, "ls", "x", 1).unwrap();
        let segs = classify("see @(1)", &shell, d.path());
        assert!(
            segs.iter()
                .any(|(k, t)| *k == Kind::RefOk && t.contains("@(1)")),
            "got {segs:?}"
        );
    }

    #[test]
    fn slash_command_is_command_kind() {
        let d = std::env::temp_dir();
        let segs = classify("/new", Path::new("/nope"), &d);
        assert!(
            segs.iter().any(|(k, _)| *k == Kind::Command),
            "got {segs:?}"
        );
    }
}

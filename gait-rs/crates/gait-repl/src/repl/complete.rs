//! reedline completer sourced from live gait state.
use reedline::{Completer, Span, Suggestion};
use std::path::PathBuf;

const SLASH_COMMANDS: &[(&str, &str)] = &[
    ("/new", "start a new session"),
    ("/workflow", "run a named workflow"),
    ("/undo", "undo last turn (both axes)"),
    ("/uc", "undo code only"),
    ("/ut", "undo transcript only"),
    ("/exit", "quit"),
];

pub struct GaitCompleter {
    pub gait_dir: PathBuf,
    pub codebase: PathBuf,
    pub shell_history_path: PathBuf,
}

fn suggestion(value: String, desc: &str, span: Span) -> Suggestion {
    Suggestion {
        value,
        description: Some(desc.to_string()),
        style: None,
        extra: None,
        span,
        append_whitespace: true,
        display_override: None,
        match_indices: None,
    }
}

impl Completer for GaitCompleter {
    fn complete(&mut self, line: &str, pos: usize) -> Vec<Suggestion> {
        let head = &line[..pos];

        // `/workflow <prefix>` → yaml names under <gait-dir>/workflows/
        if let Some(rest) = head.strip_prefix("/workflow ") {
            let start = pos - rest.len();
            let span = Span::new(start, pos);
            let mut out = Vec::new();
            if let Ok(rd) = std::fs::read_dir(self.gait_dir.join("workflows")) {
                for e in rd.flatten() {
                    let name = e.file_name().to_string_lossy().to_string();
                    if let Some(stem) = name.strip_suffix(".yaml") {
                        if stem.starts_with(rest) {
                            out.push(suggestion(stem.to_string(), "workflow", span));
                        }
                    }
                }
            }
            return out;
        }

        // leading `/` → slash commands
        if head.starts_with('/') && !head.contains(' ') {
            let span = Span::new(0, pos);
            return SLASH_COMMANDS
                .iter()
                .filter(|(c, _)| c.starts_with(head))
                .map(|(c, d)| suggestion(c.to_string(), d, span))
                .collect();
        }

        // `@<prefix>` at the end → codebase file paths (one directory level)
        if let Some(at) = head.rfind('@') {
            let frag = &head[at + 1..];
            if !frag.starts_with('(') {
                let span = Span::new(at + 1, pos);
                let mut out = Vec::new();
                if let Ok(rd) = std::fs::read_dir(&self.codebase) {
                    for e in rd.flatten() {
                        let name = e.file_name().to_string_lossy().to_string();
                        if name.starts_with(frag) {
                            out.push(suggestion(name, "path", span));
                        }
                    }
                }
                return out;
            }
        }

        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reedline::Completer;

    fn completer(dir: std::path::PathBuf) -> GaitCompleter {
        GaitCompleter {
            gait_dir: dir.clone(),
            codebase: dir.clone(),
            shell_history_path: dir.join("shell.jsonl"),
        }
    }

    #[test]
    fn slash_prefix_completes_commands() {
        use tempfile::tempdir;
        let d = tempdir().unwrap();
        let mut c = completer(d.path().to_path_buf());
        let sugg = c.complete("/n", 2);
        assert!(
            sugg.iter().any(|s| s.value == "/new"),
            "got {:?}",
            sugg.iter().map(|s| &s.value).collect::<Vec<_>>()
        );
    }

    #[test]
    fn workflow_arg_completes_yaml_names() {
        use tempfile::tempdir;
        let d = tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("workflows")).unwrap();
        std::fs::write(
            d.path().join("workflows").join("deploy.yaml"),
            "name: deploy",
        )
        .unwrap();
        let mut c = completer(d.path().to_path_buf());
        let sugg = c.complete("/workflow de", 12);
        assert!(
            sugg.iter().any(|s| s.value == "deploy"),
            "got {:?}",
            sugg.iter().map(|s| &s.value).collect::<Vec<_>>()
        );
    }
}

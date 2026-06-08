use regex::Regex;

use crate::types::PermissionRequest;

/// The user's decision when prompted about a pending tool call.
#[derive(Debug, Clone, PartialEq)]
pub enum PermissionChoice {
    /// Save pattern to gait.jsonc (persists across sessions).
    Global(String),
    /// Save pattern to agent frontmatter (scoped to this agent file).
    Agent(String),
    /// Approve once via temp file (single turn only).
    Once(String),
    /// Deny — model receives "permission denied" and can react.
    Reject,
}

/// Generate allow-list patterns for a file path, from broadest to most specific.
///
/// path_patterns("read", "src/tools/edit.rs")
///   => ["read", "read:src/*", "read:src/tools/*", "read:src/tools/edit.rs"]
///
/// The first entry is the backward-compat catch-all: bare tool name approves any call.
pub fn path_patterns(tool_name: &str, rel_path: &str) -> Vec<String> {
    let mut patterns = vec![tool_name.to_string()];
    let path = std::path::Path::new(rel_path);
    let mut ancestors: Vec<&std::path::Path> = path.ancestors().skip(1).collect();
    ancestors.reverse();
    for ancestor in ancestors {
        if ancestor.as_os_str().is_empty() { continue; }
        patterns.push(format!("{}:{}/*", tool_name, ancestor.display()));
    }
    patterns.push(format!("{}:{}", tool_name, rel_path));
    patterns
}

/// Check whether a single PermissionRequest is covered by the allow list.
///
/// Approved if any allow-list entry is:
///   - the bare tool name ("read" approves any read request), OR
///   - "tool_name:pattern" where pattern wildcard-matches req.display
///
/// In addition to wildcard_match semantics, a pattern ending in "/*" matches
/// anything directly under the directory prefix (e.g. "src/*" matches "src/main.rs").
pub fn is_request_approved(tool_name: &str, req: &PermissionRequest, allow: &[String]) -> bool {
    allow.iter().any(|entry| {
        if !entry.contains(':') {
            return entry == tool_name;
        }
        if let Some((prefix, pat)) = entry.split_once(':') {
            if prefix == tool_name {
                // Handle path/* directory wildcard: "src/*" matches anything under src/
                if let Some(dir) = pat.strip_suffix("/*") {
                    return req.display.starts_with(&format!("{}/", dir));
                }
                return wildcard_match(pat, &req.display);
            }
        }
        false
    })
}

/// Split a shell command on unquoted operators: ||, &&, |, ;, &
/// v1: naive regex — does not handle quoted strings (v2 TODO)
pub fn split_commands(cmd: &str) -> Vec<String> {
    let re = Regex::new(r"\|\||&&|[|;&]").unwrap();
    re.split(cmd)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Generate progressive wildcard prefix patterns for a single (non-compound) shell command.
/// No flag stripping — all tokens included.
///
/// "cargo test --verbose" → ["cargo *", "cargo test *", "cargo test --verbose"]
pub fn command_patterns(cmd: &str) -> Vec<String> {
    let tokens: Vec<&str> = cmd.split_whitespace().collect();
    if tokens.is_empty() {
        return vec![];
    }
    let mut patterns = Vec::new();
    for i in 1..tokens.len() {
        patterns.push(format!("{} *", tokens[..i].join(" ")));
    }
    patterns.push(tokens.join(" "));
    patterns
}

/// Match a pattern against a value.
/// "cargo *" matches "cargo", "cargo test", "cargo test --verbose" but not "cargox".
pub fn wildcard_match(pattern: &str, value: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if let Some(prefix) = pattern.strip_suffix(" *") {
        value == prefix || value.starts_with(&format!("{} ", prefix))
    } else {
        pattern == value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_simple_semicolon() {
        assert_eq!(split_commands("ls; pwd"), vec!["ls", "pwd"]);
    }

    #[test]
    fn split_pipe() {
        assert_eq!(split_commands("cat foo | grep bar"), vec!["cat foo", "grep bar"]);
    }

    #[test]
    fn split_and_and() {
        assert_eq!(split_commands("cargo build && cargo test"), vec!["cargo build", "cargo test"]);
    }

    #[test]
    fn split_or_or() {
        assert_eq!(split_commands("foo || bar"), vec!["foo", "bar"]);
    }

    #[test]
    fn split_background() {
        assert_eq!(split_commands("sleep 5 & echo done"), vec!["sleep 5", "echo done"]);
    }

    #[test]
    fn split_single_command() {
        assert_eq!(split_commands("cargo test"), vec!["cargo test"]);
    }

    #[test]
    fn split_empty() {
        let result = split_commands("");
        assert!(result.is_empty(), "got: {:?}", result);
    }

    #[test]
    fn patterns_single_token() {
        assert_eq!(command_patterns("ls"), vec!["ls"]);
    }

    #[test]
    fn patterns_two_tokens() {
        assert_eq!(command_patterns("git status"), vec!["git *", "git status"]);
    }

    #[test]
    fn patterns_multi_token() {
        let got = command_patterns("cargo test --verbose");
        assert_eq!(got, vec!["cargo *", "cargo test *", "cargo test --verbose"]);
    }

    #[test]
    fn patterns_flags_included() {
        let got = command_patterns("cargo test --verbose --package gait");
        assert_eq!(got, vec![
            "cargo *",
            "cargo test *",
            "cargo test --verbose *",
            "cargo test --verbose --package *",
            "cargo test --verbose --package gait",
        ]);
    }

    #[test]
    fn wildcard_exact_match() {
        assert!(wildcard_match("git push", "git push"));
        assert!(!wildcard_match("git push", "git pull"));
    }

    #[test]
    fn wildcard_star_suffix() {
        assert!(wildcard_match("cargo *", "cargo test"));
        assert!(wildcard_match("cargo *", "cargo test --verbose"));
        assert!(wildcard_match("cargo *", "cargo"));
        assert!(!wildcard_match("cargo *", "cargox"));
    }

    #[test]
    fn wildcard_multi_word_prefix() {
        assert!(wildcard_match("git push *", "git push origin main"));
        assert!(!wildcard_match("git push *", "git pull origin main"));
        assert!(wildcard_match("git push *", "git push"));
    }

    #[test]
    fn path_patterns_single_file_at_root() {
        let pats = path_patterns("read", "README.md");
        assert_eq!(pats, vec!["read", "read:README.md"]);
    }

    #[test]
    fn path_patterns_nested_file() {
        let pats = path_patterns("write", "src/tools/edit.rs");
        assert_eq!(pats, vec![
            "write",
            "write:src/*",
            "write:src/tools/*",
            "write:src/tools/edit.rs",
        ]);
    }

    #[test]
    fn is_request_approved_bare_tool_name_allows_all() {
        use crate::types::PermissionRequest;
        let req = PermissionRequest { display: "src/main.rs".into(), patterns: vec![] };
        assert!(is_request_approved("read", &req, &["read".to_string()]));
    }

    #[test]
    fn is_request_approved_exact_path() {
        use crate::types::PermissionRequest;
        let req = PermissionRequest { display: "src/main.rs".into(), patterns: vec![] };
        assert!(is_request_approved("read", &req, &["read:src/main.rs".to_string()]));
        assert!(!is_request_approved("read", &req, &["read:src/lib.rs".to_string()]));
    }

    #[test]
    fn is_request_approved_wildcard_path() {
        use crate::types::PermissionRequest;
        let req = PermissionRequest { display: "src/main.rs".into(), patterns: vec![] };
        assert!(is_request_approved("read", &req, &["read:src/*".to_string()]));
        assert!(!is_request_approved("read", &req, &["read:tests/*".to_string()]));
    }

    #[test]
    fn is_request_approved_shell_pattern() {
        use crate::types::PermissionRequest;
        let req = PermissionRequest { display: "cargo test".into(), patterns: vec![] };
        assert!(is_request_approved("shell", &req, &["shell:cargo *".to_string()]));
        assert!(is_request_approved("shell", &req, &["shell:cargo test".to_string()]));
        assert!(!is_request_approved("shell", &req, &["shell:git *".to_string()]));
    }

    #[test]
    fn is_request_approved_wrong_tool_name() {
        use crate::types::PermissionRequest;
        let req = PermissionRequest { display: "src/main.rs".into(), patterns: vec![] };
        assert!(!is_request_approved("read", &req, &["write".to_string()]));
    }
}

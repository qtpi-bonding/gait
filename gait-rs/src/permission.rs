use regex::Regex;
use std::path::Path;

use crate::types::{PermissionRequest, ToolPermission};

/// The user's decision when prompted about a pending tool call.
#[derive(Debug, Clone, PartialEq)]
pub enum PermissionChoice {
    /// Save pattern to gait.jsonc (persists across sessions).
    Global(String),
    /// Save pattern to agent frontmatter (scoped to this agent file).
    Agent(String),
    /// Approve once (log-derived; no temp file).
    Once(String),
    /// Approve for the rest of this session (log-derived; no config file).
    Session(String),
    /// Deny — model receives "permission denied" and can react.
    Reject,
}

/// Persist a `PermissionChoice` to its backing store.
///
/// * `Global(pattern)` → appends to `<gait_dir>/gait.jsonc`
/// * `Agent(pattern)` → appends to the agent's frontmatter at `agent_path`
/// * `Once(pattern)`  → no side effect; the grant is recorded as a
///   `PermissionsReply{scope:"once"}` log event by chat.rs and replayed by
///   the `ActivePermissions` projection.
/// * `Session(pattern)` → no side effect; the grant is recorded as a
///   `PermissionsReply{scope:"session"}` log event by chat.rs and replayed
///   by the `ActivePermissions` projection.
/// * `Reject`         → no side effect
pub fn save_choice(
    choice: &PermissionChoice,
    gait_dir: &Path,
    _session_id: &str,
    agent_path: Option<&Path>,
) -> anyhow::Result<()> {
    match choice {
        PermissionChoice::Global(pattern) => {
            crate::config::append_allow_pattern(gait_dir, pattern)?;
        }
        PermissionChoice::Agent(pattern) => {
            if let Some(path) = agent_path {
                crate::agent_frontmatter::append_agent_allow(path.to_path_buf(), pattern)?;
            }
        }
        PermissionChoice::Once(_pattern) => {
            // No side effect — derived from the PermissionsReply(once) log event.
        }
        PermissionChoice::Session(_pattern) => {
            // No side effect — derived from the PermissionsReply(session) log event.
        }
        PermissionChoice::Reject => {}
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub enum ApproveScope {
    Global,
    Agent,
    Once,
}

#[derive(Debug, Clone)]
pub struct ApproveParams {
    pub scope: ApproveScope,
    pub pattern: Option<String>,
}

/// Execute an approval against a session's pending permissions.
pub fn execute_approve(
    gait_dir: &std::path::Path,
    session_id: &str,
    params: &ApproveParams,
    agent_path: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    let tx_path = gait_dir.join("sessions").join(session_id).join("log.jsonl");
    let state = crate::session::transcript::session_state(&tx_path)?;

    let items = match &state {
        crate::session::transcript::SessionState::AwaitingPermission { items } => items.clone(),
        _ => return Err(anyhow::anyhow!("no pending permissions for session {}", session_id)),
    };

    let mut reply_items: Vec<crate::session::transcript::PermissionsReplyItem> = Vec::new();

    for item in &items {
        let effective_pattern = params.pattern.clone()
            .unwrap_or_else(|| item.patterns.first().cloned().unwrap_or_else(|| item.tool_name.clone()));

        let req = crate::types::PermissionRequest {
            display: item.cmd.clone(),
            patterns: item.patterns.clone(),
        };

        // When --pattern is supplied, only approve if that pattern would cover the request.
        // When --pattern is absent, default to approving every pending item.
        let covered = match params.pattern.as_ref() {
            Some(p) => is_request_approved(&item.tool_name, &req, &[p.clone()]),
            None => true,
        };

        if covered {
            let choice = match &params.scope {
                ApproveScope::Global => PermissionChoice::Global(effective_pattern.clone()),
                ApproveScope::Agent => PermissionChoice::Agent(effective_pattern.clone()),
                ApproveScope::Once => PermissionChoice::Once(effective_pattern.clone()),
            };
            save_choice(&choice, gait_dir, session_id, agent_path)?;
            let scope_str = match &params.scope {
                ApproveScope::Global => "global",
                ApproveScope::Agent => "agent",
                ApproveScope::Once => "once",
            };
            reply_items.push(crate::session::transcript::PermissionsReplyItem {
                id: item.id.clone(),
                decision: "approve".to_string(),
                text: None,
                scope: Some(scope_str.to_string()),
            });
        } else {
            reply_items.push(crate::session::transcript::PermissionsReplyItem {
                id: item.id.clone(),
                decision: "deny".to_string(),
                text: None,
                scope: None,
            });
        }
    }

    crate::session::transcript::write_permissions_reply(&tx_path, "user", reply_items)?;
    Ok(())
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

/// Self-describing permission for an MCP tool: addressable by its own name,
/// by its server (`mcp:<server>`), or by the exact tool (`mcp:<server>:<tool>`).
pub fn mcp_permission(server: &str, tool: &str) -> ToolPermission {
    ToolPermission::RequiresApproval(vec![PermissionRequest {
        display: tool.to_string(),
        patterns: vec![
            tool.to_string(),
            format!("mcp:{}", server),
            format!("mcp:{}:{}", server, tool),
        ],
    }])
}

/// Find the first allow-list entry that would approve this request.
/// Returns `None` if nothing in `allow` covers the request.
pub fn find_matched_allow_pattern(tool_name: &str, req: &PermissionRequest, allow: &[String]) -> Option<String> {
    allow.iter().find(|entry| {
        if !entry.contains(':') {
            return entry.as_str() == tool_name;
        }
        if let Some((prefix, pat)) = entry.split_once(':') {
            if prefix == tool_name {
                if let Some(dir) = pat.strip_suffix("/*") {
                    return req.display.starts_with(&format!("{}/", dir));
                }
                return wildcard_match(pat, &req.display);
            }
        }
        false
    }).cloned()
}

/// Find the first deny-list entry that would deny this request.
/// Returns `None` if nothing in `deny` covers the request.
pub fn find_matched_deny_pattern(tool_name: &str, req: &PermissionRequest, deny: &[String]) -> Option<String> {
    deny.iter().find(|entry| {
        if !entry.contains(':') {
            return entry.as_str() == tool_name;
        }
        if let Some((prefix, pat)) = entry.split_once(':') {
            if prefix == tool_name {
                if let Some(dir) = pat.strip_suffix("/*") {
                    return req.display.starts_with(&format!("{}/", dir));
                }
                return wildcard_match(pat, &req.display);
            }
        }
        false
    }).cloned()
}

/// Check whether a single PermissionRequest is denied by the deny list.
///
/// Denied if any deny-list entry is:
///   - the bare tool name ("write" denies any write request), OR
///   - "tool_name:pattern" where pattern wildcard-matches req.display
///
/// In addition to wildcard_match semantics, a pattern ending in "/*" matches
/// anything directly under the directory prefix (e.g. "src/*" matches "src/main.rs").
pub fn is_request_denied(tool_name: &str, req: &PermissionRequest, deny: &[String]) -> bool {
    deny.iter().any(|entry| {
        // A deny entry that exactly equals one of the request's self-described
        // patterns matches directly. Lets any Loadable classifier (e.g. MCP's
        // `mcp:<server>`) gate without relying on the tool_name prefix.
        if req.patterns.iter().any(|p| p == entry) { return true; }
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
        if req.patterns.iter().any(|p| p == entry) { return true; }
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
    #[test]
    fn is_request_denied_bare_tool_name() {
        use crate::types::PermissionRequest;
        let req = PermissionRequest { display: "src/main.rs".into(), patterns: vec![] };
        assert!(is_request_denied("write", &req, &["write".to_string()]));
        assert!(!is_request_denied("read", &req, &["write".to_string()]));
    }

    #[test]
    fn is_request_denied_shell_pattern() {
        use crate::types::PermissionRequest;
        let req = PermissionRequest { display: "rm -rf /".into(), patterns: vec![] };
        assert!(is_request_denied("shell", &req, &["shell:rm *".to_string()]));
        assert!(!is_request_denied("shell", &req, &["shell:ls *".to_string()]));
    }

    #[test]
    fn find_matched_deny_pattern_returns_match() {
        use crate::types::PermissionRequest;
        let req = PermissionRequest { display: "rm -rf /".into(), patterns: vec![] };
        let result = find_matched_deny_pattern("shell", &req, &["shell:rm *".to_string()]);
        assert_eq!(result, Some("shell:rm *".to_string()));
    }

    #[test]
    fn find_matched_deny_pattern_returns_none_when_no_match() {
        use crate::types::PermissionRequest;
        let req = PermissionRequest { display: "ls -la".into(), patterns: vec![] };
        let result = find_matched_deny_pattern("shell", &req, &["shell:rm *".to_string()]);
        assert_eq!(result, None);
    }

    #[test]
    fn deny_wins_over_allow_building_blocks() {
        // Verify that when a pattern appears in both allow and deny,
        // both functions fire — the classification loop is what enforces deny-wins
        // by checking deny before allow.
        use crate::types::PermissionRequest;
        let req = PermissionRequest { display: "rm -rf /".into(), patterns: vec![] };
        let both = vec!["shell:rm *".to_string()];
        assert!(is_request_denied("shell", &req, &both));
        assert!(is_request_approved("shell", &req, &both));
        // Both return true — ordering is the loop's job.
    }
    #[test]
    fn deny_matches_self_described_pattern_directly() {
        use crate::types::PermissionRequest;
        let req = PermissionRequest {
            display: "list_repos".into(),
            patterns: vec!["list_repos".into(), "mcp:github".into(), "mcp:github:list_repos".into()],
        };
        assert!(is_request_denied("list_repos", &req, &["mcp:github".into()]));
        assert!(!is_request_denied("list_repos", &req, &["mcp:gitlab".into()]));
    }

    #[test]
    fn allow_matches_self_described_pattern_directly() {
        use crate::types::PermissionRequest;
        let req = PermissionRequest {
            display: "list_repos".into(),
            patterns: vec!["list_repos".into(), "mcp:github".into(), "mcp:github:list_repos".into()],
        };
        assert!(is_request_approved("list_repos", &req, &["mcp:github:list_repos".into()]));
    }

    #[test]
    fn mcp_permission_emits_server_namespaced_patterns() {
        match mcp_permission("github", "list_repos") {
            ToolPermission::RequiresApproval(reqs) => {
                assert_eq!(reqs.len(), 1);
                assert_eq!(reqs[0].patterns,
                    vec!["list_repos".to_string(), "mcp:github".into(), "mcp:github:list_repos".into()]);
            }
            _ => panic!("expected RequiresApproval"),
        }
    }
}

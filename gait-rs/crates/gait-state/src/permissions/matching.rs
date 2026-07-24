use crate::types::{PermissionRequest, ToolPermission};
use gait_types::config::SandboxWhitelistEntry;
use gait_types::sandbox_grant::SandboxGrant;

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
    Deny,
}

// moved to permissions::persist (Task 3): save_choice + compute_allow_decisions
#[derive(Debug, Clone)]
pub enum AllowScope {
    Global,
    Agent,
    Session,
    Once,
}

#[derive(Debug, Clone)]
pub struct AllowParams {
    pub scope: AllowScope,
    pub pattern: Option<String>,
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
        if ancestor.as_os_str().is_empty() {
            continue;
        }
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

/// Test whether a single allow/deny-list entry matches a (tool_name, req) pair.
///
/// This is the shared predicate for `find_matched_allow_pattern`,
/// `find_matched_deny_pattern`, `is_request_approved`, and `is_request_denied`.
/// An entry matches if any of these hold:
///   - it equals one of `req.patterns` (self-described by the request, lets MCP-style
///     classifiers gate without relying on the tool_name prefix), OR
///   - it is the bare tool name (e.g. "read" matches any read request), OR
///   - it is "tool_name:pattern" where:
///       - pattern ending in "/*" is a directory prefix match (e.g. "src/*" matches
///         anything directly under "src/"), OR
///       - pattern wildcard-matches `req.display` (see `wildcard_match`).
fn matches_pattern_entry(tool_name: &str, req: &PermissionRequest, entry: &str) -> bool {
    if req.patterns.iter().any(|p| p == entry) {
        return true;
    }
    if !entry.contains(':') {
        return entry == tool_name;
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
}

/// Find the first allow-list entry that would approve this request.
/// Returns `None` if nothing in `allow` covers the request.
pub fn find_matched_allow_pattern(
    tool_name: &str,
    req: &PermissionRequest,
    allow: &[String],
) -> Option<String> {
    allow
        .iter()
        .find(|entry| matches_pattern_entry(tool_name, req, entry))
        .cloned()
}

/// Find the first deny-list entry that would deny this request.
/// Returns `None` if nothing in `deny` covers the request.
pub fn find_matched_deny_pattern(
    tool_name: &str,
    req: &PermissionRequest,
    deny: &[String],
) -> Option<String> {
    deny.iter()
        .find(|entry| matches_pattern_entry(tool_name, req, entry))
        .cloned()
}

/// Check whether a single PermissionRequest is denied by the deny list.
///
/// Denied if any deny-list entry matches the request (see `matches_pattern_entry`
/// for the matching rules, including self-described patterns and directory wildcards).
pub fn is_request_denied(tool_name: &str, req: &PermissionRequest, deny: &[String]) -> bool {
    deny.iter()
        .any(|entry| matches_pattern_entry(tool_name, req, entry))
}

/// Check whether a single PermissionRequest is covered by the allow list.
///
/// Approved if any allow-list entry matches the request (see `matches_pattern_entry`
/// for the matching rules, including self-described patterns and directory wildcards).
pub fn is_request_approved(tool_name: &str, req: &PermissionRequest, allow: &[String]) -> bool {
    allow
        .iter()
        .any(|entry| matches_pattern_entry(tool_name, req, entry))
}

/// Split a shell command on unquoted operators: ||, &&, |, ;, &
/// v1: naive scanner — does not handle quoted strings (v2 TODO). Unlike a
/// plain regex split, a bare `&` is NOT treated as an operator when it's
/// adjacent to `>` (`2>&1`, `1>&2`, `&>file`, `&>>file`) — those are fd-dup
/// redirects, not the background-job operator, and an earlier regex-only
/// version silently carved `2>&1` into two bogus segments (`"...2>"`,
/// `"1"`), which broke `whitelist_match` for the extremely common
/// `cmd 2>&1 | tail -N` pattern (every segment after the first stops being
/// a single-segment command, so no shell_whitelist entry could ever match
/// and grants like network/bind were silently withheld).
pub fn split_commands(cmd: &str) -> Vec<String> {
    scan_operators(cmd).0
}

/// True if `cmd` contains a genuine command-chaining operator (`&&`, `;`, or
/// a bare background `&`) — as opposed to only pipe operators (`|`, `||`).
/// Used by [`whitelist_match`] to decide whether it's safe to match against
/// just the first segment of a compound command: a pipeline only ever hands
/// the first command's stdout to the next stage, it can never substitute a
/// different command to run, so `cmd 2>&1 | tail -N` is exactly as
/// authorized by a whitelist entry as `cmd` alone. `&&`/`;`/`&` genuinely
/// run a separate command afterward and must keep requiring an exact
/// single-segment match (see the shell tool's `cd dir && cmd` docs for why
/// that's deliberate).
fn has_command_chaining(cmd: &str) -> bool {
    scan_operators(cmd).1
}

/// Shared scanner backing [`split_commands`] and [`has_command_chaining`].
/// Returns (non-empty trimmed segments, whether any chaining operator was seen).
fn scan_operators(cmd: &str) -> (Vec<String>, bool) {
    let bytes = cmd.as_bytes();
    let mut segments = Vec::new();
    let mut chained = false;
    let mut start = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'|' => {
                if bytes.get(i + 1) == Some(&b'|') {
                    segments.push(cmd[start..i].trim().to_string());
                    i += 2;
                } else {
                    segments.push(cmd[start..i].trim().to_string());
                    i += 1;
                }
                start = i;
            }
            b';' => {
                segments.push(cmd[start..i].trim().to_string());
                chained = true;
                i += 1;
                start = i;
            }
            b'&' => {
                if bytes.get(i + 1) == Some(&b'&') {
                    segments.push(cmd[start..i].trim().to_string());
                    chained = true;
                    i += 2;
                    start = i;
                } else {
                    let prev_is_gt = i > 0 && bytes[i - 1] == b'>';
                    let next_is_gt = bytes.get(i + 1) == Some(&b'>');
                    if prev_is_gt || next_is_gt {
                        // fd-dup redirect (2>&1, 1>&2, &>file) — not an operator.
                        i += 1;
                    } else {
                        segments.push(cmd[start..i].trim().to_string());
                        chained = true;
                        i += 1;
                        start = i;
                    }
                }
            }
            _ => i += 1,
        }
    }
    segments.push(cmd[start..].trim().to_string());
    segments.retain(|s| !s.is_empty());
    (segments, chained)
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

/// Match a shell command against `sandbox.shell_whitelist` entries. Only
/// ever called for `shell` tool calls.
///
/// A single-segment command matches directly. A multi-segment command
/// matches against its *first* segment only when every separator present is
/// a pipe (`|`/`||`) — see [`has_command_chaining`] for why that's safe:
/// `flutter test foo.dart 2>&1 | tail -60` is authorized by the same
/// `shell_whitelist` entry as `flutter test foo.dart` alone, so shaping
/// output through `tail`/`head`/`grep` (the tool's own recommended pattern
/// for large output) doesn't silently forfeit the grant. Any `&&`/`;`/`&`
/// still requires an exact single-segment match, unchanged.
pub fn whitelist_match<'a>(
    cmd: &str,
    whitelist: &'a [SandboxWhitelistEntry],
) -> Option<(&'a SandboxWhitelistEntry, Vec<SandboxGrant>)> {
    let segs = split_commands(cmd);
    let candidate: &str = match segs.len() {
        0 => return None,
        1 => cmd,
        _ => {
            if has_command_chaining(cmd) {
                return None;
            }
            segs[0].as_str()
        }
    };
    whitelist.iter().find_map(|entry| {
        wildcard_match(&entry.pattern, candidate).then(|| {
            let grants = entry
                .grants
                .iter()
                .filter_map(|dimension| SandboxGrant::from_dimension(dimension))
                .collect();
            (entry, grants)
        })
    })
}

/// Diagnostic-only: given a request `display` (e.g. "rm -rf /") and the allow
/// list, return a corrected pattern string if some allow entry is a *near-miss*
/// — it does not match today but would if its trailing `*` were the `" *"`
/// (space-star) form the request side generates. Returns `None` when no entry
/// is close. Never affects matching; purely to explain a silent non-match.
pub fn near_miss_suggestion(display: &str, allow: &[String]) -> Option<String> {
    for entry in allow {
        // entry is "tool:pat" (or bare "tool"); we only diagnose the pat side.
        let Some((tool, pat)) = entry.split_once(':') else {
            continue;
        };
        // A pattern that already matches is not a near-miss.
        if wildcard_match(pat, display) {
            continue;
        }
        // "rm*" (no space) → strip the trailing '*' → "rm"; if that is a
        // command-prefix of display, the space-star form would have matched.
        if let Some(prefix) = pat.strip_suffix('*') {
            if !prefix.ends_with(' ') && !prefix.is_empty() {
                let is_prefix = display == prefix || display.starts_with(&format!("{prefix} "));
                if is_prefix {
                    return Some(format!("{tool}:{prefix} *"));
                }
            }
        }
    }
    None
}
#[cfg(test)]
mod tests {
    use super::*;
    use gait_types::config::SandboxWhitelistEntry;
    use gait_types::sandbox_grant::SandboxGrant;

    #[test]
    fn split_simple_semicolon() {
        assert_eq!(split_commands("ls; pwd"), vec!["ls", "pwd"]);
    }

    #[test]
    fn split_pipe() {
        assert_eq!(
            split_commands("cat foo | grep bar"),
            vec!["cat foo", "grep bar"]
        );
    }

    #[test]
    fn split_and_and() {
        assert_eq!(
            split_commands("cargo build && cargo test"),
            vec!["cargo build", "cargo test"]
        );
    }

    #[test]
    fn split_or_or() {
        assert_eq!(split_commands("foo || bar"), vec!["foo", "bar"]);
    }

    #[test]
    fn split_background() {
        assert_eq!(
            split_commands("sleep 5 & echo done"),
            vec!["sleep 5", "echo done"]
        );
    }

    #[test]
    fn split_single_command() {
        assert_eq!(split_commands("cargo test"), vec!["cargo test"]);
    }

    /// `2>&1` is a bare-`&` fd-duplication redirect (merge stderr into
    /// stdout), not the background-job operator — it must stay attached to
    /// its command, not get carved into two bogus segments. This is the
    /// single most common shell idiom in agent-issued commands
    /// (`cmd 2>&1 | tail -N`), so mis-splitting it broke whitelist matching
    /// for nearly every real command.
    #[test]
    fn split_does_not_break_on_stderr_redirect() {
        assert_eq!(
            split_commands("flutter test foo.dart 2>&1 | tail -60"),
            vec!["flutter test foo.dart 2>&1", "tail -60"]
        );
    }

    #[test]
    fn split_does_not_break_on_other_fd_dup_forms() {
        assert_eq!(split_commands("cmd 1>&2"), vec!["cmd 1>&2"]);
        assert_eq!(split_commands("cmd &>out.log"), vec!["cmd &>out.log"]);
        assert_eq!(split_commands("cmd &>>out.log"), vec!["cmd &>>out.log"]);
    }

    /// A bare background `&` (not adjacent to `>`) must still split, and
    /// still count as chaining — unchanged from the old regex behavior.
    #[test]
    fn split_background_still_chains() {
        assert!(has_command_chaining("sleep 5 & echo done"));
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
        assert_eq!(
            got,
            vec![
                "cargo *",
                "cargo test *",
                "cargo test --verbose *",
                "cargo test --verbose --package *",
                "cargo test --verbose --package gait",
            ]
        );
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
        assert_eq!(
            pats,
            vec![
                "write",
                "write:src/*",
                "write:src/tools/*",
                "write:src/tools/edit.rs",
            ]
        );
    }

    #[test]
    fn is_request_approved_bare_tool_name_allows_all() {
        use crate::types::PermissionRequest;
        let req = PermissionRequest {
            display: "src/main.rs".into(),
            patterns: vec![],
        };
        assert!(is_request_approved("read", &req, &["read".to_string()]));
    }

    #[test]
    fn is_request_approved_exact_path() {
        use crate::types::PermissionRequest;
        let req = PermissionRequest {
            display: "src/main.rs".into(),
            patterns: vec![],
        };
        assert!(is_request_approved(
            "read",
            &req,
            &["read:src/main.rs".to_string()]
        ));
        assert!(!is_request_approved(
            "read",
            &req,
            &["read:src/lib.rs".to_string()]
        ));
    }

    #[test]
    fn is_request_approved_wildcard_path() {
        use crate::types::PermissionRequest;
        let req = PermissionRequest {
            display: "src/main.rs".into(),
            patterns: vec![],
        };
        assert!(is_request_approved(
            "read",
            &req,
            &["read:src/*".to_string()]
        ));
        assert!(!is_request_approved(
            "read",
            &req,
            &["read:tests/*".to_string()]
        ));
    }

    #[test]
    fn is_request_approved_shell_pattern() {
        use crate::types::PermissionRequest;
        let req = PermissionRequest {
            display: "cargo test".into(),
            patterns: vec![],
        };
        assert!(is_request_approved(
            "shell",
            &req,
            &["shell:cargo *".to_string()]
        ));
        assert!(is_request_approved(
            "shell",
            &req,
            &["shell:cargo test".to_string()]
        ));
        assert!(!is_request_approved(
            "shell",
            &req,
            &["shell:git *".to_string()]
        ));
    }

    #[test]
    fn is_request_approved_wrong_tool_name() {
        use crate::types::PermissionRequest;
        let req = PermissionRequest {
            display: "src/main.rs".into(),
            patterns: vec![],
        };
        assert!(!is_request_approved("read", &req, &["write".to_string()]));
    }
    #[test]
    fn is_request_denied_bare_tool_name() {
        use crate::types::PermissionRequest;
        let req = PermissionRequest {
            display: "src/main.rs".into(),
            patterns: vec![],
        };
        assert!(is_request_denied("write", &req, &["write".to_string()]));
        assert!(!is_request_denied("read", &req, &["write".to_string()]));
    }

    #[test]
    fn is_request_denied_shell_pattern() {
        use crate::types::PermissionRequest;
        let req = PermissionRequest {
            display: "rm -rf /".into(),
            patterns: vec![],
        };
        assert!(is_request_denied(
            "shell",
            &req,
            &["shell:rm *".to_string()]
        ));
        assert!(!is_request_denied(
            "shell",
            &req,
            &["shell:ls *".to_string()]
        ));
    }

    #[test]
    fn find_matched_deny_pattern_returns_match() {
        use crate::types::PermissionRequest;
        let req = PermissionRequest {
            display: "rm -rf /".into(),
            patterns: vec![],
        };
        let result = find_matched_deny_pattern("shell", &req, &["shell:rm *".to_string()]);
        assert_eq!(result, Some("shell:rm *".to_string()));
    }

    #[test]
    fn find_matched_deny_pattern_returns_none_when_no_match() {
        use crate::types::PermissionRequest;
        let req = PermissionRequest {
            display: "ls -la".into(),
            patterns: vec![],
        };
        let result = find_matched_deny_pattern("shell", &req, &["shell:rm *".to_string()]);
        assert_eq!(result, None);
    }

    #[test]
    fn deny_wins_over_allow_building_blocks() {
        // Verify that when a pattern appears in both allow and deny,
        // both functions fire — the classification loop is what enforces deny-wins
        // by checking deny before allow.
        use crate::types::PermissionRequest;
        let req = PermissionRequest {
            display: "rm -rf /".into(),
            patterns: vec![],
        };
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
            patterns: vec![
                "list_repos".into(),
                "mcp:github".into(),
                "mcp:github:list_repos".into(),
            ],
        };
        assert!(is_request_denied(
            "list_repos",
            &req,
            &["mcp:github".into()]
        ));
        assert!(!is_request_denied(
            "list_repos",
            &req,
            &["mcp:gitlab".into()]
        ));
    }

    #[test]
    fn allow_matches_self_described_pattern_directly() {
        use crate::types::PermissionRequest;
        let req = PermissionRequest {
            display: "list_repos".into(),
            patterns: vec![
                "list_repos".into(),
                "mcp:github".into(),
                "mcp:github:list_repos".into(),
            ],
        };
        assert!(is_request_approved(
            "list_repos",
            &req,
            &["mcp:github:list_repos".into()]
        ));
    }

    #[test]
    fn mcp_permission_emits_server_namespaced_patterns() {
        match mcp_permission("github", "list_repos") {
            ToolPermission::RequiresApproval(reqs) => {
                assert_eq!(reqs.len(), 1);
                assert_eq!(
                    reqs[0].patterns,
                    vec![
                        "list_repos".to_string(),
                        "mcp:github".into(),
                        "mcp:github:list_repos".into()
                    ]
                );
            }
            _ => panic!("expected RequiresApproval"),
        }
    }

    #[test]
    fn near_miss_flags_missing_space_star() {
        // The generated pattern is "shell:rm *" (space-star); "shell:rm*" never matches.
        let allow = vec!["shell:rm*".to_string()];
        assert_eq!(
            near_miss_suggestion("rm -rf /", &allow),
            Some("shell:rm *".to_string()),
            "a trailing '*' with no space is a near-miss for the space-star form"
        );
        // A genuinely unrelated allow entry is not a near-miss.
        assert_eq!(
            near_miss_suggestion("rm -rf /", &["shell:ls*".to_string()]),
            None
        );
        // The correct form is not a near-miss (it already matches).
        assert_eq!(
            near_miss_suggestion("rm -rf /", &["shell:rm *".to_string()]),
            None
        );
    }

    #[test]
    fn whitelist_match_single_command_and_first_match_wins() {
        let whitelist = vec![
            SandboxWhitelistEntry {
                pattern: "flutter *".into(),
                grants: vec!["read".into()],
            },
            SandboxWhitelistEntry {
                pattern: "flutter test *".into(),
                grants: vec!["network".into()],
            },
        ];
        let (entry, grants) = whitelist_match("flutter test --coverage", &whitelist).unwrap();
        assert_eq!(entry.pattern, "flutter *");
        assert_eq!(grants, vec![SandboxGrant::Readable]);
    }

    #[test]
    fn whitelist_match_rejects_compound_and_unknown_dimensions() {
        let whitelist = vec![SandboxWhitelistEntry {
            pattern: "flutter test *".into(),
            grants: vec!["network".into(), "mystery".into()],
        }];
        assert!(whitelist_match("flutter test && curl evil.example.com", &whitelist).is_none());
        let (_, grants) = whitelist_match("flutter test", &whitelist).unwrap();
        assert_eq!(grants, vec![SandboxGrant::NetworkHost]);
    }

    /// The real bug this guards against: `flutter test foo.dart 2>&1 | tail -60`
    /// is the tool's own recommended pattern for keeping large output short,
    /// and it must still get the `network`/`bind` grants a plain
    /// `flutter test foo.dart` would — piping into a read-only filter can't
    /// smuggle a different command into running, so matching on the first
    /// segment is safe.
    #[test]
    fn whitelist_match_matches_first_segment_of_a_pure_pipeline() {
        let whitelist = vec![SandboxWhitelistEntry {
            pattern: "flutter test *".into(),
            grants: vec!["network".into(), "bind".into()],
        }];
        let (entry, grants) = whitelist_match("flutter test foo.dart 2>&1 | tail -60", &whitelist)
            .expect("pipe-into-tail must still match the whitelist entry");
        assert_eq!(entry.pattern, "flutter test *");
        assert_eq!(grants, vec![SandboxGrant::NetworkHost]);
    }

    #[test]
    fn whitelist_match_still_rejects_pipeline_mixed_with_chaining() {
        let whitelist = vec![SandboxWhitelistEntry {
            pattern: "flutter test *".into(),
            grants: vec!["network".into()],
        }];
        // A `|` alone would be safe, but the trailing `&& rm -rf /` genuinely
        // chains a second command — must not match.
        assert!(whitelist_match(
            "flutter test foo.dart 2>&1 | tail -60 && rm -rf /",
            &whitelist
        )
        .is_none());
    }

    #[test]
    fn whitelist_match_returns_none_for_non_match() {
        let whitelist = vec![SandboxWhitelistEntry {
            pattern: "flutter test *".into(),
            grants: vec!["network".into()],
        }];
        assert!(whitelist_match("npm test", &whitelist).is_none());
    }
}

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use rustyline::DefaultEditor;
use rustyline::error::ReadlineError;

use crate::session::transcript::{
    self, AnyEvent, PermissionItem, PermissionsReplyItem,
    ClarifyItem, ClarifyReplyItem, InformItem, InformReplyItem,
};
use crate::theme::{Theme, RESET, DIM};

pub fn new_session_id() -> String {
    format!("s{}", &uuid::Uuid::new_v4().to_string().replace('-', "")[..8])
}

fn is_bang_command(s: &str) -> bool {
    s.starts_with('!')
}

fn bang_command_str(s: &str) -> &str {
    s.strip_prefix("! ").or_else(|| s.strip_prefix('!')).unwrap_or(s)
}

#[derive(Debug, PartialEq)]
enum PermissionChoice {
    Global(String),
    Agent(String),
    Once(String),
    Reject,
}

/// Parse one line of user input into a PermissionChoice.
/// `tool_name`: used to compute the allow-list prefix ("shell:" for shell, "" for others).
/// `has_agent`: whether --agent is active (controls whether a{N} is valid).
/// Returns None if input is invalid → caller re-prompts.
fn parse_permission_response(
    input: &str,
    patterns: &[String],
    tool_name: &str,
    has_agent: bool,
) -> Option<PermissionChoice> {
    // Shell patterns stored as "shell:<pattern>"; non-shell patterns stored bare (exact match).
    // Must match is_tool_allowed's storage format exactly.
    let prefix = if tool_name == "shell" { "shell:" } else { "" };
    match input.trim() {
        "r" | "reject" => return Some(PermissionChoice::Reject),
        "o" | "once" => {
            let pat = patterns.last()?;
            return Some(PermissionChoice::Once(format!("{}{}", prefix, pat)));
        }
        _ => {}
    }
    if let Some(rest) = input.trim().strip_prefix('g') {
        if let Ok(n) = rest.parse::<usize>() {
            if n >= 1 && n <= patterns.len() {
                return Some(PermissionChoice::Global(format!(
                    "{}{}",
                    prefix,
                    patterns[n - 1]
                )));
            }
        }
    }
    if has_agent {
        if let Some(rest) = input.trim().strip_prefix('a') {
            if let Ok(n) = rest.parse::<usize>() {
                if n >= 1 && n <= patterns.len() {
                    return Some(PermissionChoice::Agent(format!(
                        "{}{}",
                        prefix,
                        patterns[n - 1]
                    )));
                }
            }
        }
    }
    None
}

fn prompt_clarify_items(items: &[ClarifyItem], theme: &Theme) -> Vec<ClarifyReplyItem> {
    let mut replies = Vec::new();
    for item in items {
        eprintln!();
        eprintln!("{}? [{}] {}{}", theme.warning.fg(), item.kind, item.content, RESET);
        eprintln!("  {}why: {}{}", DIM, item.why, RESET);
        eprint!("> ");
        let _ = std::io::stderr().flush();
        let mut input = String::new();
        let _ = std::io::stdin().lock().read_line(&mut input);
        replies.push(ClarifyReplyItem {
            id: item.id.clone(),
            content: input.trim().to_string(),
            reasoning: None,
        });
    }
    replies
}

fn prompt_inform_items(items: &[InformItem], theme: &Theme) -> Vec<InformReplyItem> {
    let mut replies = Vec::new();
    for item in items {
        eprintln!();
        eprintln!("{}\u{2192} {}{}", theme.warning.fg(), item.content, RESET);
        if let Some(ctx) = &item.context {
            eprintln!("  {}context: {}{}", DIM, ctx, RESET);
        }
        eprintln!("  {}Press Enter to confirm, or type a correction:{}", DIM, RESET);
        eprint!("> ");
        let _ = std::io::stderr().flush();
        let mut input = String::new();
        let _ = std::io::stdin().lock().read_line(&mut input);
        let response = input.trim().to_string();
        replies.push(InformReplyItem {
            id: item.id.clone(),
            content: if response.is_empty() { "understood".into() } else { response },
        });
    }
    replies
}

fn prompt_single_permission(
    item: &PermissionItem,
    label: Option<&str>,
    has_agent: bool,
    theme: &Theme,
) -> PermissionChoice {
    eprintln!();
    if let Some(l) = label {
        eprintln!("{}\u{27f3} shell: {}  {}({}){}", theme.warning.fg(), item.cmd, DIM, l, RESET);
    } else {
        eprintln!("{}\u{27f3} shell: {}{}", theme.warning.fg(), item.cmd, RESET);
    }
    eprintln!();
    for (i, pat) in item.patterns.iter().enumerate() {
        eprintln!("  [{}] {}", i + 1, pat);
    }
    eprintln!();
    eprintln!("  [o] once   [r] reject");
    if has_agent {
        eprintln!("  scope: [a] agent  [g] global   (e.g. a2, g1, o, r)");
    } else {
        eprintln!("  g = global  (e.g. g1, g2, o, r)");
    }
    loop {
        eprint!("  > ");
        let _ = std::io::stderr().flush();
        let mut input = String::new();
        match std::io::stdin().lock().read_line(&mut input) {
            Ok(0) | Err(_) => return PermissionChoice::Reject,
            Ok(_) => {}
        }
        match parse_permission_response(input.trim(), &item.patterns, &item.tool_name, has_agent) {
            Some(c) => return c,
            None => eprintln!(
                "  unrecognized — try {}g1..g{}, o, or r",
                if has_agent {
                    format!("a1..a{}, ", item.patterns.len())
                } else {
                    String::new()
                },
                item.patterns.len()
            ),
        }
    }
}

/// Run `cmd` via `sh -c`, merging stderr into stdout (2>&1).
/// Prints each line to terminal as it arrives.
/// Returns (accumulated_output, exit_code). exit_code is None if spawn fails.
fn run_and_capture(cmd: &str, cwd: &std::path::Path) -> (String, Option<i32>) {
    let merged = format!("{} 2>&1", cmd);
    let mut child = match Command::new("sh")
        .args(["-c", &merged])
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[!] error: {e}");
            return (String::new(), None);
        }
    };
    let stdout = child.stdout.take().expect("piped stdout");
    let reader = BufReader::new(stdout);
    let mut lines: Vec<String> = Vec::new();
    for line in reader.lines() {
        match line {
            Ok(l) => {
                println!("{}", l);
                lines.push(l);
            }
            Err(_) => break,
        }
    }
    let code = child
        .wait()
        .ok()
        .and_then(|s| s.code());
    (lines.join("\n"), code)
}

pub fn run_chat(
    workspace: PathBuf,
    codebase: PathBuf,
    agent_name: Option<String>,
) -> anyhow::Result<()> {
    let gait_bin = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("gait"));
    let history_path = dirs_next::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("gait/chat_history");

    let agent_path: Option<PathBuf> = agent_name.as_deref().map(|n| {
        crate::agent_frontmatter::resolve_agent_path(&workspace, n)
    });
    let has_agent = agent_path.is_some();

    let theme = Theme::detect();

    let mut rl = DefaultEditor::new()?;
    let _ = rl.load_history(&history_path);

    let mut session_id = new_session_id();
    eprintln!(
        "{}gait{}  {}session {}{}  {}(ctrl+d to exit, /new for fresh session){}",
        theme.accent.fg(), RESET,
        theme.meta.fg(), session_id, RESET,
        DIM, RESET,
    );
    let mut shell_history_path = workspace.join("tmp").join(format!("shell-{}.jsonl", session_id));

    loop {
        let readline = rl.readline("> ");
        let line = match readline {
            Ok(l) => l,
            Err(ReadlineError::Eof) | Err(ReadlineError::Interrupted) => break,
            Err(e) => return Err(e.into()),
        };
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }
        let _ = rl.add_history_entry(&line);

        if is_bang_command(&line) {
            let cmd = bang_command_str(&line);
            let (output, code) = run_and_capture(cmd, &codebase);
            if code != Some(0) {
                eprintln!("[!] exit {}", code.unwrap_or(1));
            }
            let _ = crate::shell_history::append(
                &shell_history_path,
                cmd,
                &output,
                crate::session::transcript::now_ms(),
            );
            continue;
        }

        match line.as_str() {
            "/new" => {
                session_id = new_session_id();
                eprintln!("new session {}", session_id);
                continue;
            }
            "/exit" | "exit" | "quit" => break,
            _ => {}
        }

        let task = line.clone();

        'spawn: loop {
            let mut cmd = Command::new(&gait_bin);
            cmd.args([
                "run",
                "--session",
                &session_id,
                "--task",
                &task,
                "--workspace",
                workspace.to_str().unwrap_or("."),
                "--codebase",
                codebase.to_str().unwrap_or("."),
                // No --auto: allow list governs execution
            ]);
            if let Some(ref p) = agent_path {
                cmd.args(["--agent", p.to_str().unwrap_or("")]);
            }

            let mut child = cmd
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()?;

            let stdout = child.stdout.take().expect("piped stdout");
            let reader = BufReader::new(stdout);
            let mut renderer = crate::render::Renderer::new(theme.clone());
            let mut pending_permissions: Vec<PermissionItem> = vec![];
            let mut pending_clarify: Vec<ClarifyItem> = vec![];
            let mut pending_inform: Vec<InformItem> = vec![];

            for json_line in reader.lines() {
                match renderer.handle_line(&json_line?) {
                    crate::render::RenderAction::PermissionNeeded(items) => pending_permissions = items,
                    crate::render::RenderAction::ClarifyNeeded(items) => pending_clarify = items,
                    crate::render::RenderAction::InformNeeded(items) => pending_inform = items,
                    _ => {}
                }
            }
            renderer.flush();
            child.wait()?;

            // Handle clarify: prompt user for each question, write ClarifyReply, re-spawn
            if !pending_clarify.is_empty() {
                let tx_path = workspace.join("sessions").join(&session_id).join("transcript.jsonl");
                let reply_items = prompt_clarify_items(&pending_clarify, &theme);
                let _ = transcript::append_event(&tx_path, &AnyEvent::ClarifyReply {
                    items: reply_items,
                    ts: transcript::now_ms(),
                });
                continue 'spawn;
            }

            // Handle inform: prompt user to confirm understanding, write InformReply, re-spawn
            if !pending_inform.is_empty() {
                let tx_path = workspace.join("sessions").join(&session_id).join("transcript.jsonl");
                let reply_items = prompt_inform_items(&pending_inform, &theme);
                let _ = transcript::append_event(&tx_path, &AnyEvent::InformReply {
                    items: reply_items,
                    ts: transcript::now_ms(),
                });
                continue 'spawn;
            }

            if pending_permissions.is_empty() {
                println!();
                break 'spawn;
            }
            let pending_items = pending_permissions;

            // Group items by id for display (show compound command segments together)
            // then collect ALL decisions before writing anything
            let mut reply_items: Vec<PermissionsReplyItem> = vec![];
            let mut any_approved = false;

            for item in &pending_items {
                // For compound commands, find total segment count for this id
                let total_segs = pending_items.iter().filter(|i| i.id == item.id).count();
                let seg_idx = pending_items
                    .iter()
                    .filter(|i| i.id == item.id)
                    .position(|i| std::ptr::eq(i, item))
                    .unwrap_or(0);
                let label = if total_segs > 1 {
                    Some(format!("part {} of {}", seg_idx + 1, total_segs))
                } else {
                    None
                };

                let choice = prompt_single_permission(item, label.as_deref(), has_agent, &theme);
                match &choice {
                    PermissionChoice::Global(pat) => {
                        if let Err(e) = crate::config::append_allow_pattern(&workspace, pat) {
                            eprintln!("[gait] warning: could not save to gait.jsonc: {e}");
                        }
                        reply_items.push(PermissionsReplyItem {
                            id: item.id.clone(),
                            decision: "approve".into(),
                            text: Some(pat.clone()),
                        });
                        any_approved = true;
                    }
                    PermissionChoice::Agent(pat) => {
                        if let Some(ref p) = agent_path {
                            if let Err(e) =
                                crate::agent_frontmatter::append_agent_allow(p.clone(), pat)
                            {
                                eprintln!("[gait] warning: could not save to agent file: {e}");
                            }
                        }
                        reply_items.push(PermissionsReplyItem {
                            id: item.id.clone(),
                            decision: "approve".into(),
                            text: Some(pat.clone()),
                        });
                        any_approved = true;
                    }
                    PermissionChoice::Once(pat) => {
                        let tmp_dir = workspace.join("tmp");
                        let _ = std::fs::create_dir_all(&tmp_dir);
                        let temp = tmp_dir.join(format!("gait-approve-{}", session_id));
                        if let Ok(mut f) = std::fs::OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open(&temp)
                        {
                            let _ = writeln!(f, "{}", pat);
                        }
                        reply_items.push(PermissionsReplyItem {
                            id: item.id.clone(),
                            decision: "approve".into(),
                            text: Some(pat.clone()),
                        });
                        any_approved = true;
                    }
                    PermissionChoice::Reject => {
                        reply_items.push(PermissionsReplyItem {
                            id: item.id.clone(),
                            decision: "deny".into(),
                            text: None,
                        });
                    }
                }
            }

            // Write PermissionsReply to transcript (even on full reject — needed for denied ToolResult)
            let tx_path = workspace
                .join("sessions")
                .join(&session_id)
                .join("transcript.jsonl");
            let _ = transcript::append_event(
                &tx_path,
                &AnyEvent::PermissionsReply {
                    from: "user".into(),
                    items: reply_items,
                    ts: transcript::now_ms(),
                },
            );

            if any_approved {
                continue 'spawn;
            } else {
                eprintln!("[gait] permission denied");
                println!();
                break 'spawn;
            }
        }
    }

    let _ = rl.save_history(&history_path);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_session_id_has_expected_format() {
        let id = new_session_id();
        assert!(id.starts_with('s'), "got: {}", id);
        assert_eq!(id.len(), 9, "got: {}", id);
    }

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
    fn parse_permission_response_global_shell() {
        let patterns = vec!["cargo *".to_string(), "cargo test *".to_string()];
        assert_eq!(
            parse_permission_response("g1", &patterns, "shell", true),
            Some(PermissionChoice::Global("shell:cargo *".to_string()))
        );
    }

    #[test]
    fn parse_permission_response_global_non_shell() {
        // Non-shell tools store bare tool name in allow list (no "shell:" prefix)
        let patterns = vec!["write".to_string()];
        assert_eq!(
            parse_permission_response("g1", &patterns, "write", false),
            Some(PermissionChoice::Global("write".to_string()))
        );
    }

    #[test]
    fn parse_permission_response_agent() {
        let patterns = vec!["cargo *".to_string()];
        assert_eq!(
            parse_permission_response("a1", &patterns, "shell", true),
            Some(PermissionChoice::Agent("shell:cargo *".to_string()))
        );
        // a{N} not offered when no agent active
        assert_eq!(
            parse_permission_response("a1", &patterns, "shell", false),
            None
        );
    }

    #[test]
    fn parse_permission_response_once() {
        let patterns = vec!["cargo *".to_string(), "cargo test".to_string()];
        // Once uses the most specific (last) pattern
        assert_eq!(
            parse_permission_response("o", &patterns, "shell", false),
            Some(PermissionChoice::Once("shell:cargo test".to_string()))
        );
    }

    #[test]
    fn parse_permission_response_reject() {
        assert_eq!(
            parse_permission_response("r", &[], "shell", false),
            Some(PermissionChoice::Reject)
        );
    }

    #[test]
    fn parse_permission_response_invalid() {
        let p = vec!["cargo *".to_string()];
        assert_eq!(parse_permission_response("g99", &p, "shell", false), None);
        assert_eq!(parse_permission_response("xyz", &p, "shell", false), None);
        assert_eq!(parse_permission_response("", &p, "shell", false), None);
    }

    #[test]
    fn run_and_capture_captures_stdout() {
        let (out, code) = run_and_capture("echo hello", std::path::Path::new("."));
        assert_eq!(out.trim(), "hello");
        assert_eq!(code, Some(0));
    }

    #[test]
    fn run_and_capture_captures_stderr() {
        // Nested sh so the inner >&2 sees fd2 already pointing to the pipe (not terminal).
        // Plain "echo err >&2" would NOT work — shell-level redirects interact badly with
        // the appended 2>&1 when both are in the same sh -c string.
        let (out, code) = run_and_capture("sh -c 'echo err >&2'", std::path::Path::new("."));
        assert_eq!(out.trim(), "err");
        assert_eq!(code, Some(0));
    }

    #[test]
    fn run_and_capture_nonzero_exit() {
        let (_, code) = run_and_capture("exit 42", std::path::Path::new("."));
        assert_eq!(code, Some(42));
    }

    #[test]
    fn run_and_capture_bad_command_returns_none_code() {
        // A command that doesn't exist — sh returns exit 127
        let (_, code) = run_and_capture("_no_such_cmd_xyz_", std::path::Path::new("."));
        assert!(code != Some(0));
    }
}

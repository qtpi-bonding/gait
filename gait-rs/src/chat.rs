use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::io::Read;
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use rustyline::Editor;
use rustyline::history::DefaultHistory;

use crate::repl_helper::{ReplHelper, ShellNavHandler, ShellNavState};
use rustyline::error::ReadlineError;
use rustyline::{EventHandler, KeyCode, KeyEvent, Modifiers};
use std::sync::{Arc, Mutex};

use crate::session::transcript::{
    self, AnyEvent, PermissionItem, PermissionsReplyItem,
    ClarifyItem, ClarifyReplyItem, InformItem, InformReplyItem,
};
use crate::theme::{Theme, RESET, DIM_FG};

pub fn new_session_id() -> String {
    format!("s{}", &uuid::Uuid::new_v4().to_string().replace('-', "")[..8])
}

fn is_bang_command(s: &str) -> bool {
    s.starts_with('!')
}

fn bang_command_str(s: &str) -> &str {
    s.strip_prefix("! ").or_else(|| s.strip_prefix('!')).unwrap_or(s)
}

/// Parse one line of user input into a PermissionChoice.
/// Numbers (1..N) save to agent frontmatter. `o` = once, `r` = reject.
/// Returns None if input is invalid → caller re-prompts.
///
/// Patterns are returned verbatim — they are already correctly formatted by the
/// tool's `permission()` method (e.g. shell patterns arrive as "shell:cargo *").
/// The REPL is a pure UI layer: it must not add or strip prefixes.
fn parse_permission_response(
    input: &str,
    patterns: &[String],
) -> Option<crate::permission::PermissionChoice> {
    use crate::permission::PermissionChoice;
    match input.trim() {
        "r" | "reject" => return Some(PermissionChoice::Reject),
        "o" | "once" => {
            let pat = patterns.last()?;
            return Some(PermissionChoice::Once(pat.clone()));
        }
        _ => {}
    }
    if let Ok(n) = input.trim().parse::<usize>() {
        if n >= 1 && n <= patterns.len() {
            return Some(PermissionChoice::Agent(patterns[n - 1].clone()));
        }
    }
    None
}

fn prompt_clarify_items(items: &[ClarifyItem], theme: &Theme) -> Vec<ClarifyReplyItem> {
    let mut replies = Vec::new();
    for item in items {
        eprintln!();
        eprintln!("{}? [{}] {}{}", theme.accent.fg(), item.kind, item.content, RESET);
        eprintln!("  {}why: {}{}", DIM_FG, item.why, RESET);
        eprint!("{}>{} {}", theme.accent.fg(), RESET, theme.user_msg.fg());
        let _ = std::io::stderr().flush();
        let mut input = String::new();
        let _ = std::io::stdin().lock().read_line(&mut input);
        eprint!("{}", RESET);
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
        eprintln!("{}\u{2192} {}{}", theme.accent.fg(), item.content, RESET);
        if let Some(ctx) = &item.context {
            eprintln!("  {}context: {}{}", DIM_FG, ctx, RESET);
        }
        eprintln!("  {}Press Enter to confirm, or type a correction:{}", DIM_FG, RESET);
        eprint!("{}>{} {}", theme.accent.fg(), RESET, theme.user_msg.fg());
        let _ = std::io::stderr().flush();
        let mut input = String::new();
        let _ = std::io::stdin().lock().read_line(&mut input);
        eprint!("{}", RESET);
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
    theme: &Theme,
) -> crate::permission::PermissionChoice {
    eprintln!();
    // `item.cmd` is the tool-rendered display string (ShellTool.permission() etc.).
    // We don't add a "shell:" prefix here — the tool has already formatted the display.
    if let Some(l) = label {
        eprintln!("{}⟳ {}  {}({}){}", theme.accent.fg(), item.cmd, DIM_FG, l, RESET);
    } else {
        eprintln!("{}⟳ {}{}", theme.accent.fg(), item.cmd, RESET);
    }
    eprintln!();
    for (i, pat) in item.patterns.iter().enumerate() {
        eprintln!("  {}[{}] {}{}", DIM_FG, i + 1, pat, RESET);
    }
    eprintln!();
    eprintln!("  {}[o] once   [r] reject{}", DIM_FG, RESET);
    loop {
        eprint!("  {}>{} {}", theme.accent.fg(), RESET, theme.user_msg.fg());
        let _ = std::io::stderr().flush();
        let mut input = String::new();
        match std::io::stdin().lock().read_line(&mut input) {
            Ok(0) | Err(_) => { eprint!("{}", RESET); return crate::permission::PermissionChoice::Reject; }
            Ok(_) => { eprint!("{}", RESET); }
        }
        match parse_permission_response(input.trim(), &item.patterns) {
            Some(c) => return c,
            None => eprintln!("  unrecognized — try 1..{}, o, or r", item.patterns.len()),
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
            eprintln!("\x1b[31m[!] error: {}\x1b[0m", e);
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

#[derive(Debug)]
enum CaptureResult {
    Output(String, Option<i32>),
    Interactive,
}

fn run_and_capture_pty_raw(cmd: &str, cwd: &std::path::Path) -> CaptureResult {
    let pty_system = native_pty_system();
    let pair = match pty_system.openpty(PtySize {
        rows: 24,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
    }) {
        Ok(p) => p,
        Err(_) => {
            let (out, code) = run_and_capture(cmd, cwd);
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

fn expand_paren_marker(
    inner: &str,
    history_path: &std::path::Path,
    codebase: &std::path::Path,
) -> String {
    if let Ok(n) = inner.trim().parse::<usize>() {
        if n >= 1 {
            return match crate::shell_history::recall(history_path, n) {
                Ok(Some(entry)) => format!("[shell: {}]\n{}", entry.cmd, entry.output.trim_end()),
                Ok(None) => format!("[history @({n}) — no entry]"),
                Err(e) => format!("[history @({n}) — error: {e}]"),
            };
        }
    }
    match run_and_capture_pty_raw(inner, codebase) {
        CaptureResult::Output(output, code) => {
            if code != Some(0) {
                eprintln!("[!] @({inner}) exit {}", code.unwrap_or(1));
            }
            format!("[shell: {}]\n{}", inner, output.trim_end())
        }
        CaptureResult::Interactive => {
            format!("[interactive TUI — run @({inner}) directly in your terminal]")
        }
    }
}

pub fn expand_at_markers(
    input: &str,
    history_path: &std::path::Path,
    codebase: &std::path::Path,
) -> anyhow::Result<String> {
    let mut result = String::with_capacity(input.len());
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '@' {
            result.push(chars[i]);
            i += 1;
            continue;
        }
        let rest_start = i + 1;
        if rest_start >= chars.len() {
            result.push('@');
            i += 1;
            continue;
        }
        if chars[rest_start] == '(' {
            let inner_start = rest_start + 1;
            let close = chars[inner_start..].iter().position(|&c| c == ')');
            if let Some(rel) = close {
                let inner: String = chars[inner_start..inner_start + rel].iter().collect();
                let expanded = expand_paren_marker(&inner, history_path, codebase);
                result.push_str(&expanded);
                i = inner_start + rel + 1;
                continue;
            }
            result.push('@');
            i += 1;
            continue;
        }
        let token_end = chars[rest_start..]
            .iter()
            .position(|c| c.is_whitespace())
            .map(|p| rest_start + p)
            .unwrap_or(chars.len());
        let token: String = chars[rest_start..token_end].iter().collect();
        if looks_like_path(&token) {
            let path = if token.starts_with('/') || token.starts_with('~') {
                std::path::PathBuf::from(&token)
            } else {
                codebase.join(&token)
            };
            match std::fs::read_to_string(&path) {
                Ok(contents) => {
                    result.push_str(&format!("[file: {}]\n{}", token, contents.trim_end()));
                    i = token_end;
                    continue;
                }
                Err(e) => {
                    result.push_str(&format!("[file: {} — error: {}]", token, e));
                    i = token_end;
                    continue;
                }
            }
        }
        result.push('@');
        i += 1;
    }
    Ok(result)
}

pub fn run_chat(
    workspace: PathBuf,
    codebase: PathBuf,
    agent_name: Option<String>,
) -> anyhow::Result<()> {
    let gait_bin = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("gait"));

    let agent_path: Option<PathBuf> = agent_name.as_deref().map(|n| {
        crate::agent_frontmatter::resolve_agent_path(&workspace, n)
    });
    let theme = Theme::detect();

    let mut session_id = new_session_id();
    let mut shell_history_path = workspace.join("tmp").join(format!("shell-{}.jsonl", session_id));
    eprintln!(
        "{}gait{}  {}session {}{}  {}(ctrl+d to exit, /new for fresh session){}",
        theme.accent.fg(), RESET,
        DIM_FG, session_id, RESET,
        DIM_FG, RESET,
    );

    let nav = Arc::new(Mutex::new(ShellNavState {
        path: shell_history_path.clone(),
        index: 0,
        original_line: String::new(),
    }));

    let mut rl = Editor::<ReplHelper, DefaultHistory>::new()?;
    seed_history_from_transcripts(&mut rl, &workspace);
    rl.set_helper(Some(ReplHelper { nav: nav.clone() }));
    rl.bind_sequence(
        KeyEvent(KeyCode::Up, Modifiers::SHIFT),
        EventHandler::Conditional(Box::new(ShellNavHandler { nav: nav.clone(), up: true })),
    );
    rl.bind_sequence(
        KeyEvent(KeyCode::Down, Modifiers::SHIFT),
        EventHandler::Conditional(Box::new(ShellNavHandler { nav: nav.clone(), up: false })),
    );

    loop {
        nav.lock().unwrap().index = 0;
        let prompt = format!("{}>{} {}", theme.accent.fg(), RESET, theme.user_msg.fg());
        let readline = rl.readline(&prompt);
        eprint!("{}", RESET);
        let line = match readline {
            Ok(l) => l,
            Err(ReadlineError::Eof) | Err(ReadlineError::Interrupted) => break,
            Err(e) => return Err(e.into()),
        };
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }
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
                shell_history_path = workspace.join("tmp").join(format!("shell-{}.jsonl", session_id));
                nav.lock().unwrap().path = shell_history_path.clone();
                eprintln!("new session {}", session_id);
                continue;
            }
            "/exit" | "exit" | "quit" => break,
            _ => {}
        }

        let _ = rl.add_history_entry(&line);

        let task: String = match expand_at_markers(&line, &shell_history_path, &codebase) {
            Ok(expanded) => expanded,
            Err(e) => {
                eprintln!("[!] @ expand error: {e}");
                continue;
            }
        };

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

                let choice = prompt_single_permission(item, label.as_deref(), &theme);
                // Persist the choice to its backing store (gait.jsonc / agent file / once-file).
                // The REPL is a pure UI layer — all persistence lives in `crate::permission`.
                if let Err(e) = crate::permission::save_choice(
                    &choice,
                    &workspace,
                    &session_id,
                    agent_path.as_deref(),
                ) {
                    eprintln!("[gait] warning: could not save permission: {e}");
                }
                match &choice {
                    crate::permission::PermissionChoice::Once(pat) => {
                        reply_items.push(PermissionsReplyItem {
                            id: item.id.clone(),
                            decision: "approve".into(),
                            text: Some(pat.clone()),
                            scope: Some("once".into()),
                        });
                        any_approved = true;
                    }
                    crate::permission::PermissionChoice::Agent(pat) => {
                        reply_items.push(PermissionsReplyItem {
                            id: item.id.clone(),
                            decision: "approve".into(),
                            text: Some(pat.clone()),
                            scope: Some("agent".into()),
                        });
                        any_approved = true;
                    }
                    crate::permission::PermissionChoice::Global(pat) => {
                        reply_items.push(PermissionsReplyItem {
                            id: item.id.clone(),
                            decision: "approve".into(),
                            text: Some(pat.clone()),
                            scope: Some("global".into()),
                        });
                        any_approved = true;
                    }
                    crate::permission::PermissionChoice::Reject => {
                        reply_items.push(PermissionsReplyItem {
                            id: item.id.clone(),
                            decision: "deny".into(),
                            text: None,
                            scope: None,
                        });
                    }
                }
            }

            // Emit the reply event through the shared transcript helper.
            // (Even on full reject — the model needs a denied ToolResult.)
            let tx_path = workspace
                .join("sessions")
                .join(&session_id)
                .join("transcript.jsonl");
            let _ = transcript::write_permissions_reply(&tx_path, "user", reply_items);

            if any_approved {
                continue 'spawn;
            } else {
                eprintln!("[gait] permission denied");
                println!();
                break 'spawn;
            }
        }
    }

    Ok(())
}

fn seed_history_from_transcripts(
    rl: &mut Editor<ReplHelper, DefaultHistory>,
    workspace: &std::path::Path,
) {
    let sessions_dir = workspace.join("sessions");
    let Ok(entries) = std::fs::read_dir(&sessions_dir) else { return };

    let mut sessions: Vec<(std::time::SystemTime, std::path::PathBuf)> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let tx = e.path().join("transcript.jsonl");
            let mtime = std::fs::metadata(&tx).ok()?.modified().ok()?;
            Some((mtime, tx))
        })
        .collect();

    sessions.sort_by_key(|(mtime, _)| *mtime);

    for (_, tx_path) in &sessions {
        let Ok(events) = crate::session::transcript::read_events(tx_path) else { continue };
        for event in events {
            if let crate::session::transcript::AnyEvent::UserTurn { content, .. } = event {
                let _ = rl.add_history_entry(&content);
            }
        }
    }
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
    fn parse_permission_response_number() {
        let patterns = vec!["shell:cargo *".to_string(), "shell:cargo test *".to_string()];
        assert_eq!(
            parse_permission_response("1", &patterns),
            Some(crate::permission::PermissionChoice::Agent("shell:cargo *".to_string()))
        );
        assert_eq!(
            parse_permission_response("2", &patterns),
            Some(crate::permission::PermissionChoice::Agent("shell:cargo test *".to_string()))
        );
    }

    #[test]
    fn parse_permission_response_once() {
        let patterns = vec!["shell:cargo *".to_string(), "shell:cargo test".to_string()];
        assert_eq!(
            parse_permission_response("o", &patterns),
            Some(crate::permission::PermissionChoice::Once("shell:cargo test".to_string()))
        );
    }

    #[test]
    fn parse_permission_response_reject() {
        assert_eq!(
            parse_permission_response("r", &[]),
            Some(crate::permission::PermissionChoice::Reject)
        );
    }

    #[test]
    fn parse_permission_response_invalid() {
        let p = vec!["shell:cargo *".to_string()];
        assert_eq!(parse_permission_response("99", &p), None);
        assert_eq!(parse_permission_response("xyz", &p), None);
        assert_eq!(parse_permission_response("", &p), None);
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
        assert!(
            matches!(result, CaptureResult::Interactive),
            "expected Interactive"
        );
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

    #[test]
    fn passthrough_plain_text() {
        let dir = TempDir::new().unwrap();
        let hist = dir.path().join("hist.jsonl");
        let result = expand_at_markers("hello world", &hist, dir.path()).unwrap();
        assert_eq!(result, "hello world");
    }

    #[test]
    fn expands_command_inline() {
        let dir = TempDir::new().unwrap();
        let hist = dir.path().join("hist.jsonl");
        let result = expand_at_markers("files: @(echo hi)", &hist, dir.path()).unwrap();
        assert!(result.contains("[shell: echo hi]"), "got: {result}");
        assert!(result.contains("hi"), "got: {result}");
    }

    #[test]
    fn expands_history_recall() {
        let dir = TempDir::new().unwrap();
        let hist = make_history(dir.path());
        let result = expand_at_markers("prev: @(1)", &hist, dir.path()).unwrap();
        assert!(result.contains("[shell: ls .]"), "got: {result}");
        assert!(result.contains("file_a"), "got: {result}");
    }

    #[test]
    fn expands_file_path() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("notes.txt"), "hello file").unwrap();
        let hist = dir.path().join("hist.jsonl");
        let result = expand_at_markers("read this: @notes.txt", &hist, dir.path()).unwrap();
        assert!(result.contains("[file: notes.txt]"), "got: {result}");
        assert!(result.contains("hello file"), "got: {result}");
    }

    #[test]
    fn ignores_at_username() {
        let dir = TempDir::new().unwrap();
        let hist = dir.path().join("hist.jsonl");
        let result = expand_at_markers("ping @alice today", &hist, dir.path()).unwrap();
        assert_eq!(result, "ping @alice today");
    }

    #[test]
    fn multiple_expansions() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.txt"), "aaa").unwrap();
        let hist = dir.path().join("hist.jsonl");
        let result = expand_at_markers("x=@(echo x) y=@a.txt", &hist, dir.path()).unwrap();
        assert!(result.contains("[shell: echo x]"), "got: {result}");
        assert!(result.contains("[file: a.txt]"), "got: {result}");
    }
}

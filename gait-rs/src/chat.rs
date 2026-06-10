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

/// All user-visible hint strings for the REPL, centralised here for easy editing.
mod hints {
    pub const STARTUP: &str =
        "ctrl+d or /q to exit  \u{2022}  /n new session  \u{2022}  /u [N] undo  \u{2022}  !cmd shell";
    pub const UNDO_USAGE: &str =
        "/u [N]   undo code+transcript  (list turns if no N)\n\
         /uc [N]  undo code only\n\
         /ut [N]  undo transcript only";
}

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
/// Expand `>(N)`, `>(N-M)`, or `>(N) message` quote-reply syntax.
/// Returns the expanded string, or the original unchanged if no match.
/// Inline scanner for `>(N)` / `>(N-M)` quote-reply. Works anywhere in the string.
fn expand_quote_reply(input: &str, blocks: &[String]) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut result = String::with_capacity(input.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '>' && chars.get(i + 1) == Some(&'(') {
            let inner_start = i + 2;
            if let Some(rel) = chars[inner_start..].iter().position(|&c| c == ')') {
                let range: String = chars[inner_start..inner_start + rel].iter().collect();
                let range_str = range.trim();
                // Only attempt expansion if the range looks numeric (not arbitrary text).
                let looks_numeric = range_str.chars().all(|c| c.is_ascii_digit() || c == '-');
                if looks_numeric {
                    match resolve_quote_range(range_str, blocks) {
                        Some(expanded) => {
                            result.push_str(&expanded);
                            i = inner_start + rel + 1;
                            continue;
                        }
                        None => {
                            eprintln!("[gait] >({}): block out of range (last response had {} block{})",
                                range_str, blocks.len(),
                                if blocks.len() == 1 { "" } else { "s" });
                        }
                    }
                }
            }
        }
        result.push(chars[i]);
        i += 1;
    }
    result
}

fn resolve_quote_range(range: &str, blocks: &[String]) -> Option<String> {
    let selected: Vec<&str> = if let Some((s, e)) = range.split_once('-') {
        let start: usize = s.trim().parse().ok().filter(|&n: &usize| n > 0)?;
        let end: usize = e.trim().parse().ok().filter(|&n: &usize| n > 0)?;
        if end < start { return None; }
        let v: Vec<&str> = blocks.iter()
            .enumerate()
            .filter(|(i, _)| *i + 1 >= start && *i + 1 <= end)
            .map(|(_, b)| b.as_str())
            .collect();
        if v.is_empty() { return None; }
        v
    } else {
        let n: usize = range.trim().parse().ok().filter(|&n: &usize| n > 0)?;
        vec![blocks.get(n - 1)?]
    };
    let quoted = selected.iter()
        .flat_map(|b| b.lines().map(|l| format!("> {}", l)))
        .collect::<Vec<_>>()
        .join("\n");
    Some(quoted)
}

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

fn expand_shell_marker(
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

pub fn expand_at_markers(
    input: &str,
    history_path: &std::path::Path,
    codebase: &std::path::Path,
) -> anyhow::Result<String> {
    let mut result = String::with_capacity(input.len());
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '$' if chars.get(i + 1) == Some(&'(') => {
                // $(cmd) or $(N) — shell execution or history recall
                let inner_start = i + 2;
                if let Some(rel) = chars[inner_start..].iter().position(|&c| c == ')') {
                    let inner: String = chars[inner_start..inner_start + rel].iter().collect();
                    result.push_str(&expand_shell_marker(&inner, history_path, codebase));
                    i = inner_start + rel + 1;
                    continue;
                }
                result.push('$');
                i += 1;
            }
            '@' => {
                // @file — file contents only (no parens)
                let token_start = i + 1;
                if token_start < chars.len() {
                    let token_end = chars[token_start..]
                        .iter()
                        .position(|c| c.is_whitespace())
                        .map(|p| token_start + p)
                        .unwrap_or(chars.len());
                    let token: String = chars[token_start..token_end].iter().collect();
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
                }
                result.push('@');
                i += 1;
            }
            c => {
                result.push(c);
                i += 1;
            }
        }
    }
    Ok(result)
}

pub fn run_chat(
    workspace: PathBuf,
    codebase: PathBuf,
    agent_name: Option<String>,
) -> anyhow::Result<()> {
    let gait_bin = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("gait"));

    let agent_path: PathBuf = match agent_name.as_deref() {
        Some(n) => crate::agent_frontmatter::resolve_agent_path(&workspace, n),
        None => workspace.join("agent.md"),
    };
    let theme = Theme::detect();

    let mut session_id = new_session_id();
    let mut shell_history_path = workspace.join("tmp").join(format!("shell-{}.jsonl", session_id));
    eprintln!(
        "{}gait{}  {}session {}{}  {}{}{}",
        theme.accent.fg(), RESET,
        DIM_FG, session_id, RESET,
        DIM_FG, hints::STARTUP, RESET,
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

    let mut last_turn_blocks: Vec<String> = Vec::new();

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
            "/n" | "/new" => {
                session_id = new_session_id();
                shell_history_path = workspace.join("tmp").join(format!("shell-{}.jsonl", session_id));
                nav.lock().unwrap().path = shell_history_path.clone();
                eprintln!("new session {}", session_id);
                continue;
            }
            "/q" | "/exit" | "exit" | "quit" => break,
            _ => {}
        }

        // /u [N], /uc [N], /ut [N]: undo both axes, code-only, or transcript-only.
        // Check longest prefix first so /uc and /ut are not consumed by /u.
        let line: String = if line.starts_with("/uc") || line.starts_with("/ut") || line.starts_with("/u") {
            let (flags, label, arg_start) = if line.starts_with("/uc") {
                (["--code"].as_slice(), "code", 3usize)
            } else if line.starts_with("/ut") {
                (["--transcript"].as_slice(), "transcript", 3usize)
            } else {
                (["--code", "--transcript"].as_slice(), "both", 2usize)
            };
            let arg = line[arg_start..].trim().to_owned();
            if arg.is_empty() {
                let _ = Command::new(&gait_bin)
                    .args(["undo", &session_id, "--list",
                           "--gait-dir", workspace.to_str().unwrap_or(".")])
                    .status();
                continue;
            } else if let Ok(n) = arg.parse::<u32>() {
                let mut cmd_args = vec!["undo", &session_id, "--turn", &arg];
                cmd_args.extend_from_slice(flags);
                cmd_args.extend_from_slice(&["--gait-dir", workspace.to_str().unwrap_or(".")]);
                let ok = Command::new(&gait_bin)
                    .args(&cmd_args)
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false);
                if !ok {
                    eprintln!("[undo] failed — is there a turn {n} for this session?");
                    continue;
                }
                format!("[undo] restored {label} to turn {n}")
            } else {
                eprintln!("{}", hints::UNDO_USAGE);
                continue;
            }
        } else {
            line
        };

        let _ = rl.add_history_entry(&line);
        let line = expand_quote_reply(&line, &last_turn_blocks);

        let mut current_turn_blocks: Vec<String> = Vec::new();

        'spawn: loop {
            let mut cmd = Command::new(&gait_bin);
            cmd.args([
                "run",
                "--session",
                &session_id,
                "--task",
                &line,
                "--gait-dir",
                workspace.to_str().unwrap_or("."),
                "--codebase",
                codebase.to_str().unwrap_or("."),
                // No --auto: allow list governs execution
            ]);
            cmd.args(["--agent", agent_path.to_str().unwrap_or("")]);

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

            let mut turn_complete_blocks: Option<Vec<String>> = None;
            for json_line in reader.lines() {
                match renderer.handle_line(&json_line?) {
                    crate::render::RenderAction::PermissionNeeded(items) => pending_permissions = items,
                    crate::render::RenderAction::ClarifyNeeded(items) => pending_clarify = items,
                    crate::render::RenderAction::InformNeeded(items) => pending_inform = items,
                    crate::render::RenderAction::TurnComplete(blocks) => turn_complete_blocks = Some(blocks),
                    _ => {}
                }
            }
            renderer.flush();
            child.wait()?;
            current_turn_blocks.extend(
                turn_complete_blocks.unwrap_or_else(|| renderer.turn_blocks.drain(..).collect())
            );

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
                    Some(agent_path.as_path()),
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
                continue 'spawn;
            }
        }
        last_turn_blocks = current_turn_blocks;
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
        let result = expand_at_markers("files: $(echo hi)", &hist, dir.path()).unwrap();
        assert!(result.contains("[shell: echo hi]"), "got: {result}");
        assert!(result.contains("hi"), "got: {result}");
    }

    #[test]
    fn expands_history_recall() {
        let dir = TempDir::new().unwrap();
        let hist = make_history(dir.path());
        let result = expand_at_markers("prev: $(1)", &hist, dir.path()).unwrap();
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
    fn at_paren_no_longer_expands() {
        // @(cmd) is old syntax — should pass through unchanged
        let dir = TempDir::new().unwrap();
        let hist = dir.path().join("hist.jsonl");
        let result = expand_at_markers("old: @(echo hi)", &hist, dir.path()).unwrap();
        assert!(result.contains("@(echo hi)"), "got: {result}");
    }

    #[test]
    fn multiple_expansions() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.txt"), "aaa").unwrap();
        let hist = dir.path().join("hist.jsonl");
        let result = expand_at_markers("x=$(echo x) y=@a.txt", &hist, dir.path()).unwrap();
        assert!(result.contains("[shell: echo x]"), "got: {result}");
        assert!(result.contains("[file: a.txt]"), "got: {result}");
    }
}

#[cfg(test)]
mod quote_reply_tests {
    use super::expand_quote_reply;

    fn blocks() -> Vec<String> {
        vec![
            "First paragraph here.".into(),
            "Second paragraph here.".into(),
            "Third paragraph here.".into(),
        ]
    }

    #[test]
    fn no_prefix_passes_through() {
        assert_eq!(expand_quote_reply("hello", &blocks()), "hello");
    }

    #[test]
    fn bare_gt_passes_through() {
        assert_eq!(expand_quote_reply(">", &blocks()), ">");
    }

    #[test]
    fn gt_without_parens_passes_through() {
        assert_eq!(expand_quote_reply(">2", &blocks()), ">2");
    }

    #[test]
    fn gt_paren_non_number_passes_through() {
        assert_eq!(expand_quote_reply(">(foo)", &blocks()), ">(foo)");
    }

    #[test]
    fn unclosed_paren_passes_through() {
        assert_eq!(expand_quote_reply(">(2", &blocks()), ">(2");
    }

    #[test]
    fn single_block_quoted() {
        let result = expand_quote_reply(">(2)", &blocks());
        assert_eq!(result, "> Second paragraph here.");
    }

    #[test]
    fn inline_mid_sentence() {
        let result = expand_quote_reply("regarding >(2) — what did you mean?", &blocks());
        assert!(result.contains("> Second paragraph here."), "got: {result}");
        assert!(result.contains("regarding"), "got: {result}");
        assert!(result.contains("what did you mean?"), "got: {result}");
    }

    #[test]
    fn single_block_with_trailing_text() {
        let result = expand_quote_reply(">(1) follow up", &blocks());
        assert_eq!(result, "> First paragraph here. follow up");
    }

    #[test]
    fn range_quoted() {
        let result = expand_quote_reply(">(1-2)", &blocks());
        assert!(result.contains("> First paragraph here."), "got: {result}");
        assert!(result.contains("> Second paragraph here."), "got: {result}");
        assert!(!result.contains("Third"), "got: {result}");
    }

    #[test]
    fn out_of_range_passes_through() {
        assert_eq!(expand_quote_reply(">(99)", &blocks()), ">(99)");
    }

    #[test]
    fn zero_passes_through() {
        assert_eq!(expand_quote_reply(">(0)", &blocks()), ">(0)");
    }

    #[test]
    fn empty_blocks_passes_through() {
        assert_eq!(expand_quote_reply(">(1)", &[]), ">(1)");
    }
}

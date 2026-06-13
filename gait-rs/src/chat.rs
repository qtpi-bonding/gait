use std::path::PathBuf;
use std::process::Command;
use rustyline::Editor;
use rustyline::history::DefaultHistory;

use crate::repl_helper::{ReplHelper, ShellNavHandler, ShellNavState};
use rustyline::error::ReadlineError;
use rustyline::{EventHandler, KeyCode, KeyEvent, Modifiers};
use std::sync::{Arc, Mutex};

use crate::proto::{events, wire};
use crate::theme::{Theme, RESET, DIM_FG};
use crate::ui::{ReplBuilder, ViewSignal};
use crate::repl_input::{is_bang_command, bang_command_str, expand_input, ExpandCtx, parse_skill_toggle, run_and_capture_pty_raw, CaptureResult};

/// All user-visible hint strings for the REPL, centralised here for easy editing.
mod hints {
    pub const STARTUP: &str =
        "ctrl+d or /q to exit  \u{2022}  /n new session  \u{2022}  /u [N] undo  \u{2022}  /skill enable  \u{2022}  !cmd shell";
    pub const UNDO_USAGE: &str =
        "/u [N]   undo code+transcript  (list turns if no N)\n\
         /uc [N]  undo code only\n\
         /ut [N]  undo transcript only";
}

pub fn new_session_id() -> String {
    format!("s{}", &uuid::Uuid::new_v4().to_string().replace('-', "")[..8])
}

pub async fn run_chat(
    gait_dir: PathBuf,
    codebase: PathBuf,
    agent_name: Option<String>,
    session_id: Option<String>,
) -> anyhow::Result<()> {
    let gait_bin = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("gait"));

    let agent_path: Option<PathBuf> = match agent_name.as_deref() {
        Some(n) => Some(crate::agent_frontmatter::resolve_markdown_context_path(
            &gait_dir,
            crate::agent_frontmatter::MarkdownKind::Agent,
            n,
        )),
        None => {
            let default = crate::agent_frontmatter::resolve_markdown_context_path(
                &gait_dir,
                crate::agent_frontmatter::MarkdownKind::Agent,
                "default",
            );
            if default.exists() { Some(default) } else { None }
        }
    };
    let theme = Theme::detect();

    let mut session_id = session_id.unwrap_or_else(new_session_id);
    let mut shell_history_path = gait_dir.join("tmp").join(format!("shell-{}.jsonl", session_id));
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
    seed_history_from_transcripts(&mut rl, &gait_dir);
    rl.set_helper(Some(ReplHelper { nav: nav.clone() }));
    rl.bind_sequence(
        KeyEvent(KeyCode::Up, Modifiers::SHIFT),
        EventHandler::Conditional(Box::new(ShellNavHandler { nav: nav.clone(), up: true })),
    );
    rl.bind_sequence(
        KeyEvent(KeyCode::Down, Modifiers::SHIFT),
        EventHandler::Conditional(Box::new(ShellNavHandler { nav: nav.clone(), up: false })),
    );

    let mut builder = ReplBuilder::new(rl, theme.clone());
    let mut last_turn_blocks: Vec<String> = Vec::new();

    loop {
        nav.lock().unwrap().index = 0;
        let prompt = format!("{}>{} {}", theme.accent.fg(), RESET, theme.user_msg.fg());
        let line = match builder.readline(&prompt) {
            Ok(l) => { eprint!("{}", RESET); l.trim().to_string() }
            Err(ReadlineError::Eof) | Err(ReadlineError::Interrupted) => break,
            Err(e) => return Err(e.into()),
        };
        if line.is_empty() {
            continue;
        }
        if is_bang_command(&line) {
            let cmd = bang_command_str(&line);
            let (output, code) = match run_and_capture_pty_raw(cmd, &codebase) {
                CaptureResult::Output(out, code) => (out, code),
                CaptureResult::Interactive => (String::new(), Some(0)),
            };
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

        // /skillname [off] — emit Toggle{kind:SKILL} event; do NOT forward to model.
        // Checked on the original line (before the undo block mutates it) so that
        // a user typing "/tdd" toggles the skill rather than being swallowed by undo.
        if let Some((skill_name, enabled)) = parse_skill_toggle(&line) {
            let available = crate::skills::scan_skills(&gait_dir);
            if !available.iter().any(|s| s.name == skill_name) {
                eprintln!(
                    "[gait] no such skill {:?} — is skills/{}/SKILL.md present?",
                    skill_name, skill_name,
                );
            } else {
                let tx_path = gait_dir
                    .join("sessions")
                    .join(&session_id)
                    .join("log.jsonl");
                let _ = wire::append(
                    &tx_path,
                    events::toggle("skill", &skill_name, enabled),
                );
                if enabled {
                    eprintln!("skill {} enabled", skill_name);
                } else {
                    eprintln!("skill {} disabled", skill_name);
                }
            }
            continue;
        }


        match line.as_str() {
            "/n" | "/new" => {
                session_id = new_session_id();
                shell_history_path = gait_dir.join("tmp").join(format!("shell-{}.jsonl", session_id));
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
                           "--gait-dir", gait_dir.to_str().unwrap_or(".")])
                    .status();
                continue;
            } else if let Ok(n) = arg.parse::<u32>() {
                let mut cmd_args = vec!["undo", &session_id, "--turn", &arg];
                cmd_args.extend_from_slice(flags);
                cmd_args.extend_from_slice(&["--gait-dir", gait_dir.to_str().unwrap_or(".")]);
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

        builder.add_history_entry(&line);
        // Unified expansion: $(cmd)/$(N) shell, @path files, >(N) quote-reply —
        // identical to the CLI path. No cascade (see expand_input).
        let line = expand_input(&line, &ExpandCtx {
            codebase: &codebase,
            history: &shell_history_path,
            last_blocks: &last_turn_blocks,
        });

        // Build session/config/api_key ONCE per task line (in-process, no subprocess).
        let repos = crate::git::Repos::production();
        let session = Arc::new(crate::session::Session::load(
            gait_dir.clone(),
            session_id.clone(),
            codebase.clone(),
            repos.clone(),
        ));
        let config = crate::config::load_config(&gait_dir);

        'spawn: loop {
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<
                crate::proto::gait::log::v1::StreamItem,
            >();
            let emitter = crate::proto::Emitter::for_session(&gait_dir, &session_id, tx);

            // Rebuild Profile per turn (Profile is not Clone) and resolve with OWNED args.
            let profile = crate::profile::Profile::from_config_and_agent(
                &config,
                &gait_dir,
                Some(codebase.as_path()),
                agent_path.as_deref(),
                None,
            );
            // Derive provider + api_key from the RESOLVED model (matches gait.rs run
            // handler): precedence is override > agent frontmatter > config, so the
            // provider must come from profile.model, not config.model. Compute the
            // owned api_key in one statement so the &profile.model borrow ends before
            // profile.resolve(...) moves profile.
            let api_key = crate::auth::resolve_api_key(
                profile.model.split_once('/').map(|(p, _)| p).unwrap_or(&profile.model),
            );
            let caps = profile
                .resolve(
                    codebase.clone(),
                    session_id.clone(),
                    gait_dir.clone(),
                    repos.clone(),
                    api_key,
                )
                .await?;
            let input = crate::turn::TurnInput {
                task: Some(line.clone()),
                auto: false,
                max_iter: config.max_iter,
                preview_policy: config.preview,
            };

            // Spawn the turn; move owned session(Arc)+emitter+caps+input so emitter
            // drops on completion → channel closes → rx.recv() yields None.
            let session_for_turn = session.clone();
            let turn_handle = tokio::spawn(async move {
                crate::turn::run_turn(&*session_for_turn, &emitter, caps, input).await
            });

            builder.begin_turn();
            let mut turn_done_blocks: Option<Vec<String>> = None;

            while let Some(item) = rx.recv().await {
                match builder.handle_stream_item(item) {
                    ViewSignal::TurnDone(blocks) => { turn_done_blocks = Some(blocks); }
                    ViewSignal::Continue => {}
                }
            }
            builder.flush();

            // Graceful turn-error handling: a turn that errors or panics must NOT
            // kill the REPL (the old subprocess just printed and returned to the
            // prompt). break 'spawn falls through to the outer loop.
            match turn_handle.await {
                Ok(Ok(_)) => {}
                Ok(Err(e)) => {
                    eprintln!("[gait] turn error: {e}");
                    break 'spawn;
                }
                Err(e) => {
                    eprintln!("[gait] turn panicked: {e}");
                    break 'spawn;
                }
            }

            last_turn_blocks = turn_done_blocks.unwrap_or_else(|| builder.take_turn_blocks());

            if builder.has_interaction() {
                let tx_path = gait_dir.join("sessions").join(&session_id).join("log.jsonl");
                let outcome = builder.run_interaction(&gait_dir, &session_id, agent_path.as_deref())?;
                wire::append(&tx_path, outcome.event_body)?;
                if outcome.respawn { continue 'spawn; }
            }

            println!();
            break 'spawn;
        }
    }

    Ok(())
}

fn seed_history_from_transcripts(
    rl: &mut Editor<ReplHelper, DefaultHistory>,
    gait_dir: &std::path::Path,
) {
    let sessions_dir = gait_dir.join("sessions");
    let Ok(entries) = std::fs::read_dir(&sessions_dir) else { return };

    let mut sessions: Vec<(std::time::SystemTime, std::path::PathBuf)> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let tx = e.path().join("log.jsonl");
            let mtime = std::fs::metadata(&tx).ok()?.modified().ok()?;
            Some((mtime, tx))
        })
        .collect();

    sessions.sort_by_key(|(mtime, _)| *mtime);

    for (_, tx_path) in &sessions {
        use crate::proto::gait::log::v1::{event, payload};
        let Ok(events) = crate::session::transcript::read_events(tx_path) else { continue };
        for ev in events {
            if let Some(event::Body::Payload(p)) = &ev.body {
                if let Some(payload::Kind::UserTurn(u)) = &p.kind {
                    let _ = rl.add_history_entry(&u.content);
                }
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
}

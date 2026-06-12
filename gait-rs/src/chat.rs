use std::path::PathBuf;
use std::process::Command;
use rustyline::Editor;
use rustyline::history::DefaultHistory;

use crate::repl_helper::{ReplHelper, ShellNavHandler, ShellNavState};
use rustyline::error::ReadlineError;
use rustyline::{EventHandler, KeyCode, KeyEvent, Modifiers};
use std::sync::{Arc, Mutex};

use crate::session::transcript::{self, PermissionItem, PermissionsReplyItem, ClarifyItem, InformItem};
use crate::proto::{events, wire};
use crate::theme::{Theme, RESET, DIM_FG};
use crate::ui::{Timer, Renderer, RenderAction, prompt_permission, prompt_clarify, prompt_inform};
use crate::repl_input::{is_bang_command, bang_command_str, expand_input, ExpandCtx, run_and_capture_pty_raw, CaptureResult};

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

pub async fn run_chat(
    gait_dir: PathBuf,
    codebase: PathBuf,
    agent_name: Option<String>,
    session_id: Option<String>,
) -> anyhow::Result<()> {
    let gait_bin = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("gait"));

    let agent_path: PathBuf = match agent_name.as_deref() {
        Some(n) => crate::agent_frontmatter::resolve_agent_path(&gait_dir, n),
        None => gait_dir.join("agent.md"),
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

        let _ = rl.add_history_entry(&line);
        // Unified expansion: $(cmd)/$(N) shell, @path files, >(N) quote-reply —
        // identical to the CLI path. No cascade (see expand_input).
        let line = expand_input(&line, &ExpandCtx {
            codebase: &codebase,
            history: &shell_history_path,
            last_blocks: &last_turn_blocks,
        });

        let mut current_turn_blocks: Vec<String> = Vec::new();

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
                Some(agent_path.as_path()),
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
            };

            // Spawn the turn; move owned session(Arc)+emitter+caps+input so emitter
            // drops on completion → channel closes → rx.recv() yields None.
            let session_for_turn = session.clone();
            let turn_handle = tokio::spawn(async move {
                crate::turn::run_turn(&*session_for_turn, &emitter, caps, input).await
            });

            // Drain events IN ORDER, rendering as they arrive.
            use crate::proto::gait::log::v1::stream_item::Item;
            let mut renderer = Renderer::new(theme.clone());
            let mut pending_permissions: Vec<PermissionItem> = vec![];
            let mut pending_clarify: Vec<ClarifyItem> = vec![];
            let mut pending_inform: Vec<InformItem> = vec![];
            let mut turn_complete_blocks: Option<Vec<String>> = None;
            let mut timer = Timer::start();

            while let Some(stream_item) = rx.recv().await {
                timer.stop();
                let action = match stream_item.item {
                    Some(Item::Event(ev)) => renderer.handle_event(&ev),
                    Some(Item::Delta(d)) => {
                        renderer.push_text(&d.content);
                        RenderAction::Continue
                    }
                    None => RenderAction::Continue,
                };
                match action {
                    RenderAction::PermissionNeeded(items) => pending_permissions = items,
                    RenderAction::ClarifyNeeded(items) => pending_clarify = items,
                    RenderAction::InformNeeded(items) => pending_inform = items,
                    RenderAction::TurnComplete(blocks) => turn_complete_blocks = Some(blocks),
                    _ => {}
                }
            }
            renderer.flush();
            timer.stop();
            // Graceful turn-error handling: a turn that errors or panics must NOT
            // kill the REPL (the old subprocess just printed and returned to the
            // prompt). break 'spawn falls through to `last_turn_blocks =
            // current_turn_blocks;` below, keeping quote-reply state consistent.
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
            current_turn_blocks.extend(
                turn_complete_blocks.unwrap_or_else(|| renderer.turn_blocks.drain(..).collect())
            );

            // Handle clarify: prompt user for each question, write ClarifyReply, re-spawn
            if !pending_clarify.is_empty() {
                let tx_path = gait_dir.join("sessions").join(&session_id).join("log.jsonl");
                let reply_items = prompt_clarify(&pending_clarify, &theme);
                let _ = wire::append(&tx_path, events::clarify_reply(&reply_items));
                continue 'spawn;
            }

            // Handle inform: prompt user to confirm understanding, write InformReply, re-spawn
            if !pending_inform.is_empty() {
                let tx_path = gait_dir.join("sessions").join(&session_id).join("log.jsonl");
                let reply_items = prompt_inform(&pending_inform, &theme);
                let _ = wire::append(&tx_path, events::inform_reply(&reply_items));
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

                let choice = prompt_permission(item, label.as_deref(), &theme);
                if let Err(e) = crate::permission::save_choice(
                    &choice,
                    &gait_dir,
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
                    crate::permission::PermissionChoice::Session(pat) => {
                        reply_items.push(PermissionsReplyItem {
                            id: item.id.clone(),
                            decision: "approve".into(),
                            text: Some(pat.clone()),
                            scope: Some("session".into()),
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

            let tx_path = gait_dir
                .join("sessions")
                .join(&session_id)
                .join("log.jsonl");
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

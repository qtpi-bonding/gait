use rustyline::history::DefaultHistory;
use rustyline::Editor;
use std::path::PathBuf;

use crate::repl::{ReplHelper, ShellNavHandler, ShellNavState};
use gait_core::embed::{TurnSource, TurnStart};
use rustyline::{EventHandler, KeyCode, KeyEvent, Modifiers};
use std::sync::{Arc, Mutex};

use crate::theme::{Theme, DIM_FG, RESET};
use crate::ui::ReplBuilder;

/// All user-visible hint strings for the REPL, centralised here for easy editing.
pub(crate) mod hints {
    pub const WORDMARK: &str = include_str!("assets/wordmark.txt");
    pub const STARTUP: &str =
        "ctrl+d or /q to exit  \u{2022}  /n new session  \u{2022}  /u [N] undo  \u{2022}  /skill enable  \u{2022}  !cmd shell";
    pub const UNDO_USAGE: &str = "/u [N]   undo code+transcript  (list turns if no N)\n\
         /uc [N]  undo code only\n\
         /ut [N]  undo transcript only";
}

/// The REPL: a banner + rustyline editor, then an outer session loop. Each
/// `run_loop` runs one session's turns through the shared input boundary
/// (`ReplSource`/`ReplSink`); `/n` ends it and starts a new session (reusing the
/// same editor/history), `/q`/EOF exits. (Phase 4b — the REPL on `run_loop`.)
pub async fn run_chat(
    gait_dir: PathBuf,
    codebase: PathBuf,
    agent_name: Option<String>,
    session_id: Option<String>,
) -> anyhow::Result<()> {
    let gait_bin = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("gait"));

    let agent_path: Option<PathBuf> = match agent_name.as_deref() {
        Some(n) => Some(gait_core::agent_frontmatter::resolve_markdown_context_path(
            &gait_dir,
            gait_core::agent_frontmatter::MarkdownKind::Agent,
            n,
        )),
        None => {
            let default = gait_core::agent_frontmatter::resolve_markdown_context_path(
                &gait_dir,
                gait_core::agent_frontmatter::MarkdownKind::Agent,
                "default",
            );
            if default.exists() {
                Some(default)
            } else {
                None
            }
        }
    };
    let theme = Theme::detect();
    // If a previous session crashed with raw mode on, crossterm would save raw

    // mode as the "original" on first enable_raw_mode(), then restore to it on

    // disable_raw_mode(). Reset unconditionally — stty sane is cheap and the

    // banner's \r\n output (Task 1) has already made visual output safe, but

    // rustyline and interactive input still behave better in cooked mode.

    if std::io::IsTerminal::is_terminal(&std::io::stderr())
        && crossterm::terminal::is_raw_mode_enabled().unwrap_or(false)
    {
        let _ = std::process::Command::new("stty")
            .arg("sane")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }

    let mut session_id = session_id.unwrap_or_else(gait_core::session::new_session_id);
    let shell_history_path = gait_dir
        .join("tmp")
        .join(format!("shell-{}.jsonl", session_id));
    if std::io::IsTerminal::is_terminal(&std::io::stderr()) {
        let wordmark_crlf = hints::WORDMARK.replace('\n', "\r\n");
        eprint!("{}{}{}", theme.accent.fg(), wordmark_crlf, RESET);
        let rev = gait_core::GAIT_BUILD_GIT_HASH
            .get(..7)
            .unwrap_or(gait_core::GAIT_BUILD_GIT_HASH);
        eprint!("{}  {}{}\r\n", DIM_FG, rev, RESET);
    }
    eprint!(
        "{}gait{}  {}session {}{}  {}{}{}\r\n",
        theme.accent.fg(),
        RESET,
        DIM_FG,
        session_id,
        RESET,
        DIM_FG,
        hints::STARTUP,
        RESET,
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
        EventHandler::Conditional(Box::new(ShellNavHandler {
            nav: nav.clone(),
            up: true,
        })),
    );
    rl.bind_sequence(
        KeyEvent(KeyCode::Down, Modifiers::SHIFT),
        EventHandler::Conditional(Box::new(ShellNavHandler {
            nav: nav.clone(),
            up: false,
        })),
    );

    // One builder, shared by the per-session source + sink. It outlives every
    // `run_loop` so editor history and view state persist across `/n`.
    let builder = ReplBuilder::new(rl, theme.clone());
    let shared: crate::repl::Shared =
        std::rc::Rc::new(std::cell::RefCell::new(crate::repl::ReplShared {
            builder,
            turn_done_blocks: None,
            control: None,
            watcher_done: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            watcher_handle: None,
            sandbox_on: gait_core::config::load_config(&gait_dir).sandbox.on,
        }));
    let repos = gait_core::git::Repos::production();

    // Shared slot: holds the current cascade group so the signal task can reap it.
    let signal_slot: gait_core::turn::spine::SignalSlot =
        std::sync::Arc::new(std::sync::Mutex::new(None));

    // Spawn SIGINT/SIGTERM handler: kill current group (if any) then exit.
    {
        use tokio::signal::unix::{signal, SignalKind};
        let slot = signal_slot.clone();
        tokio::spawn(async move {
            let mut sigint = signal(SignalKind::interrupt()).expect("SIGINT handler");
            let mut sigterm = signal(SignalKind::terminate()).expect("SIGTERM handler");
            tokio::select! {
                _ = sigint.recv() => {}
                _ = sigterm.recv() => {}
            }
            if let Some(group) = slot.lock().unwrap().clone() {
                let _ = group.kill_all();
            }
            std::process::exit(0);
        });
    }

    loop {
        let shell_history_path = gait_dir
            .join("tmp")
            .join(format!("shell-{}.jsonl", session_id));
        nav.lock().unwrap().path = shell_history_path.clone();
        let config = gait_core::config::load_config(&gait_dir);

        // Resolve API key for the brain (spine passes it via env, never disk).
        let provider_name = {
            let profile = gait_core::profile::Profile::from_config_and_agent(
                &config,
                &gait_dir,
                Some(codebase.as_path()),
                agent_path.as_deref(),
                None,
            );
            match profile.model.split_once('/') {
                Some((p, _)) => p.to_string(),
                None => profile.model.clone(),
            }
        };
        let api_key = gait_core::auth::resolve_api_key(&provider_name).unwrap_or_default();

        // Ensure the session worktree (and log dir) exist before appending input.
        repos
            .traces
            .create_session_worktree(&gait_dir, &session_id)
            .map_err(|e| anyhow::anyhow!("create session worktree: {e}"))?;

        let tx_path = gait_dir
            .join("sessions")
            .join(&session_id)
            .join("log.jsonl");

        // Build brain argv: --session, --gait-dir, --codebase, sandbox flag,
        // optional --max-iter / --agent. No --task (brain runs Resume mode).
        let mut brain_args = vec![
            "--session".to_string(),
            session_id.clone(),
            "--gait-dir".to_string(),
            gait_dir.to_str().unwrap_or(".").to_string(),
            "--codebase".to_string(),
            codebase.to_str().unwrap_or(".").to_string(),
        ];
        // Sandbox flag: read from shared (user may toggle per-session).
        if shared.borrow().sandbox_on {
            brain_args.push("--sandbox".to_string());
        } else {
            brain_args.push("--no-sandbox".to_string());
        }
        if let Some(n) = config.max_iter {
            brain_args.push("--max-iter".to_string());
            brain_args.push(n.to_string());
        }
        if let Some(ref name) = agent_name {
            brain_args.push("--agent".to_string());
            brain_args.push(name.clone());
        }

        let mut source = crate::repl::ReplSource {
            shared: shared.clone(),
            gait_dir: gait_dir.clone(),
            codebase: codebase.clone(),
            session_id: session_id.clone(),
            agent_path: agent_path.clone(),
            repos: repos.clone(),
            theme: theme.clone(),
            nav: nav.clone(),
            shell_history_path,
            gait_bin: gait_bin.clone(),
            last_blocks: Vec::new(),
            ran_turn: false,
        };
        let mut sink = crate::repl::ReplSink {
            shared: shared.clone(),
        };

        // Per-turn spine loop: prompt → write input → spawn brain → relay.
        loop {
            let Some(start) = source.next().await else {
                break;
            };

            // Write input events to log (Resume skips this).
            if let TurnStart::Input(bodies) = start {
                for body in bodies {
                    gait_core::proto::wire::append(&tx_path, body)
                        .map_err(|e| anyhow::anyhow!("append input: {e}"))?;
                }
            }

            // Re-read sandbox_on in case the user toggled it this turn.
            let sandbox_on = shared.borrow().sandbox_on;
            let mut turn_args = brain_args.clone();
            // Replace --sandbox / --no-sandbox with the current per-turn value.
            if let Some(pos) = turn_args
                .iter()
                .position(|a| a == "--sandbox" || a == "--no-sandbox")
            {
                turn_args[pos] = if sandbox_on {
                    "--sandbox".to_string()
                } else {
                    "--no-sandbox".to_string()
                };
            }

            let profile_inputs = gait_core::turn::spine::ProfileInputs {
                agent_name: agent_name.clone(),
                model_override: None,
                allow: vec![],
                deny: vec![],
            };
            let _failed = gait_core::turn::spine::relay_brain_turn(
                &turn_args,
                &gait_dir,
                &codebase,
                &session_id,
                &api_key,
                &gait_bin,
                &config.sandbox,
                sandbox_on,
                &profile_inputs,
                &mut sink,
                None, // top-level REPL turn: no inherited cascade group
                Some(&signal_slot),
            )
            .await
            .unwrap_or_else(|e| {
                eprintln!("[gait] turn error: {e}\r\n");
                false
            });

            // Clear the slot — turn complete, no live group to kill between turns.
            *signal_slot.lock().unwrap() = None;
        }

        // The source ended the loop with a control signal (or EOF).
        let control = shared.borrow_mut().control.take();
        match control {
            Some(crate::repl::Control::NewSession(id)) => {
                session_id = id;
                eprint!("new session {}\r\n", session_id);
                continue;
            }
            Some(crate::repl::Control::Quit) | None => break,
        }
    }

    let mut resume = format!("gait chat --session {}", session_id);
    let cwd = std::env::current_dir().unwrap_or_default();
    if gait_dir != cwd {
        resume.push_str(&format!(" --gait-dir {}", gait_dir.display()));
    }
    eprint!("{}{}{}\r\n", DIM_FG, resume, RESET);

    Ok(())
}

fn seed_history_from_transcripts(
    rl: &mut Editor<ReplHelper, DefaultHistory>,
    gait_dir: &std::path::Path,
) {
    let sessions_dir = gait_dir.join("sessions");
    let Ok(entries) = std::fs::read_dir(&sessions_dir) else {
        return;
    };

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
        use gait_core::proto::gait::log::v1::{event, payload};
        let Ok(events) = gait_core::session::log::read_events(tx_path) else {
            continue;
        };
        for ev in events {
            if let Some(event::Body::Payload(p)) = &ev.body {
                if let Some(payload::Kind::UserText(u)) = &p.kind {
                    let _ = rl.add_history_entry(&u.content);
                }
            }
        }
    }
}

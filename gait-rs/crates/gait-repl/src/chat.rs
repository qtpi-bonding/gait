use std::path::PathBuf;

use crate::theme::{Theme, DIM_FG, RESET};
use crate::ui::ReplBuilder;

/// All user-visible hint strings for the REPL, centralised here for easy editing.
pub(crate) mod hints {
    pub const WORDMARK: &str = include_str!("assets/wordmark.txt");
    pub const STARTUP: &str =
        "ctrl+d or /q to exit  \u{2022}  shift-tab mode cycle  \u{2022}  /n new session  \u{2022}  /u [N] undo  \u{2022}  /skill enable  \u{2022}  !cmd shell";
    pub const UNDO_USAGE: &str = "/u [N]   undo code+transcript  (list turns if no N)\n\
         /uc [N]  undo code only\n\
         /ut [N]  undo transcript only";
}

fn build_line_editor(
    shell_history_path: std::path::PathBuf,
    history_file: std::path::PathBuf,
    gait_dir: std::path::PathBuf,
    codebase: std::path::PathBuf,
    theme: &crate::theme::Theme,
) -> reedline::Reedline {
    use reedline::{
        default_emacs_keybindings, ColumnarMenu, EditCommand, Emacs, FileBackedHistory, KeyCode,
        KeyModifiers, MenuBuilder, Reedline, ReedlineEvent, ReedlineMenu,
    };
    let history = Box::new(
        FileBackedHistory::with_file(2000, history_file).expect("create reedline history"),
    );
    let mut kb = default_emacs_keybindings();
    // Alt-Enter inserts a newline; plain Enter submits (reedline default).
    kb.add_binding(
        KeyModifiers::ALT,
        KeyCode::Enter,
        ReedlineEvent::Edit(vec![EditCommand::InsertNewline]),
    );
    kb.add_binding(
        KeyModifiers::SHIFT,
        KeyCode::BackTab,
        ReedlineEvent::ExecuteHostCommand("gait:cycle-mode".into()),
    );
    kb.add_binding(
        KeyModifiers::NONE,
        KeyCode::Tab,
        ReedlineEvent::UntilFound(vec![
            ReedlineEvent::Menu("completion_menu".to_string()),
            ReedlineEvent::MenuNext,
        ]),
    );
    Reedline::create()
        .with_history(history)
        .with_hinter(Box::new(crate::repl::GaitHinter {
            shell_history_path: shell_history_path.clone(),
            current: String::new(),
        }))
        .with_completer(Box::new(crate::repl::GaitCompleter {
            gait_dir: gait_dir.clone(),
            codebase: codebase.clone(),
            shell_history_path: shell_history_path.clone(),
        }))
        .with_highlighter(Box::new(crate::repl::GaitHighlighter {
            shell_history_path: shell_history_path.clone(),
            codebase: codebase.clone(),
            user_msg: {
                let c = &theme.user_msg;
                if crate::theme::supports_truecolor() {
                    nu_ansi_term::Color::Rgb(c.0, c.1, c.2)
                } else {
                    nu_ansi_term::Color::Fixed(crate::theme::rgb_to_ansi256(c.0, c.1, c.2))
                }
            },
        }))
        .with_menu(ReedlineMenu::EngineCompleter(Box::new(
            ColumnarMenu::default().with_name("completion_menu"),
        )))
        .with_edit_mode(Box::new(Emacs::new(kb)))
}

/// The REPL: a banner + reedline editor, then an outer session loop. Each
/// `run_loop` runs one session's turns through the shared input boundary
/// (`ReplSource`); `/n` ends it and starts a new session (reusing the
/// same editor/history), `/q`/EOF exits. (Phase 4b — the REPL on `run_loop`.)
pub async fn run_chat(
    gait_dir: PathBuf,
    codebase: PathBuf,
    agent_name: Option<String>,
    session_id: Option<String>,
    model_override: Option<String>,
) -> anyhow::Result<()> {
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
    // banner's \r\n output has already made visual output safe.
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
        // No `ReplBuilder` constructed yet at startup — nothing to synchronize against.
        #[allow(clippy::print_stderr)]
        {
            eprint!("{}{}{}", theme.accent.fg(), wordmark_crlf, RESET);
            let rev = gait_core::GAIT_BUILD_GIT_HASH
                .get(..7)
                .unwrap_or(gait_core::GAIT_BUILD_GIT_HASH);
            eprint!("{}  {}{}\r\n", DIM_FG, rev, RESET);
        }
    }
    #[allow(clippy::print_stderr)] // still pre-`shared`
    {
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
    }

    let history_file = gait_dir.join("repl_history");
    let rl = build_line_editor(
        shell_history_path.clone(),
        history_file,
        gait_dir.clone(),
        codebase.clone(),
        &theme,
    );

    // One builder, shared by the per-session source + sink. It outlives every
    // `run_loop` so editor history and view state persist across `/n`.
    let builder = ReplBuilder::new(Box::new(crate::repl::ReedlineReader(rl)), theme.clone());
    let shared: crate::repl::Shared =
        std::rc::Rc::new(std::cell::RefCell::new(crate::repl::ReplShared {
            builder,
            turn_done_blocks: None,
            control: None,
            // Entry point: surface gait.jsonc parse errors instead of silently
            // defaulting. The REPL's whole UX hinges on `sandbox.on` being
            // the value the user wrote, not the compiled-in default.
            sandbox_on: match gait_core::config::load_config_with(
                &gait_dir,
                gait_core::config::ConfigLoadPolicy::Strict,
            ) {
                Ok(c) => c.sandbox.on,
                Err(e) => {
                    // No `ReplBuilder` constructed yet — this closure runs while
                    // building the `ReplShared` it would otherwise write through.
                    #[allow(clippy::print_stderr)]
                    {
                        eprintln!("[gait] {e}");
                    }
                    std::process::exit(1);
                }
            },
        }));
    let repos = gait_core::git::Repos::production();

    // Process-lifetime cascade kill handle — held by the SIGINT/SIGTERM handler
    // and threaded through run_loop so relay_brain_turn can register the active
    // group. On signal, kill() tears down the sealed tree before exiting.
    let kill_handle = gait_core::embed::CascadeKillHandle::new();
    {
        use tokio::signal::unix::{signal, SignalKind};
        let handle = kill_handle.clone();
        tokio::spawn(async move {
            let mut sigint = signal(SignalKind::interrupt()).expect("SIGINT handler");
            let mut sigterm = signal(SignalKind::terminate()).expect("SIGTERM handler");
            tokio::select! {
                _ = sigint.recv() => {}
                _ = sigterm.recv() => {}
            }
            handle.kill();
            std::process::exit(0);
        });
    }

    loop {
        let shell_history_path = gait_dir
            .join("tmp")
            .join(format!("shell-{}.jsonl", session_id));

        let source = crate::repl::ReplSource {
            shared: shared.clone(),
            gait_dir: gait_dir.clone(),
            codebase: codebase.clone(),
            session_id: session_id.clone(),
            agent_path: agent_path.clone(),
            repos: repos.clone(),
            shell_history_path,
            model_override: model_override.clone(),
            last_blocks: Vec::new(),
            ran_turn: false,
        };

        // Drive the interactive REPL session through the one blessed
        // degenerate-workflow API (spec-06 §1). `prompt: None` builds an
        // interactive/open node internally; `continue_session` pins it to the
        // current session's log.
        let run_id = format!("chat-{}", session_id);
        let mut sink = crate::repl::ReplSink {
            shared: shared.clone(),
        };
        let mut observer = gait_workflow::observer::NullObserver;
        let _outcome = gait_workflow::single_prompt_run(
            gait_workflow::SinglePromptOpts {
                run_id,
                continue_session: session_id.clone(),
                prompt: None,
                gait_dir: gait_dir.clone(),
                codebase: codebase.clone(),
                args: vec![],
                profile: model_override
                    .clone()
                    .map(|model| gait_workflow::ProfileOverride {
                        inference: Some(model),
                        explicit_inference_override: true,
                        ..Default::default()
                    }),
                ..Default::default()
            },
            &mut sink,
            &mut observer,
            Some(Box::new(source)),
        )
        .await?;
        // NodeOpen means the ReplSource yielded None (the user typed /q, /n,
        // or EOF).  Check the control signal to decide whether to loop.

        // The source ended the run with a control signal (or EOF).
        let control = shared.borrow_mut().control.take();
        match control {
            Some(crate::repl::Control::NewSession(id)) => {
                session_id = id;
                shared
                    .borrow_mut()
                    .builder
                    .write_raw(&format!("new session {}\r\n", session_id));
                continue;
            }
            Some(crate::repl::Control::Quit) | None => break,
        }
    }

    let cwd = std::env::current_dir().unwrap_or_default();
    let gait_dir_flag = if gait_dir != cwd {
        format!(" --gait-dir {}", gait_dir.display())
    } else {
        String::new()
    };
    let resume = format!("gait chat --session {}{}", session_id, gait_dir_flag);
    let persist = format!("gait persist {}{}", session_id, gait_dir_flag);
    let discard = format!("gait discard {}{}", session_id, gait_dir_flag);
    #[allow(clippy::print_stderr)]
    // post-loop, `shared`'s builder is going out of scope; nothing left to synchronize against
    {
        eprint!("{}{}{}\r\n", DIM_FG, resume, RESET);
        eprint!("{}{}{}\r\n", DIM_FG, persist, RESET);
        eprint!("{}{}{}\r\n", DIM_FG, discard, RESET);
    }

    Ok(())
}

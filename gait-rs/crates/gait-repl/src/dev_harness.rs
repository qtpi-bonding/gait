//! Drives the real REPL turn-engine path (`gait_turn_runner::run_turn_source`,
//! the same call `chat.rs` makes) through a scripted, multi-turn conversation
//! — input lines via `ScriptedReader`, canned brain responses via `mock_brain`
//! — and returns the annotated transcript (see `ui::annotate`) of what would
//! have rendered. `run_script` drives an existing gait-dir; `run_script_fresh`
//! additionally spins one up in a temp dir.
//!
//! The `Script`/`Turn`/`Frame` scripting types and the mock-brain frame
//! wire-format helpers live in `crate::turn_script` (no `ReplBuilder`/`Theme`
//! dependency there, so they're shared with `plain::run_plain_script_fresh`
//! too); this module adds only the themed-rendering drive on top, so it stays
//! gated on `rich-tui` in addition to `dev-harness`.
use std::path::Path;

pub use crate::turn_script::{Frame, Script, Turn};

/// Drives `script` through the real interactive REPL path against an
/// already-set-up `gait_dir` (git-inited, `GAIT_BRAIN_EXE` already pointed at
/// `mock_brain`) and returns the annotated transcript of everything that
/// would have rendered.
///
/// NOTE: this future is `!Send` — `ReplSource` uses `#[async_trait(?Send)]`
/// (it holds an `Rc<RefCell<ReplShared>>`, the same single-threaded sharing
/// `chat.rs`'s real REPL loop uses). Callers must `.await` it directly on
/// whatever task already owns it — a plain `#[tokio::test]` (single-threaded
/// by default) or `chat.rs`'s own call site both do this correctly. Do NOT
/// `tokio::spawn` it or call it from a multi-threaded-runtime context that
/// requires `Send` futures — that won't compile, and isn't a bug to "fix" by
/// smuggling in `Send` bounds; it's inherent to the REPL's shared-state model.
pub async fn run_script(
    gait_dir: &Path,
    codebase: &Path,
    session_id: &str,
    script: &Script,
) -> anyhow::Result<String> {
    crate::turn_script::write_numbered_frames(gait_dir, &script.turns)?;

    let theme = crate::theme::Theme::dark();
    let inputs: Vec<&str> = script.turns.iter().map(|t| t.input.as_str()).collect();
    let builder = crate::ui::ReplBuilder::new(
        Box::new(crate::repl::ScriptedReader::lines(&inputs)),
        theme.clone(),
    );
    let shared: crate::repl::Shared =
        std::rc::Rc::new(std::cell::RefCell::new(crate::repl::ReplShared {
            builder,
            status_text: String::new(),
            turn_done_blocks: None,
            control: None,
            sandbox_on: false,
        }));

    let source = crate::repl::ReplSource {
        shared: shared.clone(),
        gait_dir: gait_dir.to_path_buf(),
        codebase: codebase.to_path_buf(),
        session_id: session_id.to_string(),
        agent_path: None,
        repos: gait_core::git::Repos::production(),
        shell_history_path: gait_dir
            .join("tmp")
            .join(format!("shell-{session_id}.jsonl")),
        model_override: None,
        last_blocks: Vec::new(),
        ran_turn: false,
        undo_note: None,
    };
    let mut sink = crate::repl::ReplSink {
        shared: shared.clone(),
    };

    let capture = crate::ui::capture::Capture::begin();
    let outcome = gait_turn_runner::run_turn_source(
        gait_turn_runner::TurnRunOpts {
            run_id: format!("repl-drive-{session_id}"),
            session_id: session_id.to_string(),
            continue_session: session_id.to_string(),
            gait_dir: gait_dir.to_path_buf(),
            codebase: codebase.to_path_buf(),
            args: Vec::new(),
            sandbox_on: Some(false),
            cascade_group_arg: None,
            cascade: None,
            profile: None,
            fs_delegate: None,
            allow_intree: false,
        },
        Box::new(source),
        &mut sink,
    )
    .await;
    let bytes = capture.end();
    if let Err(e) = outcome {
        anyhow::bail!(
            "run_turn_source failed: {e}; capture={}",
            String::from_utf8_lossy(&bytes)
        );
    }
    if String::from_utf8_lossy(&bytes).contains("turn failed") {
        let events = gait_core::session::Ingestor::from_session(gait_dir, session_id)
            .events()
            .unwrap_or_default();
        let coords: Vec<_> = events
            .iter()
            .filter(|e| e.step.is_some())
            .map(|e| (e.run, e.turn, e.step))
            .collect();
        anyhow::bail!(
            "run_turn_source reported a turn failure; coords={coords:?}; capture={}",
            String::from_utf8_lossy(&bytes),
        );
    }

    Ok(crate::ui::annotate::annotate(&bytes, &theme))
}

/// Convenience wrapper: spins up a throwaway git-inited temp gait-dir, points
/// `GAIT_BRAIN_EXE` at `mock_brain_exe`, and calls `run_script`. Returns the
/// `TempDir` too — it deletes on drop, so the caller must hold it until done.
pub async fn run_script_fresh(
    script: &Script,
    mock_brain_exe: &Path,
) -> anyhow::Result<(tempfile::TempDir, String)> {
    let ws = crate::turn_script::setup_fresh_gait_dir(mock_brain_exe)?;
    let gait_dir = ws.path().to_path_buf();
    let session_id = gait_core::session::new_session_id();
    let transcript = run_script(&gait_dir, &gait_dir, &session_id, script).await?;
    Ok((ws, transcript))
}

//! End-to-end proof that the plain REPL's real entry point
//! (`gait_repl::plain::run_plain_repl`'s inner `run_plain_loop`, exercised
//! here via the test-only `run_plain_script_fresh` wrapper) drives a real
//! turn through `gait_turn_runner::run_turn_source` and a real `mock_brain`
//! process — with `rich-tui` off. Companion to `repl_drive_e2e.rs` (the rich
//! REPL's own mock-brain e2e test, which additionally requires `rich-tui`
//! since it renders through the themed `ReplBuilder`); this file only needs
//! `dev-harness`, proving the plain flavor's independence from the rich
//! rendering stack even for a real, brain-backed turn.
use gait_repl::plain::run_plain_script_fresh;
use gait_repl::turn_script::{Frame, Script, Turn};

#[tokio::test]
async fn plain_repl_drives_a_real_turn_through_mock_brain_and_exits_cleanly() {
    let script = Script {
        turns: vec![
            Turn {
                input: "hello".into(),
                frames: vec![
                    Frame::Delta("hi there\n".into()),
                    Frame::TerminatedOk("done".into()),
                ],
            },
            Turn {
                input: "/q".into(),
                frames: vec![],
            },
        ],
    };

    let mock_brain = std::path::PathBuf::from(env!("CARGO_BIN_EXE_mock_brain"));
    let (_ws, output) = run_plain_script_fresh(&script, &mock_brain)
        .await
        .expect("run_plain_script_fresh must complete the scripted session and exit on /q");

    assert!(
        output.contains("hi there") && output.contains("assistant"),
        "the mock brain's real streamed reply must appear in the plain REPL's own output: {output:?}"
    );
    assert!(
        !output.contains("\x1b[2J"),
        "no screen-clear control code (rich-TUI-only): {output:?}"
    );
    assert!(
        !output.contains("\x1b[?1049h"),
        "no alternate-screen-buffer control code (rich-TUI-only): {output:?}"
    );
}

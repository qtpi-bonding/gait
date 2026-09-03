//! End-to-end proof that the dev-harness drives the real REPL turn-engine
//! path (gait_workflow::single_prompt_run, via ScriptedReader + mock_brain)
//! through a multi-turn script and produces an annotated transcript — see
//! docs/superpowers/specs/2026-07-20-repl-drive-harness-design.md.
//!
//! `harness = false` (see Cargo.toml's `[[test]]` entry for this file): the
//! default libtest harness installs a process-wide output-capture sink that
//! intercepts every `print!`/`eprint!` call — on ANY thread, not just the
//! test's own — before it reaches a real OS-level fd, diverting it into
//! libtest's own buffer instead. `ui::capture::Capture` (used by
//! `run_script_fresh` via `single_prompt_run`'s renderer) works by dup2-ing
//! the real stderr fd and reading back whatever lands on it; under the
//! default harness that's nothing at all — `Capture::end()` silently returns
//! zero bytes regardless of what the renderer actually rendered, only
//! visible when a test panics and libtest dumps its own buffer verbatim
//! (which is why raw ANSI/spinner text appears in a failing run's terminal
//! output even though the returned transcript is empty). `harness = false`
//! skips test::test_main entirely, so no such capture is ever installed and
//! `Capture` sees everything it's supposed to.
use std::path::Path;

use gait_repl::dev_harness::{run_script_fresh, Frame, Script, Turn};

fn main() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");
    rt.block_on(three_turn_script_produces_an_ordered_annotated_transcript());
    println!("test three_turn_script_produces_an_ordered_annotated_transcript ... ok");
    rt.block_on(structured_only_completion_renders_the_done_message());
    println!("test structured_only_completion_renders_the_done_message ... ok");
    rt.block_on(repl_two_messages_reuse_one_session());
    println!("test repl_two_messages_reuse_one_session ... ok");
}

// Characterization test (gait-min-composition plan, Task 1): a multi-turn
// REPL session — two real turns followed by `/q` — must complete both turns
// within the same session (one `run_script_fresh` drive), proving the REPL's
// current turn-reuse behavior before any refactor.
//
// NOTE: a bare `terminated_ok` message (no `Delta`/`TurnDone` frame) is not
// itself rendered into the transcript (confirmed empirically — see the RED
// evidence in task-1-report.md), so each turn here streams a distinct,
// literal reply via `Delta` and the assertion checks both replies appear, in
// order.
async fn repl_two_messages_reuse_one_session() {
    let script: Script = serde_yaml::from_str(
        "turns:\n  - input: \"first task\"\n    frames:\n      - delta: \"first reply\\n\"\n      - terminated_ok: \"done\"\n  - input: \"second task\"\n    frames:\n      - delta: \"second reply\\n\"\n      - terminated_ok: \"done\"\n  - input: \"/q\"\n",
    )
    .unwrap();
    let (_ws, transcript) = run_script_fresh(&script, Path::new(env!("CARGO_BIN_EXE_mock_brain")))
        .await
        .unwrap();
    let first_at = transcript
        .find("first reply")
        .unwrap_or_else(|| panic!("turn 1's reply must appear in the transcript: {transcript}"));
    let second_at = transcript
        .find("second reply")
        .unwrap_or_else(|| panic!("turn 2's reply must appear in the transcript: {transcript}"));
    assert!(
        first_at < second_at,
        "both turns must complete, in order, within the same session: {transcript}"
    );
}

async fn structured_only_completion_renders_the_done_message() {
    let message = "Codex structured reply reached the REPL";
    let script = Script {
        turns: vec![Turn {
            input: "hello".into(),
            frames: vec![
                Frame::ToolCall {
                    name: "structured_done".into(),
                    args: serde_json::json!({"message": message}),
                },
                Frame::ToolResult {
                    ok: true,
                    name: "structured_done".into(),
                },
                Frame::TurnDone {
                    message: message.into(),
                    result: None,
                },
                Frame::TerminatedOk(message.into()),
            ],
        }],
    };

    let mock_brain = std::path::PathBuf::from(env!("CARGO_BIN_EXE_mock_brain"));
    let (_tempdir, transcript) = run_script_fresh(&script, &mock_brain)
        .await
        .expect("run_script_fresh");

    assert!(
        transcript.contains("<success>  ✓│</success>") && transcript.contains(message),
        "a structured_done-only turn must render its final message: {transcript}"
    );
}

async fn three_turn_script_produces_an_ordered_annotated_transcript() {
    let script = Script {
        turns: vec![
            Turn {
                input: "what does foo.rs do?".into(),
                frames: vec![
                    Frame::Delta("It defines the Foo struct.\n".into()),
                    Frame::TerminatedOk("done".into()),
                ],
            },
            Turn {
                input: "/sandbox off".into(),
                frames: vec![],
            },
            Turn {
                input: "now add a docstring".into(),
                frames: vec![
                    Frame::ToolCall {
                        name: "read".into(),
                        args: serde_json::json!({"path": "foo.rs"}),
                    },
                    Frame::ToolResult {
                        ok: true,
                        name: "read".into(),
                    },
                    Frame::Delta("Added.\n".into()),
                    Frame::TerminatedOk("done".into()),
                ],
            },
        ],
    };

    let mock_brain = std::path::PathBuf::from(env!("CARGO_BIN_EXE_mock_brain"));
    let (_tempdir, transcript) = run_script_fresh(&script, &mock_brain)
        .await
        .expect("run_script_fresh");

    let turn1_at = transcript
        .find("It defines the Foo struct")
        .expect("turn 1 reply must appear in the transcript");
    let turn3_at = transcript
        .find("Added.")
        .expect("turn 3 reply must appear in the transcript");
    assert!(
        turn1_at < turn3_at,
        "turns must appear in order: {transcript}"
    );
    assert!(
        transcript.contains("read"),
        "the tool call/result from turn 3 must be visible: {transcript}"
    );
}

//! End-to-end proof that the dev-harness drives the real REPL turn-engine
//! path (gait_workflow::single_prompt_run, via ScriptedReader + mock_brain)
//! through a multi-turn script and produces an annotated transcript — see
//! docs/superpowers/specs/2026-07-20-repl-drive-harness-design.md.
use gait_repl::dev_harness::{run_script_fresh, Frame, Script, Turn};

#[tokio::test]
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

//! Integration test: `gait-acp __run-tool` reads a binary Impulse on stdin,
//! runs the named built-in tool, writes a binary Feedback on stdout — proving
//! the `gait-acp` binary answers to the same sealed role tokens `gait` does.
//! Regression test for the bug where `gait_workflow`'s `current_exe()`-based
//! brain launch, invoked from inside `gait-acp`, spawned a `gait-acp` child
//! that had no role dispatch at all and just reran the ACP server, choking on
//! the Nerve init frame it received instead of ACP JSON-RPC.

use std::io::Write;
use std::process::{Command, Stdio};

use gait_proto::gait::sandbox::v1::{
    hand_response, network_scope::Mode, HandResponse, Impulse, NetworkScope, OutputCaptureMode,
    OutputPresentation, Scope,
};
use prost::Message;

fn feedback_from_output(out: std::process::Output) -> gait_proto::gait::sandbox::v1::Feedback {
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let hand_resp =
        HandResponse::decode(out.stdout.as_slice()).expect("valid HandResponse from hand");
    match hand_resp.outcome {
        Some(hand_response::Outcome::Feedback(f)) => f,
        other => panic!("expected Feedback outcome, got {:?}", other),
    }
}

#[test]
fn gait_acp_binary_executes_run_tool_read_in_subprocess() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("hello.txt"), "hi from gait-acp hand").unwrap();

    let args_struct: pbjson_types::Struct =
        serde_json::from_value(serde_json::json!({"file_path": "hello.txt"})).unwrap();
    let dir_str = dir.path().to_string_lossy().into_owned();
    let impulse = Impulse {
        tool: "read".into(),
        args: Some(args_struct),
        requested: Some(Scope {
            read_all: true,
            readable: vec![dir_str.clone()],
            writable: vec![dir_str.clone()],
            network: Some(NetworkScope {
                mode: Mode::Deny as i32,
                hosts: vec![],
            }),
            local_bind: false,
            read_deny: None,
            write_deny: None,
        }),
        prefetch: None,
        shell_whitelist_patterns: vec![],
        output_capture_mode: OutputCaptureMode::Returned as i32,
        output_presentation: OutputPresentation::Unspecified as i32,
    };
    let impulse_bytes = impulse.encode_to_vec();

    let exe = env!("CARGO_BIN_EXE_gait-acp");
    let mut child = Command::new(exe)
        .args([
            "__run-tool",
            "--gait-dir",
            dir.path().to_str().unwrap(),
            "--codebase",
            dir.path().to_str().unwrap(),
            "--session",
            "stest123",
            "--tool",
            "read",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    // Write Impulse bytes to stdin, then drop to close the pipe so the hand
    // can finish reading and exit.
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(&impulse_bytes).unwrap();
    drop(stdin);

    let feedback = feedback_from_output(child.wait_with_output().unwrap());
    // The `read` hand returns the file content wrapped in <file>...</file> as
    // a StringValue (see gait-bin/tests/run_tool_hand.rs's matching test) —
    // not a StructValue, so match that shape here too.
    let result_proto = feedback.result.expect("Feedback.result must be set");
    let result_str = match result_proto.kind {
        Some(pbjson_types::value::Kind::StringValue(s)) => s,
        other => panic!("expected StringValue, got {:?}", other),
    };
    assert!(
        result_str.contains("hi from gait-acp hand"),
        "file content should be in result, got: {:?}",
        result_str
    );
}
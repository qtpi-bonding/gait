//! Integration test: `gait __run-tool` reads a binary Impulse on stdin, runs the
//! named built-in tool, writes a binary Feedback on stdout. Spawned as a
//! subprocess so the test exercises the real boundary code path (not an
//! in-process shortcut).
//!
//! Phase 2a: the first end-to-end proof that the brain→hand wire works for a
//! read-only tool call. The hand runs the real `ReadTool` against a temp
//! codebase; the test asserts the file content survives the binary protobuf hop.

use std::io::Write;
use std::process::{Command, Stdio};

use gait_proto::gait::sandbox::v1::{
    hand_response, network_scope::Mode, HandResponse, Impulse, NetworkScope, Scope,
};
use prost::Message;

#[test]
fn run_tool_executes_read_in_subprocess() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("hello.txt"), "hi from hand").unwrap();

    // Build binary Impulse for a 'read' tool call.
    // The 'read' tool expects args keyed as `file_path` (per ReadParams in
    // tools/read.rs), so the args object must match that schema exactly.
    let args_struct: pbjson_types::Struct =
        serde_json::from_value(serde_json::json!({"file_path": "hello.txt"})).unwrap();
    let dir_str = dir.path().to_string_lossy().into_owned();
    let impulse = Impulse {
        tool: "read".into(),
        args: Some(args_struct),
        cwd: dir_str.clone(),
        // Task 6: ship a `requested` scope that lists the tempdir as writable
        // so the hand applies a SandboxApplied seal end-to-end (read_all=true
        // means reads anywhere succeed, so the file_path lookup works).
        requested: Some(Scope {
            read_all: true,
            readable: vec![dir_str.clone()],
            writable: vec![dir_str.clone()],
            network: Some(NetworkScope {
                mode: Mode::Deny as i32,
                hosts: vec![],
            }),
        }),
        prefetch: None,
    };
    let impulse_bytes = impulse.encode_to_vec();

    let exe = env!("CARGO_BIN_EXE_gait");
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

    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Decode binary HandResponse from stdout.
    let hand_resp =
        HandResponse::decode(out.stdout.as_slice()).expect("valid HandResponse from hand");
    let feedback = match hand_resp.outcome {
        Some(hand_response::Outcome::Feedback(f)) => f,
        other => panic!("expected Feedback outcome, got {:?}", other),
    };
    // Result should be the file content wrapped in <file>...</file> (a string value).
    let result_proto = feedback.result.expect("Feedback.result must be set");
    let result_str = match result_proto.kind {
        Some(pbjson_types::value::Kind::StringValue(s)) => s,
        other => panic!("expected StringValue, got {:?}", other),
    };
    assert!(
        result_str.contains("hi from hand"),
        "file content should be in result, got: {:?}",
        result_str
    );
}

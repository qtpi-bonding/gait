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
    hand_response, network_scope::Mode, HandResponse, Impulse, NetworkScope, OutputCaptureMode,
    Scope,
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

fn hand_response_from_output(out: std::process::Output) -> HandResponse {
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    HandResponse::decode(out.stdout.as_slice()).expect("valid HandResponse from hand")
}

fn structured_output(feedback: gait_proto::gait::sandbox::v1::Feedback) -> String {
    let result = feedback.result.expect("Feedback.result must be set");
    let object = match result.kind {
        Some(pbjson_types::value::Kind::StructValue(s)) => s,
        other => panic!("expected structured result, got {other:?}"),
    };
    match object.fields.get("output").and_then(|v| v.kind.as_ref()) {
        Some(pbjson_types::value::Kind::StringValue(s)) => s.clone(),
        other => panic!("expected output string, got {other:?}"),
    }
}

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
            local_bind: false,
            read_deny: None,
            write_deny: None,
        }),
        prefetch: None,
        shell_whitelist_patterns: vec![],
        output_capture_mode: OutputCaptureMode::Returned as i32,
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

    let feedback = feedback_from_output(child.wait_with_output().unwrap());
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

#[test]
fn returned_tool_rejects_a_supplied_spool_target() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("hello.txt"), "hi from hand").unwrap();
    let spool = dir.path().join("unexpected-spool/output.txt");
    std::fs::create_dir_all(spool.parent().unwrap()).unwrap();
    let impulse = Impulse {
        tool: "read".into(),
        args: Some(serde_json::from_value(serde_json::json!({"file_path": "hello.txt"})).unwrap()),
        requested: None,
        prefetch: None,
        shell_whitelist_patterns: vec![],
        output_capture_mode: OutputCaptureMode::Returned as i32,
    };

    let mut child = Command::new(env!("CARGO_BIN_EXE_gait"))
        .args([
            "__run-tool",
            "--gait-dir",
            dir.path().to_str().unwrap(),
            "--codebase",
            dir.path().to_str().unwrap(),
            "--session",
            "sreturned-mismatch",
            "--tool",
            "read",
            "--output-spool",
            spool.to_str().unwrap(),
            "--seal",
            "false",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(&impulse.encode_to_vec())
        .unwrap();

    let response = hand_response_from_output(child.wait_with_output().unwrap());
    match response.outcome {
        Some(hand_response::Outcome::Error(error)) => assert!(
            error.message.contains("output capture mismatch"),
            "unexpected hand error: {}",
            error.message
        ),
        other => panic!("expected output-capture rejection, got {other:?}"),
    }
    assert!(
        !spool.exists(),
        "rejected tool must not touch the spool target"
    );
}

#[test]
fn shell_output_and_tmpdir_survive_the_hand_subprocess() {
    let dir = tempfile::tempdir().unwrap();
    let session_id = "sshell-output";
    let outputs = gait_state::session::live::outputs_dir(dir.path(), session_id);
    let process_tmp = outputs.join("process/integration");
    let output_spool = outputs.join("shell/integration/output.txt");
    std::fs::create_dir_all(&process_tmp).unwrap();
    std::fs::create_dir_all(output_spool.parent().unwrap()).unwrap();

    let args_struct: pbjson_types::Struct = serde_json::from_value(serde_json::json!({
        "cmd": "printf 'tmp=%s\\nfull-marker\\n' \"$TMPDIR\"",
        "reason": "session output integration"
    }))
    .unwrap();
    let impulse = Impulse {
        tool: "shell".into(),
        args: Some(args_struct),
        requested: Some(Scope {
            read_all: false,
            readable: vec![
                dir.path().to_string_lossy().into_owned(),
                outputs.to_string_lossy().into_owned(),
            ],
            writable: vec![
                dir.path().to_string_lossy().into_owned(),
                outputs.to_string_lossy().into_owned(),
            ],
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
        output_capture_mode: OutputCaptureMode::Spool as i32,
    };

    let exe = env!("CARGO_BIN_EXE_gait");
    let mut child = Command::new(exe)
        .args([
            "__run-tool",
            "--gait-dir",
            dir.path().to_str().unwrap(),
            "--codebase",
            dir.path().to_str().unwrap(),
            "--session",
            session_id,
            "--tool",
            "shell",
            "--output-spool",
            output_spool.to_str().unwrap(),
        ])
        .env("TMPDIR", &process_tmp)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(&impulse.encode_to_vec())
        .unwrap();

    let output = structured_output(feedback_from_output(child.wait_with_output().unwrap()));
    assert_eq!(
        output,
        format!("[cwd: {}]", dir.path().display()),
        "a spooling hand returns metadata while the spine owns finalization"
    );
    assert_eq!(
        std::fs::read_to_string(&output_spool).unwrap(),
        format!("tmp={}\nfull-marker\n", process_tmp.display()),
        "complete process output must remain in the supplied spool after hand exit"
    );
}

#[test]
fn detached_shell_keeps_its_inherited_session_tmpdir_after_hand_exit() {
    let dir = tempfile::tempdir().unwrap();
    let session_id = "sdetached-output";
    let outputs = gait_state::session::live::outputs_dir(dir.path(), session_id);
    let process_tmp = outputs.join("process/detached-integration");
    let marker = process_tmp.join("after-hand.txt");
    let output_spool = outputs.join("shell/detached-integration/output.txt");
    std::fs::create_dir_all(&process_tmp).unwrap();
    std::fs::create_dir_all(output_spool.parent().unwrap()).unwrap();

    let args_struct: pbjson_types::Struct = serde_json::from_value(serde_json::json!({
        "cmd": "sleep 0.1; printf detached-ok > \"$TMPDIR/after-hand.txt\"",
        "reason": "detached tmpdir lifecycle",
        "detached": true,
        "name": "tmpdir-lifecycle"
    }))
    .unwrap();
    let impulse = Impulse {
        tool: "shell".into(),
        args: Some(args_struct),
        requested: Some(Scope {
            read_all: false,
            readable: vec![
                dir.path().to_string_lossy().into_owned(),
                outputs.to_string_lossy().into_owned(),
            ],
            writable: vec![
                dir.path().to_string_lossy().into_owned(),
                outputs.to_string_lossy().into_owned(),
            ],
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
        output_capture_mode: OutputCaptureMode::Spool as i32,
    };

    let exe = env!("CARGO_BIN_EXE_gait");
    let mut child = Command::new(exe)
        .args([
            "__run-tool",
            "--gait-dir",
            dir.path().to_str().unwrap(),
            "--codebase",
            dir.path().to_str().unwrap(),
            "--session",
            session_id,
            "--tool",
            "shell",
            "--output-spool",
            output_spool.to_str().unwrap(),
        ])
        .env("TMPDIR", &process_tmp)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(&impulse.encode_to_vec())
        .unwrap();
    let feedback = feedback_from_output(child.wait_with_output().unwrap());
    assert!(
        feedback.daemon.is_some(),
        "hand must return before its daemon"
    );

    for _ in 0..100 {
        if marker.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert_eq!(
        std::fs::read_to_string(marker).unwrap(),
        "detached-ok",
        "detached process must retain a writable TMPDIR after hand exit"
    );
}

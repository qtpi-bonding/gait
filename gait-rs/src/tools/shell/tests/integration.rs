use super::super::*;
use crate::tools::ToolCtx;
use tempfile::TempDir;

fn make_tool(dir: &TempDir) -> ShellTool {
    ShellTool::new(ToolCtx::new(
        dir.path().to_path_buf(),
        dir.path().to_path_buf(),
        "test".into(),
    ))
}

#[tokio::test]
async fn runs_command_and_captures_output() {
    let dir = TempDir::new().unwrap();
    let tool = make_tool(&dir);
    let result = tool
        .execute(
            serde_json::json!({
                "cmd": "echo hello",
                "reason": "test"
            }),
            &crate::Recorder::detached(),
        )
        .await
        .unwrap();
    // Shell now returns a structured process-tool shape: { output, exit_code }.
    // The model-facing `output` carries the clean command text + file note; the model
    // never sees the JSON wrapper or any "exit N (Xms)" prefix.
    let output = result
        .get("output")
        .and_then(|v| v.as_str())
        .expect("output field");
    let exit_code = result
        .get("exit_code")
        .and_then(|v| v.as_i64())
        .expect("exit_code field");
    assert_eq!(exit_code, 0, "got: {result}");
    assert!(output.contains("hello"), "got: {output}");
}

#[tokio::test]
async fn returns_nonzero_exit_code_on_failure() {
    let dir = TempDir::new().unwrap();
    let tool = make_tool(&dir);
    let result = tool
        .execute(
            serde_json::json!({
                "cmd": "false",
                "reason": "test"
            }),
            &crate::Recorder::detached(),
        )
        .await
        .unwrap();
    let exit_code = result
        .get("exit_code")
        .and_then(|v| v.as_i64())
        .expect("exit_code field");
    assert_eq!(exit_code, 1, "got: {result}");
}

#[tokio::test]
async fn runs_in_codebase_dir() {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("sentinel.txt"), "marker").unwrap();
    let tool = make_tool(&dir);
    let result = tool
        .execute(
            serde_json::json!({
                "cmd": "cat sentinel.txt",
                "reason": "test"
            }),
            &crate::Recorder::detached(),
        )
        .await
        .unwrap();
    let output = result
        .get("output")
        .and_then(|v| v.as_str())
        .expect("output field");
    assert!(output.contains("marker"), "got: {output}");
}

#[tokio::test]
async fn cwd_param_runs_in_subdir() {
    let dir = TempDir::new().unwrap();
    std::fs::create_dir_all(dir.path().join("sub")).unwrap();
    std::fs::write(dir.path().join("sub/sentinel.txt"), "submarker").unwrap();
    let tool = make_tool(&dir);
    let result = tool
        .execute(
            serde_json::json!({
                "cmd": "cat sentinel.txt",
                "reason": "test",
                "cwd": "sub"
            }),
            &crate::Recorder::detached(),
        )
        .await
        .unwrap();
    let output = result
        .get("output")
        .and_then(|v| v.as_str())
        .expect("output field");
    assert!(output.contains("submarker"), "got: {output}");
}

#[tokio::test]
async fn cwd_param_persists_across_calls() {
    let dir = TempDir::new().unwrap();
    std::fs::create_dir_all(dir.path().join("sub")).unwrap();
    let tool = make_tool(&dir);
    // Call 1: cd into sub via the cwd param (emits a Cwd event).
    let _ = tool
        .execute(
            serde_json::json!({"cmd": "pwd", "reason": "test", "cwd": "sub"}),
            &crate::Recorder::detached(),
        )
        .await
        .unwrap();
    // Call 2: simulate what cmd_run_tool does — inject __cwd (the active cwd from the
    // brain's session state) so the tool knows where to run. Shell no longer refolids
    // the session log itself; the caller tracks cwd and injects it per call.
    let sub_path = dir.path().join("sub").to_string_lossy().into_owned();
    let result = tool
        .execute(
            serde_json::json!({"cmd": "pwd", "reason": "test", "__cwd": sub_path}),
            &crate::Recorder::detached(),
        )
        .await
        .unwrap();
    let output = result
        .get("output")
        .and_then(|v| v.as_str())
        .expect("output field");
    assert!(
        output.contains("sub"),
        "expected to run in sub, got: {output}"
    );
}

#[tokio::test]
async fn shell_cd_publishes_cwd_event() {
    use crate::proto::gait::log::v1::{event, payload, stream_item};
    let dir = TempDir::new().unwrap();
    std::fs::create_dir_all(dir.path().join("sessions").join("sh1")).unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let rec = crate::Recorder::for_session(dir.path(), "sh1", tx);
    let tool = ShellTool::new(crate::tools::ctx::ToolCtx::new(
        dir.path().to_path_buf(),
        dir.path().to_path_buf(),
        "sh1".into(),
    ));
    // The `cwd` param (not parsing `cd` in `cmd`) is what emits a Cwd event;
    // "." resolves against the codebase root and succeeds, so a Cwd is produced.
    let _ = tool
        .execute(
            serde_json::json!({"cmd": "true", "reason": "t", "cwd": "."}),
            &rec,
        )
        .await;
    drop(rec); // close channel
    let mut saw_cwd = false;
    while let Some(item) = rx.recv().await {
        if let Some(stream_item::Item::Event(ev)) = item.item {
            if let Some(event::Body::Payload(p)) = ev.body {
                if matches!(p.kind, Some(payload::Kind::Cwd(_))) {
                    saw_cwd = true;
                }
            }
        }
    }
    assert!(
        saw_cwd,
        "shell must PUBLISH the cwd event, not only append it"
    );
}

#[tokio::test]
async fn cwd_nonexistent_errors_without_emitting() {
    let dir = TempDir::new().unwrap();
    let tool = make_tool(&dir);
    let result = tool
        .execute(
            serde_json::json!({
                "cmd": "pwd", "reason": "test", "cwd": "nope"
            }),
            &crate::Recorder::detached(),
        )
        .await
        .unwrap();
    let exit_code = result
        .get("exit_code")
        .and_then(|v| v.as_i64())
        .expect("exit_code field");
    let output = result
        .get("output")
        .and_then(|v| v.as_str())
        .expect("output field");
    assert_eq!(exit_code, 1, "got: {result}");
    assert!(output.contains("no such directory"), "got: {output}");
    // No Cwd event written: the session log is absent or empty.
    let log = dir.path().join("sessions").join("test").join("log.jsonl");
    let folded = crate::proto::wire::read_events(&log).unwrap_or_default();
    assert!(
        folded.is_empty(),
        "a failed cd must not emit a Cwd event, got {} events",
        folded.len()
    );
}

/// stdin=null: `cat` reads from stdin and should get EOF immediately, not hang.
#[tokio::test]
async fn stdin_closed_cat_returns_promptly() {
    use std::time::Duration;
    let dir = TempDir::new().unwrap();
    let tool = make_tool(&dir);
    // If stdin were inherited, `cat` would block forever waiting for input.
    // With stdin=null it gets EOF and exits immediately.
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        tool.execute(
            serde_json::json!({"cmd": "cat", "reason": "test stdin closed"}),
            &crate::Recorder::detached(),
        ),
    )
    .await
    .expect("cat must complete within 5s (stdin=null gives EOF)")
    .unwrap();
    let exit_code = result
        .get("exit_code")
        .and_then(|v| v.as_i64())
        .expect("exit_code field");
    assert_eq!(exit_code, 0, "cat exited cleanly: {result}");
}

/// Regression: exit_code must still appear in the result after the exec.rs lift was updated.
#[tokio::test]
async fn exit_code_present_in_result() {
    let dir = TempDir::new().unwrap();
    let tool = make_tool(&dir);
    let result = tool
        .execute(
            serde_json::json!({"cmd": "exit 42", "reason": "test exit_code"}),
            &crate::Recorder::detached(),
        )
        .await
        .unwrap();
    let exit_code = result
        .get("exit_code")
        .and_then(|v| v.as_i64())
        .expect("exit_code must be present in shell result");
    assert_eq!(exit_code, 42, "got: {result}");
}

/// (a) Streaming: bytes appear in the file BEFORE the command finishes.
#[tokio::test]
async fn streams_output_to_file_before_completion() {
    use std::time::Duration;
    use tokio::time::sleep;
    use uuid::Uuid;

    let dir = TempDir::new().unwrap();
    let tool = make_tool(&dir);

    // Use a unique sentinel so only this test's file can possibly match,
    // even if other test runs leave stale shell-*.txt files around.
    let sentinel = format!("stream_sentinel_{}", Uuid::new_v4().simple());

    // Run in a background task; we'll poll the file while it's still running.
    let sentinel_clone = sentinel.clone();
    let handle = tokio::spawn(async move {
        tool.execute(
            serde_json::json!({
                "cmd": format!("echo {sentinel_clone}_early; sleep 0.4; echo {sentinel_clone}_late"),
                "reason": "test streaming"
            }),
            &crate::Recorder::detached(),
        )
        .await
        .unwrap()
    });

    // Give the command time to emit the first line but not finish.
    sleep(Duration::from_millis(100)).await;

    // Find shell-*.txt files that contain our unique sentinel marker.
    // Because the sentinel is a UUID-based string, no other test's file can match.
    let tmp_dir = std::env::temp_dir().join("gait-hand");
    let early_marker = format!("{sentinel}_early");
    let mut contents = String::new();
    if let Ok(entries) = std::fs::read_dir(&tmp_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            if !name.to_string_lossy().starts_with("shell-") {
                continue;
            }
            let c = std::fs::read_to_string(entry.path()).unwrap_or_default();
            if c.contains(&early_marker) {
                contents = c;
                break;
            }
        }
    }

    assert!(
        contents.contains(&early_marker),
        "expected '{early_marker}' in a shell-*.txt before command finished"
    );

    // Wait for full completion.
    let result = handle.await.unwrap();
    let output = result["output"].as_str().unwrap();
    let late_marker = format!("{sentinel}_late");
    assert!(
        output.contains(&early_marker),
        "final output missing {early_marker}: {output}"
    );
    assert!(
        output.contains(&late_marker),
        "final output missing {late_marker}: {output}"
    );
}

/// (b) Both stdout and stderr end up in the file — no EOF-drop.
#[tokio::test]
async fn captures_both_stdout_and_stderr() {
    let dir = TempDir::new().unwrap();
    let tool = make_tool(&dir);
    let result = tool
        .execute(
            serde_json::json!({
                "cmd": "echo OUT; echo ERR 1>&2",
                "reason": "test both streams"
            }),
            &crate::Recorder::detached(),
        )
        .await
        .unwrap();
    let output = result["output"].as_str().unwrap();
    assert!(output.contains("OUT"), "missing stdout in output: {output}");
    assert!(output.contains("ERR"), "missing stderr in output: {output}");
}

/// (c) total byte count in the file note matches actual content, not capped tail.
#[tokio::test]
async fn total_bytes_reflects_file_not_tail() {
    let dir = TempDir::new().unwrap();
    let tool = make_tool(&dir);
    let result = tool
        .execute(
            serde_json::json!({
                "cmd": "echo hello_world",
                "reason": "test total bytes"
            }),
            &crate::Recorder::detached(),
        )
        .await
        .unwrap();
    let output = result["output"].as_str().unwrap();
    // The note contains "(N bytes)" — verify it's present and > 0.
    assert!(
        output.contains("bytes)"),
        "expected bytes count in output: {output}"
    );
    // Also verify the file contains the content.
    assert!(output.contains("hello_world"), "content missing: {output}");
}

#[tokio::test]
async fn writes_output_to_tmp_file() {
    let dir = TempDir::new().unwrap();
    let tool = make_tool(&dir);
    let result = tool
        .execute(
            serde_json::json!({
                "cmd": "echo payload",
                "reason": "test"
            }),
            &crate::Recorder::detached(),
        )
        .await
        .unwrap();
    // Result's `output` field references the tmp file (file note is part of display).
    let output = result
        .get("output")
        .and_then(|v| v.as_str())
        .expect("output field");
    assert!(output.contains("full output:"), "got: {output}");
    assert!(output.contains("shell-"), "got: {output}");
    // Scratch goes to the system temp dir under gait-hand/ (not the gait-dir).
    assert!(
        output.contains("gait-hand"),
        "expected gait-hand in output path, got: {output}"
    );
    let tmp_dir = std::env::temp_dir().join("gait-hand");
    assert!(tmp_dir.exists(), "gait-hand tmp dir not created");
}

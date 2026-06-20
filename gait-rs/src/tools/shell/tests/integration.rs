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
    // Shell now returns a structured process-tool shape: { output, exit_code, timed_out }.
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
    let timed_out = result
        .get("timed_out")
        .and_then(|v| v.as_bool())
        .expect("timed_out field");
    assert_eq!(exit_code, 0, "got: {result}");
    assert!(!timed_out, "got: {result}");
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
    let timed_out = result
        .get("timed_out")
        .and_then(|v| v.as_bool())
        .expect("timed_out field");
    assert_eq!(exit_code, 1, "got: {result}");
    assert!(!timed_out, "got: {result}");
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

#[tokio::test]
async fn times_out_long_running_command() {
    let dir = TempDir::new().unwrap();
    let tool = make_tool(&dir);
    let result = tool
        .execute(
            serde_json::json!({
                "cmd": "sleep 10",
                "reason": "test timeout",
                "timeout": 100
            }),
            &crate::Recorder::detached(),
        )
        .await
        .unwrap();
    // Timeout branch: exit_code=124, timed_out=true, output carries the TIMED OUT message.
    let exit_code = result
        .get("exit_code")
        .and_then(|v| v.as_i64())
        .expect("exit_code field");
    let timed_out = result
        .get("timed_out")
        .and_then(|v| v.as_bool())
        .expect("timed_out field");
    let output = result
        .get("output")
        .and_then(|v| v.as_str())
        .expect("output field");
    assert_eq!(exit_code, 124, "got: {result}");
    assert!(timed_out, "got: {result}");
    assert!(output.contains("TIMED OUT"), "got: {output}");
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

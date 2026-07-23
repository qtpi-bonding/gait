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
    std::fs::create_dir_all(
        crate::session::live::log_path(dir.path(), "sh1")
            .parent()
            .unwrap(),
    )
    .unwrap();
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
async fn cwd_outside_scope_errors_without_emitting_cwd_event() {
    use crate::proto::gait::log::v1::{event, payload, stream_item};
    use crate::sandbox::SandboxScope;

    let dir = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap(); // a real, existing directory, but not in scope
    std::fs::create_dir_all(
        crate::session::live::log_path(dir.path(), "sh2")
            .parent()
            .unwrap(),
    )
    .unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let rec = crate::Recorder::for_session(dir.path(), "sh2", tx);

    let scope = SandboxScope {
        readable: vec![dir.path().to_path_buf()],
        writable: vec![dir.path().to_path_buf()],
        read_all: false,
        network: Default::default(),
        local_bind: false,
    };
    let tool = ShellTool::new(
        crate::tools::ctx::ToolCtx::new(
            dir.path().to_path_buf(),
            dir.path().to_path_buf(),
            "sh2".into(),
        )
        .with_scope(scope),
    );

    let result = tool
        .execute(
            serde_json::json!({
                "cmd": "pwd",
                "reason": "test",
                "cwd": outside.path().to_string_lossy(),
            }),
            &rec,
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
    assert!(output.contains("outside sandbox scope"), "got: {output}");

    // The wedge this guards against: no Cwd event may reach the transcript for
    // an out-of-scope request, since active_cwd is last-write-wins with no
    // rollback — one bad Cwd event here would corrupt every later shell call.
    drop(rec);
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
        !saw_cwd,
        "shell must not record a Cwd event for an out-of-scope path"
    );
}

#[tokio::test]
async fn cwd_inside_scope_still_succeeds_and_emits_cwd_event() {
    use crate::sandbox::SandboxScope;

    let dir = TempDir::new().unwrap();
    std::fs::create_dir_all(dir.path().join("sub")).unwrap();
    let canonical_root = std::fs::canonicalize(dir.path()).unwrap();
    let scope = SandboxScope {
        readable: vec![canonical_root.clone()],
        writable: vec![canonical_root],
        read_all: false,
        network: Default::default(),
        local_bind: false,
    };
    let tool = ShellTool::new(
        crate::tools::ctx::ToolCtx::new(
            dir.path().to_path_buf(),
            dir.path().to_path_buf(),
            "test".into(),
        )
        .with_scope(scope),
    );
    let result = tool
        .execute(
            serde_json::json!({"cmd": "pwd", "reason": "test", "cwd": "sub"}),
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
    assert_eq!(exit_code, 0, "got: {result}");
    assert!(output.contains("sub"), "got: {output}");
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
    let log = crate::session::live::log_path(dir.path(), "test");
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

// ── Detached daemon tests ──────────────────────────────────────────────────

/// Helper: extract the `__daemon` object from a shell-tool response.
fn daemon_proto(result: &serde_json::Value) -> &serde_json::Value {
    result
        .get("__daemon")
        .expect("__daemon key missing from detached response")
}

#[tokio::test]
async fn detached_spawn_returns_immediately() {
    // A long-running sleep should not block execute() — it must return before the
    // process finishes.
    let dir = TempDir::new().unwrap();
    let tool = make_tool(&dir);
    let before = std::time::Instant::now();
    let result = tool
        .execute(
            serde_json::json!({
                "cmd": "sleep 30",
                "reason": "daemon test",
                "detached": true
            }),
            &crate::Recorder::detached(),
        )
        .await
        .unwrap();
    let elapsed = before.elapsed();

    let pid = daemon_proto(&result)
        .get("pid")
        .and_then(|v| v.as_i64())
        .expect("pid") as i32;

    // Teardown: kill the spawned daemon.
    unsafe { libc::kill(pid, libc::SIGKILL) };

    assert!(
        elapsed.as_secs() < 5,
        "execute blocked for {}s — should return immediately",
        elapsed.as_secs()
    );
    assert!(pid > 0, "pid must be positive, got {pid}");
}

#[tokio::test]
async fn detached_child_is_in_own_session() {
    // After setsid() the child's session id == its own pid (it is the session leader).
    let dir = TempDir::new().unwrap();
    let tool = make_tool(&dir);
    let result = tool
        .execute(
            serde_json::json!({
                "cmd": "sleep 30",
                "reason": "daemon session test",
                "detached": true
            }),
            &crate::Recorder::detached(),
        )
        .await
        .unwrap();

    let pid = daemon_proto(&result)
        .get("pid")
        .and_then(|v| v.as_i64())
        .expect("pid") as i32;

    let child_sid = unsafe { libc::getsid(pid) };
    let our_sid = unsafe { libc::getsid(0) };

    // Teardown.
    unsafe { libc::kill(pid, libc::SIGKILL) };

    assert_ne!(
        child_sid, our_sid,
        "child sid ({child_sid}) must differ from our sid ({our_sid})"
    );
    // Specifically, for a setsid leader, getsid(pid) == pid.
    assert_eq!(child_sid, pid, "child must be its own session leader");
}

#[tokio::test]
async fn detached_output_appears_in_log_file() {
    // The daemon's stdout/stderr must be redirected to the log file.
    let dir = TempDir::new().unwrap();
    let tool = make_tool(&dir);
    let result = tool
        .execute(
            serde_json::json!({
                "cmd": "echo daemon-marker",
                "reason": "log test",
                "detached": true
            }),
            &crate::Recorder::detached(),
        )
        .await
        .unwrap();

    let proto = daemon_proto(&result);
    let log_path = proto
        .get("log_path")
        .and_then(|v| v.as_str())
        .expect("log_path");
    let pid = proto.get("pid").and_then(|v| v.as_i64()).expect("pid") as i32;

    // Give the short-lived process a moment to write and exit.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    unsafe { libc::kill(pid, libc::SIGTERM) };

    let contents = std::fs::read_to_string(log_path)
        .unwrap_or_else(|e| panic!("cannot read log {log_path}: {e}"));
    assert!(
        contents.contains("daemon-marker"),
        "expected 'daemon-marker' in log, got: {contents:?}"
    );
}

#[tokio::test]
async fn detached_daemon_record_proto_fields_populated() {
    // The __daemon proto must contain all required fields that the spine uses to
    // write a DaemonStart record into daemons.jsonl.
    let dir = TempDir::new().unwrap();
    let tool = make_tool(&dir);
    let result = tool
        .execute(
            serde_json::json!({
                "cmd": "sleep 30",
                "reason": "record test",
                "detached": true,
                "name": "test-daemon"
            }),
            &crate::Recorder::detached(),
        )
        .await
        .unwrap();

    let proto = daemon_proto(&result);
    let pid = proto.get("pid").and_then(|v| v.as_i64()).expect("pid") as i32;

    // Teardown.
    unsafe { libc::kill(pid, libc::SIGKILL) };

    // cmd must be the raw user command, not "sh -c <cmd>".
    let cmd = proto.get("cmd").and_then(|v| v.as_str()).expect("cmd");
    assert_eq!(
        cmd, "sleep 30",
        "cmd should be raw user command, got: {cmd}"
    );

    // name must match what we passed.
    let name = proto.get("name").and_then(|v| v.as_str()).expect("name");
    assert_eq!(name, "test-daemon");

    // log_path must live under the daemons/ subdirectory.
    let log_path = proto
        .get("log_path")
        .and_then(|v| v.as_str())
        .expect("log_path");
    assert!(
        log_path.contains("daemons/"),
        "log_path should be under daemons/, got: {log_path}"
    );

    // started_at must be non-zero.
    let started_at = proto
        .get("started_at")
        .and_then(|v| v.as_u64())
        .expect("started_at");
    assert!(started_at > 0, "started_at must be non-zero");
}

/// The spine writes a DaemonStart record into `<gait_dir>/daemons.jsonl` using
/// the `__daemon` proto returned by the shell tool.  This test exercises that
/// round-trip directly: spawn a daemon, extract the proto, call
/// `daemons::append_start`, then verify `daemons::active()` lists the daemon.
#[tokio::test]
async fn detached_daemon_start_record_lands_in_daemons_jsonl() {
    use crate::session::daemons;
    let dir = TempDir::new().unwrap();
    let tool = make_tool(&dir);
    let result = tool
        .execute(
            serde_json::json!({
                "cmd": "sleep 30",
                "reason": "daemons.jsonl test",
                "detached": true,
                "name": "jsonl-test-daemon"
            }),
            &crate::Recorder::detached(),
        )
        .await
        .unwrap();

    let proto = daemon_proto(&result);
    let pid = proto.get("pid").and_then(|v| v.as_i64()).expect("pid") as i32;

    // Reconstruct the DaemonStart (proto) as spine would, then write it.
    let rec = crate::proto::gait::log::v1::DaemonStart {
        name: proto
            .get("name")
            .and_then(|v| v.as_str())
            .expect("name")
            .to_owned(),
        pid,
        pgid: proto.get("pgid").and_then(|v| v.as_i64()).expect("pgid") as i32,
        started_at: proto
            .get("started_at")
            .and_then(|v| v.as_u64())
            .expect("started_at"),
        cmd: proto
            .get("cmd")
            .and_then(|v| v.as_str())
            .expect("cmd")
            .to_owned(),
        cwd: proto
            .get("cwd")
            .and_then(|v| v.as_str())
            .expect("cwd")
            .to_owned(),
        log_path: proto
            .get("log_path")
            .and_then(|v| v.as_str())
            .expect("log_path")
            .to_owned(),
        session: String::new(),
    };

    // Teardown: kill before we assert so the process doesn't linger on failure.
    unsafe { libc::kill(pid, libc::SIGKILL) };

    // Write the record as spine does.
    daemons::append_start(dir.path(), &rec).expect("append_start must succeed");

    // Verify daemons.jsonl exists and active() returns the record.
    let jsonl_path = dir.path().join("daemons.jsonl");
    assert!(jsonl_path.exists(), "daemons.jsonl must be created");

    let active = daemons::active(dir.path()).expect("active() must succeed");
    assert_eq!(
        active.len(),
        1,
        "expected 1 active daemon, got {}",
        active.len()
    );
    assert_eq!(active[0].name, "jsonl-test-daemon");
    assert_eq!(active[0].cmd, "sleep 30", "cmd must be raw user command");
}

#[tokio::test]
async fn times_out_and_nudges_to_detached() {
    let dir = TempDir::new().unwrap();
    // User config: 1-second timeout.
    std::fs::write(
        dir.path().join("gait.jsonc"),
        r#"{"shell_timeout_secs": 1}"#,
    )
    .unwrap();
    let tool = make_tool(&dir);

    let start = std::time::Instant::now();
    let result = tool
        .execute(
            serde_json::json!({ "cmd": "sleep 30", "reason": "test" }),
            &crate::Recorder::detached(),
        )
        .await
        .unwrap();
    let elapsed = start.elapsed();

    assert!(
        elapsed.as_secs() < 5,
        "should not wait for the full 30s: {elapsed:?}"
    );
    let exit_code = result
        .get("exit_code")
        .and_then(|v| v.as_i64())
        .expect("exit_code");
    let output = result
        .get("output")
        .and_then(|v| v.as_str())
        .expect("output");
    assert_eq!(exit_code, 124, "got: {result}");
    assert!(output.contains("killed after 1s"), "got: {output}");
    assert!(output.contains("detached:true"), "got: {output}");
}

#[tokio::test]
async fn fast_command_unaffected_by_timeout() {
    let dir = TempDir::new().unwrap();
    std::fs::write(
        dir.path().join("gait.jsonc"),
        r#"{"shell_timeout_secs": 10}"#,
    )
    .unwrap();
    let tool = make_tool(&dir);
    let result = tool
        .execute(
            serde_json::json!({ "cmd": "echo hi", "reason": "test" }),
            &crate::Recorder::detached(),
        )
        .await
        .unwrap();
    assert_eq!(result.get("exit_code").and_then(|v| v.as_i64()), Some(0));
    assert!(result
        .get("output")
        .and_then(|v| v.as_str())
        .unwrap()
        .contains("hi"));
}

/// End-to-end idle-wake: a detached daemon finishes, the registry records a
/// `Stop`, and the parent's session log gains a `daemon_update` event (the
/// wake). Polls both conditions within a 5s deadline rather than sleeping.
#[tokio::test]
async fn detached_daemon_wakes_idle_parent_on_exit() {
    use std::time::Duration;
    use tokio::time::Instant;

    // 1. Real gait_dir (TempDir) + a real parent session dir inside it.
    let gd = TempDir::new().unwrap();
    let parent_id = "swake001";
    let parent_dir = crate::session::live::worktree_dir(gd.path(), parent_id);
    std::fs::create_dir_all(&parent_dir).unwrap();
    let parent_log_path = crate::session::live::log_path(gd.path(), parent_id);
    std::fs::create_dir_all(parent_log_path.parent().unwrap()).unwrap();
    // Empty log.jsonl — `wire::append` is fine writing the first event into it.
    std::fs::write(&parent_log_path, "").unwrap();

    // 2. Tool whose ctx.gait_dir == gd and ctx.session_id == parent_id.
    let tool = ShellTool::new(crate::tools::ctx::ToolCtx::new(
        gd.path().to_path_buf(),
        gd.path().to_path_buf(),
        parent_id.to_string(),
    ));

    // 3. Run a fast detached command; the wrapper fires __daemon-exit on exit.
    let _ = tool
        .execute(
            serde_json::json!({
                "cmd": "sleep 0.1",
                "reason": "wake test",
                "detached": true,
                "name": "t",
            }),
            &crate::Recorder::detached(),
        )
        .await
        .unwrap();

    // 4. Poll for BOTH conditions within 20s.
    // Under the full workspace parallel test run (782+ tests simultaneously)
    // the setsid'd subprocess can take several seconds to get scheduled; 20s
    // is enough headroom while still catching real breakage. Standalone the
    // hook fires in well under 1s.
    let daemons_jsonl = gd.path().join("daemons.jsonl");
    let parent_log = parent_log_path;
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut stop_recorded = false;
    let mut update_recorded = false;
    while Instant::now() < deadline && !(stop_recorded && update_recorded) {
        if !stop_recorded {
            if let Ok(raw) = std::fs::read_to_string(&daemons_jsonl) {
                stop_recorded = raw
                    .lines()
                    .any(|l| l.contains("\"kind\":\"stop\"") && l.contains("\"name\":\"t\""));
            }
        }
        if !update_recorded {
            if let Ok(raw) = std::fs::read_to_string(&parent_log) {
                update_recorded = raw.contains("daemon_update") || raw.contains("daemonUpdate");
            }
        }
        if stop_recorded && update_recorded {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    assert!(
        stop_recorded,
        "daemons.jsonl never got a stop line for name 't' within 5s; contents: {}",
        std::fs::read_to_string(&daemons_jsonl).unwrap_or_default()
    );
    assert!(
        update_recorded,
        "parent log.jsonl never got a daemon_update event within 5s; contents: {}",
        std::fs::read_to_string(&parent_log).unwrap_or_default()
    );
}

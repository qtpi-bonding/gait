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

fn full_output_path(result: &serde_json::Value) -> std::path::PathBuf {
    let output = result
        .get("output")
        .and_then(|v| v.as_str())
        .expect("output field");
    let marker = "[full output: ";
    let rest = output
        .split_once(marker)
        .unwrap_or_else(|| panic!("missing full-output note: {output}"))
        .1;
    let path = rest
        .split_once(" (")
        .unwrap_or_else(|| panic!("malformed full-output note: {output}"))
        .0;
    std::path::PathBuf::from(path)
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
async fn full_output_file_is_session_scoped_and_survives_tool_return() {
    let dir = TempDir::new().unwrap();
    let tool = make_tool(&dir);
    let result = tool
        .execute(
            serde_json::json!({
                "cmd": "printf 'first-line\\nsecond-line\\n'",
                "reason": "output lifecycle test"
            }),
            &crate::Recorder::detached(),
        )
        .await
        .unwrap();

    let path = full_output_path(&result);
    let expected_root = crate::session::live::outputs_dir(dir.path(), "test").join("shell");
    assert!(
        path.starts_with(&expected_root),
        "full output must be owned by the active session: {}",
        path.display()
    );
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "first-line\nsecond-line\n",
        "reported full-output path must remain readable after execute returns"
    );
}

#[tokio::test]
async fn full_output_retains_content_omitted_from_the_bounded_tail() {
    let dir = TempDir::new().unwrap();
    let tool = make_tool(&dir);
    let result = tool
        .execute(
            serde_json::json!({
                "cmd": "printf 'only-in-full\\n'; yes x | head -c 70000",
                "reason": "large output lifecycle test"
            }),
            &crate::Recorder::detached(),
        )
        .await
        .unwrap();

    let displayed = result.get("output").and_then(|v| v.as_str()).unwrap();
    assert!(
        !displayed.starts_with("only-in-full"),
        "the old prefix should be outside the bounded tail: {displayed}"
    );
    let full = std::fs::read_to_string(full_output_path(&result)).unwrap();
    assert!(
        full.starts_with("only-in-full\n"),
        "the complete output file must retain content omitted from the tail"
    );
    assert!(full.len() > 64 * 1024);
}

#[tokio::test]
async fn full_output_files_are_isolated_by_session() {
    let dir = TempDir::new().unwrap();
    let tool_a = ShellTool::new(ToolCtx::new(
        dir.path().to_path_buf(),
        dir.path().to_path_buf(),
        "session-a".into(),
    ));
    let tool_b = ShellTool::new(ToolCtx::new(
        dir.path().to_path_buf(),
        dir.path().to_path_buf(),
        "session-b".into(),
    ));

    let result_a = tool_a
        .execute(
            serde_json::json!({"cmd": "printf a", "reason": "isolation a"}),
            &crate::Recorder::detached(),
        )
        .await
        .unwrap();
    let result_b = tool_b
        .execute(
            serde_json::json!({"cmd": "printf b", "reason": "isolation b"}),
            &crate::Recorder::detached(),
        )
        .await
        .unwrap();

    let path_a = full_output_path(&result_a);
    let path_b = full_output_path(&result_b);
    assert!(path_a.starts_with(crate::session::live::outputs_dir(dir.path(), "session-a")));
    assert!(path_b.starts_with(crate::session::live::outputs_dir(dir.path(), "session-b")));
    assert_ne!(path_a, path_b);
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

/// Regression test for the bug reproduced in dogfood session `sc4f35bf6`: a
/// model reasonably re-sends the exact same relative `cwd` it used before,
/// expecting it to always mean "this subdir of the codebase". Before this fix,
/// a prior call's resolved directory got folded into "current" state and a
/// later relative `cwd` request was joined against THAT instead of the
/// codebase root — turning the second, identical request into a nonexistent
/// doubled path (".../sub/sub") and failing with "cd: no such directory".
/// `cwd` must be per-call and idempotent: repeating it always resolves the
/// same way, and omitting it always means the codebase root, regardless of
/// what any earlier call passed.
#[tokio::test]
async fn cwd_param_is_per_call_not_sticky() {
    let dir = TempDir::new().unwrap();
    std::fs::create_dir_all(dir.path().join("sub")).unwrap();
    let tool = make_tool(&dir);

    let run = |cmd: &'static str, cwd: Option<&'static str>| {
        let tool = &tool;
        async move {
            let mut args = serde_json::json!({"cmd": cmd, "reason": "test"});
            if let Some(cwd) = cwd {
                args["cwd"] = serde_json::Value::String(cwd.to_string());
            }
            let result = tool
                .execute(args, &crate::Recorder::detached())
                .await
                .unwrap();
            result
                .get("output")
                .and_then(|v| v.as_str())
                .expect("output field")
                .to_string()
        }
    };

    // Call 1: cwd: "sub" resolves from the codebase root.
    let out1 = run("pwd", Some("sub")).await;
    assert!(out1.contains("sub"), "expected to run in sub, got: {out1}");
    assert!(!out1.contains("no such directory"), "got: {out1}");

    // Call 2: the exact same relative request again — must succeed identically,
    // not compound against call 1's resolved directory.
    let out2 = run("pwd", Some("sub")).await;
    assert!(
        out2.contains("sub"),
        "expected to run in sub again, got: {out2}"
    );
    assert!(!out2.contains("no such directory"), "got: {out2}");

    // Call 3: omits `cwd` entirely — must run at the codebase root, not
    // wherever calls 1/2 happened to land.
    let out3 = run("pwd", None).await;
    assert!(
        !out3.contains("/sub"),
        "a call omitting cwd must run at the codebase root, not a prior call's dir: {out3}"
    );
}

/// A stale `__cwd` key (the old fold-injection channel) must have zero effect —
/// there is nothing left that reads it. Guards against silently reintroducing
/// the sticky-cwd channel through the back door.
#[tokio::test]
async fn stale_dunder_cwd_key_is_ignored() {
    let dir = TempDir::new().unwrap();
    std::fs::create_dir_all(dir.path().join("sub")).unwrap();
    let tool = make_tool(&dir);
    let bogus = dir.path().join("sub").to_string_lossy().into_owned();
    let result = tool
        .execute(
            serde_json::json!({"cmd": "pwd", "reason": "test", "__cwd": bogus}),
            &crate::Recorder::detached(),
        )
        .await
        .unwrap();
    let output = result
        .get("output")
        .and_then(|v| v.as_str())
        .expect("output field");
    assert!(
        !output.contains("/sub"),
        "a stale __cwd key must not steer where the command runs: {output}"
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
        read_deny: Default::default(),
        write_deny: Default::default(),
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

    // No Cwd event may reach the transcript for an out-of-scope request — it's
    // a plain historical log record, but there's no reason to record a
    // directory the command never actually ran in.
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
        read_deny: Default::default(),
        write_deny: Default::default(),
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
    let output_dir = crate::session::live::outputs_dir(dir.path(), "test").join("shell");
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
    let early_marker = format!("{sentinel}_early");
    let mut contents = String::new();
    if let Ok(entries) = std::fs::read_dir(&output_dir) {
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
async fn writes_output_to_session_file() {
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
    // Result's `output` field references the session file (file note is part of display).
    let output = result
        .get("output")
        .and_then(|v| v.as_str())
        .expect("output field");
    assert!(output.contains("full output:"), "got: {output}");
    assert!(output.contains("shell-"), "got: {output}");
    let path = full_output_path(&result);
    let expected = crate::session::live::outputs_dir(dir.path(), "test").join("shell");
    assert!(
        path.starts_with(&expected),
        "expected session output path, got: {}",
        path.display()
    );
    assert_eq!(std::fs::read_to_string(path).unwrap(), "payload\n");
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

    // log_path must live under daemons/<session_id>/ — namespaced per session
    // so one session can never see (or accidentally read) another session's
    // daemon logs, even when both share a gait_dir.
    let log_path = proto
        .get("log_path")
        .and_then(|v| v.as_str())
        .expect("log_path");
    assert!(
        log_path.contains("daemons/test/"),
        "log_path should be under daemons/<session_id>/ (session is \"test\"), got: {log_path}"
    );

    // started_at must be non-zero.
    let started_at = proto
        .get("started_at")
        .and_then(|v| v.as_u64())
        .expect("started_at");
    assert!(started_at > 0, "started_at must be non-zero");
}

/// Two sessions sharing a gait_dir must never land daemon logs in the same
/// place — this is what makes the sandbox's per-session grant meaningful. A
/// later session inspecting its own daemon dir must not stumble on an
/// unrelated session's stale daemon logs (the failure mode that burned ~35
/// steps of an agent's step budget re-reading other sessions' task-1 logs).
#[tokio::test]
async fn detached_daemon_logs_are_isolated_per_session() {
    let dir = TempDir::new().unwrap();
    let tool_a = ShellTool::new(ToolCtx::new(
        dir.path().to_path_buf(),
        dir.path().to_path_buf(),
        "session-a".into(),
    ));
    let tool_b = ShellTool::new(ToolCtx::new(
        dir.path().to_path_buf(),
        dir.path().to_path_buf(),
        "session-b".into(),
    ));

    let result_a = tool_a
        .execute(
            serde_json::json!({
                "cmd": "sleep 30",
                "reason": "isolation test a",
                "detached": true,
                "name": "same-name"
            }),
            &crate::Recorder::detached(),
        )
        .await
        .unwrap();
    let result_b = tool_b
        .execute(
            serde_json::json!({
                "cmd": "sleep 30",
                "reason": "isolation test b",
                "detached": true,
                "name": "same-name"
            }),
            &crate::Recorder::detached(),
        )
        .await
        .unwrap();

    let proto_a = daemon_proto(&result_a);
    let proto_b = daemon_proto(&result_b);
    let pid_a = proto_a.get("pid").and_then(|v| v.as_i64()).expect("pid") as i32;
    let pid_b = proto_b.get("pid").and_then(|v| v.as_i64()).expect("pid") as i32;
    unsafe {
        libc::kill(pid_a, libc::SIGKILL);
        libc::kill(pid_b, libc::SIGKILL);
    }

    let log_path_a = proto_a
        .get("log_path")
        .and_then(|v| v.as_str())
        .expect("log_path");
    let log_path_b = proto_b
        .get("log_path")
        .and_then(|v| v.as_str())
        .expect("log_path");
    assert_ne!(
        log_path_a, log_path_b,
        "same daemon name from different sessions must not collide"
    );
    assert!(log_path_a.contains("daemons/session-a/"));
    assert!(log_path_b.contains("daemons/session-b/"));
}

/// Every shell result must echo the effective cwd, so the model can never
/// silently lose track of it — the root cause of a real stall where an agent
/// alternated between assuming cwd was the repo root and assuming it was
/// already inside a subdirectory, with no feedback to tell it which was true.
#[tokio::test]
async fn foreground_result_echoes_effective_cwd() {
    let dir = TempDir::new().unwrap();
    let tool = make_tool(&dir);
    let result = tool
        .execute(
            serde_json::json!({
                "cmd": "echo hi",
                "reason": "cwd echo test"
            }),
            &crate::Recorder::detached(),
        )
        .await
        .unwrap();
    let output = result.get("output").and_then(|v| v.as_str()).unwrap();
    let expected = format!("[cwd: {}]", dir.path().display());
    assert!(
        output.contains(&expected),
        "expected output to echo cwd {expected:?}, got: {output}"
    );
}

/// The cwd echo must reflect an explicit `cwd` param too, not just the
/// default — this is exactly the case that mattered in the real stall
/// (mixing an explicit-`cwd` call with later calls that assumed a different
/// default).
#[tokio::test]
async fn foreground_result_echoes_explicit_cwd_param() {
    let dir = TempDir::new().unwrap();
    let sub = dir.path().join("subdir");
    std::fs::create_dir_all(&sub).unwrap();
    let tool = make_tool(&dir);
    let result = tool
        .execute(
            serde_json::json!({
                "cmd": "echo hi",
                "reason": "cwd echo test",
                "cwd": "subdir"
            }),
            &crate::Recorder::detached(),
        )
        .await
        .unwrap();
    let output = result.get("output").and_then(|v| v.as_str()).unwrap();
    let expected = format!("[cwd: {}]", sub.canonicalize().unwrap().display());
    assert!(
        output.contains(&expected),
        "expected output to echo cwd {expected:?}, got: {output}"
    );
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

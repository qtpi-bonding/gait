use std::process::{Command, Stdio};

fn gait() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_gait"));
    cmd.stdin(Stdio::null()).env_remove("OTEL_EXPORTER_OTLP_ENDPOINT");
    cmd
}

#[test]
fn broken_config_exits_65() {
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::write(dir.path().join("gait.jsonc"), "{ \"inference\": ").unwrap();
    let status = gait()
        .args(["-e", "list files", "--gait-dir"])
        .arg(dir.path())
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(65));
}

#[test]
fn unknown_flag_exits_65() {
    let status = gait().arg("--no-such-flag").status().unwrap();
    assert_eq!(status.code(), Some(65));
}

#[test]
fn help_exits_0() {
    let status = gait().arg("--help").status().unwrap();
    assert_eq!(status.code(), Some(0));
}

#[test]
fn missing_gait_dir_flag_exits_65() {
    let cwd = tempfile::TempDir::new().unwrap();
    let home = tempfile::TempDir::new().unwrap();
    let output = gait()
        .arg("sessions")
        .current_dir(cwd.path())
        .env("HOME", home.path())
        .env_remove("GAIT_DIR")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(65));
    assert!(String::from_utf8_lossy(&output.stderr).contains("--gait-dir"));
}

#[test]
fn missing_session_exits_65() {
    let workspace = tempfile::TempDir::new().unwrap();
    let init = gait().arg("init").arg(workspace.path()).status().unwrap();
    assert!(init.success());
    let status = gait()
        .args(["allow", "nosuch", "--gait-dir"])
        .arg(workspace.path())
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(65));
}

/// Every get/act/delete on an object that does not exist is a logical
/// not-found (exit 65, "not found" on stderr), never success.
fn assert_not_found(args: &[&str]) {
    let workspace = tempfile::TempDir::new().unwrap();
    let init = gait().arg("init").arg(workspace.path()).status().unwrap();
    assert!(init.success());
    let output = gait()
        .arg(args[0])
        .arg("--gait-dir")
        .arg(workspace.path())
        .args(&args[1..])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(65), "{args:?}: stderr: {stderr}");
    assert!(stderr.contains("not found"), "{args:?}: stderr: {stderr}");
}

#[test]
fn stats_unknown_session_is_not_found() {
    assert_not_found(&["stats", "nosuch"]);
}

#[test]
fn log_unknown_session_is_not_found() {
    assert_not_found(&["log", "nosuch"]);
}

#[test]
fn undo_list_unknown_session_is_not_found() {
    assert_not_found(&["undo", "nosuch", "--list"]);
}

#[test]
fn discard_unknown_session_is_not_found() {
    assert_not_found(&["discard", "nosuch", "--yes"]);
}

#[test]
fn persist_unknown_session_is_not_found() {
    assert_not_found(&["persist", "nosuch", "--yes"]);
}

#[test]
fn unregister_unknown_mcp_is_not_found() {
    assert_not_found(&["unregister", "mcp", "nosuch"]);
}

#[test]
fn config_get_unknown_key_is_not_found() {
    assert_not_found(&["config", "get", "nosuch"]);
}

#[test]
fn inference_show_unknown_is_not_found() {
    assert_not_found(&["inference", "show", "nosuch"]);
}

#[test]
fn mode_show_unknown_is_not_found() {
    assert_not_found(&["mode", "show", "nosuch"]);
}

#[test]
fn daemons_stop_unknown_is_not_found() {
    assert_not_found(&["daemons", "stop", "nosuch"]);
}

#[test]
fn reply_unknown_session_is_not_found() {
    assert_not_found(&["reply", "nosuch", "hi"]);
}

#[test]
fn load_skill_into_unknown_session_is_not_found() {
    assert_not_found(&["load", "skill", "x", "nosuch"]);
}

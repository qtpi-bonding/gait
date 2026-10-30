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
fn no_workspace_exits_65() {
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
    assert!(String::from_utf8_lossy(&output.stderr).contains("not a gait workspace"));
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

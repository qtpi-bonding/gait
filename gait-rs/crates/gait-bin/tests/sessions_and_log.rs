fn init_workspace(ws: &std::path::Path) {
    let exe = env!("CARGO_BIN_EXE_gait");
    let out = std::process::Command::new(exe)
        .arg("init")
        .arg(ws)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "gait init failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn sessions_last_flag_is_accepted_and_empty_workspace_returns_no_active_sessions() {
    let ws = tempfile::TempDir::new().unwrap();
    init_workspace(ws.path());
    let exe = env!("CARGO_BIN_EXE_gait");

    let out = std::process::Command::new(exe)
        .arg("sessions")
        .arg("--gait-dir")
        .arg(ws.path())
        .args(["--last", "3"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let value: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).unwrap();
    assert_eq!(value, serde_json::json!({"active": []}));
}

#[test]
fn log_on_an_unknown_session_is_not_found() {
    let ws = tempfile::TempDir::new().unwrap();
    init_workspace(ws.path());
    let exe = env!("CARGO_BIN_EXE_gait");

    let out = std::process::Command::new(exe)
        .arg("log")
        .arg("never-existed")
        .arg("--gait-dir")
        .arg(ws.path())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(65));
    assert!(String::from_utf8_lossy(&out.stderr).contains("not found"));
}

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
fn log_on_a_session_with_no_completed_run_returns_an_empty_summary() {
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
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let value: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).unwrap();
    assert_eq!(
        value,
        serde_json::json!({
            "turn": null,
            "message": "",
            "checkpoint_short": null,
            "file_count": 0,
            "added_lines": null,
            "removed_lines": null,
            "tools": []
        })
    );
}

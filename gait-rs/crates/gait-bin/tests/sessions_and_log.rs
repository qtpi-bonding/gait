fn init_workspace(ws: &std::path::Path) {
    let exe = env!("CARGO_BIN_EXE_gait");
    let out = std::process::Command::new(exe)
        .arg("init")
        .arg("--gait-dir")
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
    assert_eq!(value, serde_json::json!({"items": []}));
}

#[test]
fn log_on_an_unknown_session_is_not_found() {
    let ws = tempfile::TempDir::new().unwrap();
    init_workspace(ws.path());
    let exe = env!("CARGO_BIN_EXE_gait");

    let out = std::process::Command::new(exe)
        .arg("log")
        .args(["--id", "never-existed"])
        .arg("--gait-dir")
        .arg(ws.path())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(65));
    assert!(String::from_utf8_lossy(&out.stderr).contains("not found"));
}

#[test]
fn log_without_an_id_is_a_usage_error() {
    let ws = tempfile::TempDir::new().unwrap();
    init_workspace(ws.path());
    let exe = env!("CARGO_BIN_EXE_gait");

    let out = std::process::Command::new(exe)
        .arg("log")
        .arg("--gait-dir")
        .arg(ws.path())
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn the_old_positional_session_id_is_gone() {
    let ws = tempfile::TempDir::new().unwrap();
    init_workspace(ws.path());
    let exe = env!("CARGO_BIN_EXE_gait");

    let out = std::process::Command::new(exe)
        .args(["log", "never-existed", "--gait-dir"])
        .arg(ws.path())
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn ps_on_an_empty_workspace_has_items() {
    let ws = tempfile::TempDir::new().unwrap();
    init_workspace(ws.path());
    let exe = env!("CARGO_BIN_EXE_gait");

    let out = std::process::Command::new(exe)
        .arg("ps")
        .arg("--gait-dir")
        .arg(ws.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let value: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).unwrap();
    assert_eq!(value, serde_json::json!({"items": []}));
}

#[test]
fn help_lists_the_session_verbs_and_not_stop() {
    let exe = env!("CARGO_BIN_EXE_gait");
    let out = std::process::Command::new(exe)
        .arg("--help")
        .output()
        .unwrap();
    let help = String::from_utf8_lossy(&out.stdout);
    for verb in [
        "sessions", "ps", "stats", "log", "undo", "persist", "discard", "allow", "deny", "unlock",
        "reply",
    ] {
        assert!(
            help.contains(&format!("\n  {verb} ")),
            "help lacks {verb}: {help}"
        );
    }
    // `stop` was a stub that always failed; it is deleted, not ported.
    assert!(!help.contains("\n  stop "), "help still lists stop: {help}");
}

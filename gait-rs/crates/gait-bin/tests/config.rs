#[test]
fn config_set_then_get_round_trips_via_binary() {
    let ws = tempfile::TempDir::new().unwrap();
    let exe = env!("CARGO_BIN_EXE_gait");

    let set_out = std::process::Command::new(exe)
        .arg("config")
        .arg("--gait-dir")
        .arg(ws.path())
        .args(["set", "model", "anthropic/claude-opus-4"])
        .output()
        .unwrap();
    assert!(
        set_out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&set_out.stderr)
    );

    let get_out = std::process::Command::new(exe)
        .arg("config")
        .arg("--gait-dir")
        .arg(ws.path())
        .args(["get", "model"])
        .output()
        .unwrap();
    assert!(get_out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&get_out.stdout).trim(),
        "\"anthropic/claude-opus-4\""
    );
}

#[test]
fn config_set_max_iter_stores_a_number_not_a_string() {
    let ws = tempfile::TempDir::new().unwrap();
    let exe = env!("CARGO_BIN_EXE_gait");
    std::process::Command::new(exe)
        .arg("config")
        .arg("--gait-dir")
        .arg(ws.path())
        .args(["set", "max_iter", "50"])
        .output()
        .unwrap();
    let raw = std::fs::read_to_string(ws.path().join("gait.jsonc")).unwrap();
    let val: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(val["max_iter"], 50);
}

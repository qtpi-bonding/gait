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

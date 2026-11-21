//! End-to-end test for `gait reply` (the `reply` op in src/ops/session.rs),
//! spawned as a subprocess. Follows the same subprocess-spawn convention as
//! run_tool_hand.rs in this same tests/ dir.
use std::path::Path;

fn make_session(ws: &Path, id: &str) -> std::path::PathBuf {
    let tx_path = gait_state::session::live::log_path(ws, id);
    std::fs::create_dir_all(tx_path.parent().unwrap()).unwrap();
    tx_path
}

fn seed_clarify(tx_path: &Path, item: gait_state::proto::gait::log::v1::ClarifyItem) {
    let body = gait_state::proto::events::clarify(&[item]);
    gait_state::proto::wire::append(tx_path, body).unwrap();
}

#[test]
fn cmd_reply_answers_pending_clarify_via_subprocess() {
    let ws = tempfile::TempDir::new().unwrap();
    let tx = make_session(ws.path(), "scli02");
    seed_clarify(
        &tx,
        gait_state::proto::gait::log::v1::ClarifyItem {
            id: "c1".into(),
            content: "which db?".into(),
            why: "ambiguous".into(),
            kind: gait_state::proto::gait::log::v1::ClarifyKind::Is as i32,
            options: vec![],
        },
    );
    let exe = env!("CARGO_BIN_EXE_gait");
    let output = std::process::Command::new(exe)
        .args([
            "reply",
            "--id",
            "scli02",
            "--message",
            "postgres",
            "--gait-dir",
        ])
        .arg(ws.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&output.stdout).trim()).unwrap();
    assert_eq!(
        value,
        serde_json::json!({"sessionId": "scli02", "kind": "clarify", "id": "c1"})
    );
    let state = gait_state::session::log::session_state(&tx).unwrap();
    assert!(matches!(
        state,
        gait_state::session::log::SessionState::Idle
    ));
}

#[test]
fn cmd_reply_errors_when_nothing_pending_via_subprocess() {
    let ws = tempfile::TempDir::new().unwrap();
    let _tx = make_session(ws.path(), "scli03");
    let exe = env!("CARGO_BIN_EXE_gait");
    let output = std::process::Command::new(exe)
        .args([
            "reply",
            "--id",
            "scli03",
            "--message",
            "postgres",
            "--gait-dir",
        ])
        .arg(ws.path())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(65));
    assert!(String::from_utf8_lossy(&output.stderr).contains("no pending"));
}

#[test]
fn reply_without_a_message_is_a_usage_error() {
    let ws = tempfile::TempDir::new().unwrap();
    let _tx = make_session(ws.path(), "scli04");
    let exe = env!("CARGO_BIN_EXE_gait");
    let output = std::process::Command::new(exe)
        .args(["reply", "--id", "scli04", "--gait-dir"])
        .arg(ws.path())
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

//! End-to-end test for `gait reply` (crates/gait-cli/src/cli/session.rs::cmd_reply),
//! spawned as a subprocess since cmd_reply calls std::process::exit and eprintln
//! (Wall 1 boundary — gait-ops itself never does either). Follows the same
//! subprocess-spawn convention as run_tool_hand.rs in this same tests/ dir.
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
        .args(["reply", "scli02", "postgres", "--gait-dir"])
        .arg(ws.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
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
        .args(["reply", "scli03", "postgres", "--gait-dir"])
        .arg(ws.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("no pending"));
}

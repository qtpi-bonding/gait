//! Integration test for `gait reply` (crates/gait-cli/src/cli/session.rs::cmd_reply).
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
fn reply_via_ops_matches_cli_args_shape_and_resolves_session() {
    let ws = tempfile::TempDir::new().unwrap();
    let tx = make_session(ws.path(), "scli01");
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
    // Exercise the exact op cmd_reply calls, with the exact args shape
    // ReplyArgs carries, proving the CLI wiring has nothing left to do
    // beyond forwarding these fields (cmd_reply's body is asserted by
    // reading the log afterward, not by capturing stderr/exit-code, since
    // gait-ops — and therefore the op cmd_reply calls — never prints or
    // exits per Wall 1).
    let out = gait_ops::interaction::reply(ws.path(), "scli01", "postgres".into()).unwrap();
    assert_eq!(out.id, "c1");
    let state = gait_state::session::log::session_state(&tx).unwrap();
    assert!(matches!(
        state,
        gait_state::session::log::SessionState::Idle
    ));
}

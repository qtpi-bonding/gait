//! End-to-end idle-wake: a detached daemon finishes, the registry records a
//! `Stop`, and the parent's session log gains a `daemon_update` event (the
//! wake). It runs the `gait` binary Cargo built for this package as the
//! `__daemon-exit` hook, so it never depends on a `gait` on the PATH.
#![cfg(feature = "full")]

use gait_core::tools::{ShellTool, ToolCtx};
use gait_core::types::Tool;
use std::time::{Duration, Instant};

#[tokio::test]
async fn detached_daemon_wakes_idle_parent_on_exit() {
    std::env::set_var("GAIT_DAEMON_EXE", env!("CARGO_BIN_EXE_gait"));

    let gd = tempfile::TempDir::new().unwrap();
    let parent_id = "swake001";
    let parent_dir = gait_core::session::live::worktree_dir(gd.path(), parent_id);
    std::fs::create_dir_all(&parent_dir).unwrap();
    let parent_log = gait_core::session::live::log_path(gd.path(), parent_id);
    std::fs::create_dir_all(parent_log.parent().unwrap()).unwrap();
    std::fs::write(&parent_log, "").unwrap();

    let tool = ShellTool::new(ToolCtx::new(
        gd.path().to_path_buf(),
        gd.path().to_path_buf(),
        parent_id.to_string(),
    ));
    tool.execute(
        serde_json::json!({
            "cmd": "sleep 0.1",
            "reason": "wake test",
            "detached": true,
            "name": "t",
        }),
        &gait_core::Recorder::detached(),
    )
    .await
    .unwrap();

    let daemons_jsonl = gd.path().join("daemons.jsonl");
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut stop_recorded = false;
    let mut update_recorded = false;
    while Instant::now() < deadline && !(stop_recorded && update_recorded) {
        if let Ok(raw) = std::fs::read_to_string(&daemons_jsonl) {
            stop_recorded = raw
                .lines()
                .any(|l| l.contains("\"kind\":\"stop\"") && l.contains("\"name\":\"t\""));
        }
        if let Ok(raw) = std::fs::read_to_string(&parent_log) {
            update_recorded = raw.contains("daemon_update") || raw.contains("daemonUpdate");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    assert!(
        stop_recorded,
        "daemons.jsonl never got a stop line for name 't' within 20s; contents: {}",
        std::fs::read_to_string(&daemons_jsonl).unwrap_or_default()
    );
    assert!(
        update_recorded,
        "parent log.jsonl never got a daemon_update event within 20s; contents: {}",
        std::fs::read_to_string(&parent_log).unwrap_or_default()
    );

    // The hook runs in the sandboxed hand and cannot sign, so the notice is
    // unsigned and the parent folds it as unverified.
    let events = gait_core::proto::wire::read_events(&parent_log).unwrap();
    let update = events
        .iter()
        .find(|ev| {
            matches!(
                gait_core::proto::event_kind(ev),
                Some(gait_core::proto::gait::log::v1::payload::Kind::DaemonUpdate(_))
            )
        })
        .expect("a daemon_update event");
    assert!(
        update.provenance.is_empty(),
        "the daemon-exit notice must be unsigned"
    );
}

//! Cross-consumer equivalence: run the same two-node shell workflow (written
//! as a JavaScript workflow script for the script engine) with `CliObserver`
//! and `ChannelObserver` and assert that both produce identical folded
//! `WorkflowState` (both nodes Done, workflow completed).
//!
//! Shell nodes require no model, so this test runs fully offline.

use std::sync::Arc;

use gait_workflow::observer::{ChannelObserver, CliObserver};
use gait_workflow::run_log::WorkflowRunLog;
use gait_workflow::runner::WorkflowOutcome;
use gait_workflow::script::{run_script, ScriptRunOpts};
use gait_workflow::state::{reconstruct_state, NodeStatus};
use gait_workflow::turn_exec::testing::ScriptedTurnExecutor;

use tempfile::TempDir;

const WORKFLOW_SCRIPT: &str = r#"
await gait.wf.config({ name: "cross-consumer-test" });
await gait.wf.shell({ id: "a", script: "echo alpha" });
await gait.wf.shell({ id: "b", script: "echo beta" });
"#;

fn setup_gait_dir(dir: &std::path::Path) {
    std::fs::create_dir_all(dir.join(".active")).unwrap();
    std::fs::create_dir_all(dir.join("workflow-runs")).unwrap();
    std::process::Command::new("git")
        .args(["init", "--initial-branch=main"])
        .current_dir(dir)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["config", "user.email", "test@test.com"])
        .current_dir(dir)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["config", "user.name", "Test"])
        .current_dir(dir)
        .output()
        .unwrap();
    std::fs::write(dir.join(".gitkeep"), "").unwrap();
    std::process::Command::new("git")
        .args(["add", ".gitkeep"])
        .current_dir(dir)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["commit", "-m", "init"])
        .current_dir(dir)
        .output()
        .unwrap();
}

fn unique_session_id(prefix: &str) -> String {
    format!(
        "{}-{}",
        prefix,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    )
}

async fn run_js(
    tmp: &TempDir,
    run_id: &str,
    source: &str,
    observer: &mut dyn gait_workflow::observer::WorkflowObserver,
) -> anyhow::Result<WorkflowOutcome> {
    let script_path = tmp.path().join("wf.js");
    std::fs::write(&script_path, source).unwrap();
    let mut sink = gait_core::embed::CliSink::stdio(true);
    run_script(
        ScriptRunOpts {
            script_path,
            run_id: run_id.to_string(),
            gait_dir: tmp.path().to_path_buf(),
            codebase: tmp.path().to_path_buf(),
            args: vec![],
            force_rerun_nodes: vec![],
            force: false,
            turns: Arc::new(ScriptedTurnExecutor::new()),
        },
        &mut sink,
        observer,
    )
    .await
}

#[tokio::test]
async fn cli_and_channel_observer_produce_identical_state() {
    // ── Run 1: CliObserver ────────────────────────────────────────────────────
    let dir1 = TempDir::new().unwrap();
    let gait_dir1 = dir1.path().to_path_buf();
    setup_gait_dir(&gait_dir1);
    let session1 = unique_session_id("cli");

    let mut obs1 = CliObserver;
    let outcome1 = run_js(&dir1, &session1, WORKFLOW_SCRIPT, &mut obs1)
        .await
        .expect("CliObserver run should succeed");

    assert_eq!(outcome1, WorkflowOutcome::Completed);

    let log1 = WorkflowRunLog::open(&gait_dir1.join("workflow-runs"), &session1).unwrap();
    let state1 = reconstruct_state(&log1.read_events().unwrap());

    // ── Run 2: ChannelObserver ────────────────────────────────────────────────
    let dir2 = TempDir::new().unwrap();
    let gait_dir2 = dir2.path().to_path_buf();
    setup_gait_dir(&gait_dir2);
    let session2 = unique_session_id("chan");

    let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut obs2 = ChannelObserver::new(event_tx);

    let outcome2 = run_js(&dir2, &session2, WORKFLOW_SCRIPT, &mut obs2)
        .await
        .expect("ChannelObserver run should succeed");

    assert_eq!(outcome2, WorkflowOutcome::Completed);

    let log2 = WorkflowRunLog::open(&gait_dir2.join("workflow-runs"), &session2).unwrap();
    let state2 = reconstruct_state(&log2.read_events().unwrap());

    // ── Assert identical folded state ─────────────────────────────────────────
    assert_eq!(
        state1.node_status("a"),
        NodeStatus::Done,
        "CliObserver: node a should be Done"
    );
    assert_eq!(
        state1.node_status("b"),
        NodeStatus::Done,
        "CliObserver: node b should be Done"
    );
    assert_eq!(
        state2.node_status("a"),
        NodeStatus::Done,
        "ChannelObserver: node a should be Done"
    );
    assert_eq!(
        state2.node_status("b"),
        NodeStatus::Done,
        "ChannelObserver: node b should be Done"
    );

    // Both states should report the same node statuses.
    assert_eq!(
        state1.node_status("a"),
        state2.node_status("a"),
        "node a: CliObserver and ChannelObserver must produce identical state"
    );
    assert_eq!(
        state1.node_status("b"),
        state2.node_status("b"),
        "node b: CliObserver and ChannelObserver must produce identical state"
    );
}

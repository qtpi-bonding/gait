//! Cross-consumer equivalence: run the same two-node shell workflow with
//! `CliObserver` and `ChannelObserver` and assert that both produce identical
//! folded `WorkflowState` (both nodes Done, workflow completed).
//!
//! Shell nodes require no model, so this test runs fully offline.

use gait_workflow::convert::to_proto;
use gait_workflow::observer::{ChannelObserver, CliObserver};
use gait_workflow::run_log::WorkflowRunLog;
use gait_workflow::runner::{run_workflow, WorkflowOutcome, WorkflowRunOpts};
use gait_workflow::schema::WorkflowDef;
use gait_workflow::state::{reconstruct_state, NodeStatus};

use tempfile::TempDir;

const WORKFLOW_YAML: &str = r#"
name: cross-consumer-test
nodes:
  - id: a
    shell: "echo alpha"
  - id: b
    depends_on: [a]
    shell: "echo beta"
"#;

fn setup_gait_dir(dir: &std::path::Path) {
    std::fs::create_dir_all(dir.join("sessions")).unwrap();
    std::fs::create_dir_all(dir.join("workflow-runs")).unwrap();
    std::process::Command::new("git")
        .args(["init"])
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

#[tokio::test]
async fn cli_and_channel_observer_produce_identical_state() {
    let wf: WorkflowDef = serde_yaml::from_str(WORKFLOW_YAML).unwrap();

    // ── Run 1: CliObserver ────────────────────────────────────────────────────
    let dir1 = TempDir::new().unwrap();
    let gait_dir1 = dir1.path().to_path_buf();
    setup_gait_dir(&gait_dir1);
    let session1 = unique_session_id("cli");

    let mut sink1 = gait_core::embed::CliSink::stdio(true);
    let outcome1 = run_workflow(
        WorkflowRunOpts {
            workflow_def: to_proto(&wf),
            yaml_path: "cross-consumer-test.yaml".to_string(),
            run_id: session1.clone(),
            gait_dir: gait_dir1.clone(),
            codebase: gait_dir1.clone(),
            args: vec![],
        },
        &mut sink1,
        &mut CliObserver,
        None,
    )
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

    let mut sink2 = gait_core::embed::CliSink::stdio(true);
    let outcome2 = run_workflow(
        WorkflowRunOpts {
            workflow_def: to_proto(&wf),
            yaml_path: "cross-consumer-test.yaml".to_string(),
            run_id: session2.clone(),
            gait_dir: gait_dir2.clone(),
            codebase: gait_dir2.clone(),
            args: vec![],
        },
        &mut sink2,
        &mut obs2,
        None,
    )
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

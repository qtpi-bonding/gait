//! Observer parity test: run a two-shell-node workflow with a `ChannelObserver`
//! and assert that:
//! 1. The run-log (folded via `reconstruct_state`) reports both nodes as Done.
//! 2. The event channel received `WorkflowStart` and `WorkflowComplete` events.
//!
//! Shell nodes require no model, so this test runs fully offline.

use gait_workflow::observer::ChannelObserver;
use gait_workflow::run_log::WorkflowRunLog;
use gait_workflow::runner::{run_workflow, WorkflowOutcome, WorkflowRunOpts};
use gait_workflow::schema::WorkflowDef;
use gait_workflow::state::{reconstruct_state, NodeStatus};

use gait_core::proto::gait::workflow::v1::workflow_event;

use tempfile::TempDir;

/// Minimal two-node sequential shell workflow.
const WORKFLOW_YAML: &str = r#"
name: parity-test
nodes:
  - id: step1
    shell: "echo hello"
  - id: step2
    depends_on: [step1]
    shell: "echo world"
"#;

#[tokio::test]
async fn observer_parity_shell_workflow() {
    let dir = TempDir::new().unwrap();
    let gait_dir = dir.path().to_path_buf();

    // Minimal gait-dir structure
    std::fs::create_dir_all(gait_dir.join("sessions")).unwrap();
    std::fs::create_dir_all(gait_dir.join("workflow-runs")).unwrap();
    // git repo needed by Repos::production() / commit_session
    std::process::Command::new("git")
        .args(["init", "--bare", "traces.git"])
        .current_dir(&gait_dir)
        .output()
        .unwrap();
    // We need a proper traces repo that gait_core can use.
    // Instead, we override with a non-git path and rely on the fact that
    // commit_session falls back gracefully; however gait_core::git::Repos::production()
    // uses the actual filesystem. Let's init a real git repo for the session.
    std::process::Command::new("git")
        .args(["init"])
        .current_dir(&gait_dir)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["config", "user.email", "test@test.com"])
        .current_dir(&gait_dir)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["config", "user.name", "Test"])
        .current_dir(&gait_dir)
        .output()
        .unwrap();
    // Need an initial commit so the repo has a main branch for worktree creation.
    std::fs::write(gait_dir.join(".gitkeep"), "").unwrap();
    std::process::Command::new("git")
        .args(["add", ".gitkeep"])
        .current_dir(&gait_dir)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["commit", "-m", "init"])
        .current_dir(&gait_dir)
        .output()
        .unwrap();

    let wf: WorkflowDef = serde_yaml::from_str(WORKFLOW_YAML).unwrap();
    let session_id = format!(
        "parity-test-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );

    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
    let (item_tx, _item_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut obs = ChannelObserver::new(event_tx, item_tx);

    let outcome = run_workflow(
        WorkflowRunOpts {
            workflow_def: wf,
            yaml_path: "workflows/parity-test.yaml".to_string(),
            session_id: session_id.clone(),
            gait_dir: gait_dir.clone(),
            codebase: gait_dir.clone(),
            args: vec![],
        },
        &mut obs,
    )
    .await
    .unwrap();

    assert_eq!(outcome, WorkflowOutcome::Completed);

    // ── Fold the run-log and assert node statuses ──────────────────────────
    let workflow_runs_dir = gait_dir.join("workflow-runs");
    let log = WorkflowRunLog::open(&workflow_runs_dir, &session_id).unwrap();
    let events = log.read_events().unwrap();
    let state = reconstruct_state(&events);

    assert_eq!(
        state.node_status("step1"),
        NodeStatus::Done,
        "step1 should be Done"
    );
    assert_eq!(
        state.node_status("step2"),
        NodeStatus::Done,
        "step2 should be Done"
    );

    // ── Assert event channel saw WorkflowStart and WorkflowComplete ────────
    let mut saw_start = false;
    let mut saw_complete = false;
    while let Ok(ev) = event_rx.try_recv() {
        match &ev.kind {
            Some(workflow_event::Kind::WorkflowStart(_)) => saw_start = true,
            Some(workflow_event::Kind::WorkflowComplete(_)) => saw_complete = true,
            _ => {}
        }
    }
    assert!(saw_start, "event channel should have seen WorkflowStart");
    assert!(
        saw_complete,
        "event channel should have seen WorkflowComplete"
    );
}

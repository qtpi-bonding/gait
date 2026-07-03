//! Resume/exit integration tests for the orchestrator's open-node and gate paths.
//!
//! (a) An absent-prompt node with `input: None` → the run exits with
//!     `WorkflowOutcome::NodeOpen`, the run-log has `NodeStart("chat")` but
//!     **no** `NodeComplete("chat")`, and `reconstruct_state.open_node() == Some("chat")`.
//!
//! (b) A gate node → `WorkflowOutcome::GatePaused`, distinct from `NodeOpen`;
//!     no `NodeOpen` event is emitted.
//!
//! Both tests run fully offline (no model call) — shell and gate/open nodes
//! don't require an LLM.

use gait_workflow::convert::to_proto;
use gait_workflow::run_log::WorkflowRunLog;
use gait_workflow::runner::{run_workflow, WorkflowOutcome, WorkflowRunOpts};
use gait_workflow::schema::WorkflowDef;
use gait_workflow::state::reconstruct_state;

use gait_core::proto::gait::workflow::v1::workflow_event;

use gait_workflow::observer::CliObserver;
use tempfile::TempDir;

/// A workflow with a single absent-prompt node (empty `prompt:`) named "chat".
/// Running headless (input=None) should exit with NodeOpen.
const OPEN_NODE_YAML: &str = r#"
name: open-node-test
nodes:
  - id: chat
    prompt: ""
"#;

/// A workflow with a single gate node.
const GATE_YAML: &str = r#"
name: gate-test
nodes:
  - id: approve
    gate: "Ready to proceed?"
"#;

#[tokio::test]
async fn absent_prompt_headless_exits_node_open() {
    let tmp = TempDir::new().unwrap();
    let gait_dir = tmp.path().to_path_buf();

    // Initialize the git repo that session worktrees need.
    std::process::Command::new("git")
        .args(["init", "--initial-branch=main"])
        .current_dir(&gait_dir)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["commit", "--allow-empty", "-m", "init"])
        .current_dir(&gait_dir)
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "t@t.com")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "t@t.com")
        .current_dir(&gait_dir)
        .output()
        .unwrap();

    let wf: WorkflowDef = serde_yaml::from_str(OPEN_NODE_YAML).unwrap();
    let run_id = "test-open-node-001".to_string();

    let mut sink = gait_core::embed::CliSink::stdio(true);
    let outcome = run_workflow(
        WorkflowRunOpts {
            workflow_def: to_proto(&wf),
            yaml_path: "open-node-test.yaml".to_string(),
            run_id: run_id.clone(),
            gait_dir: gait_dir.clone(),
            codebase: gait_dir.clone(),
            args: vec![],
            ..Default::default()
        },
        &mut sink,
        &mut CliObserver,
        None, // headless: no input source
    )
    .await
    .unwrap();

    // (a-1) Outcome is NodeOpen, NOT GatePaused.
    assert_eq!(
        outcome,
        WorkflowOutcome::NodeOpen,
        "headless absent-prompt node must exit NodeOpen"
    );
    assert_ne!(outcome, WorkflowOutcome::GatePaused);

    // (a-2) Run-log has NodeStart("chat") but NO NodeComplete("chat").
    let run_log = WorkflowRunLog::open(&gait_dir.join("workflow-runs"), &run_id).unwrap();
    let events = run_log.read_events().unwrap_or_default();
    let has_node_start = events.iter().any(|e| {
        matches!(
            &e.kind,
            Some(workflow_event::Kind::NodeStart(ns)) if ns.node_id == "chat"
        )
    });
    let has_node_complete = events.iter().any(|e| {
        matches!(
            &e.kind,
            Some(workflow_event::Kind::NodeComplete(nc)) if nc.node_id == "chat"
        )
    });
    assert!(has_node_start, "run-log must have NodeStart(chat)");
    assert!(
        !has_node_complete,
        "run-log must NOT have NodeComplete(chat)"
    );

    // (a-3) reconstruct_state.open_node() == Some("chat").
    let state = reconstruct_state(&events);
    assert_eq!(
        state.open_node(),
        Some("chat"),
        "state must show 'chat' as the open node"
    );
    // Gate-specific field must be unset.
    assert_eq!(state.gate_paused, None);
}

#[tokio::test]
async fn gate_node_exits_gate_paused_not_node_open() {
    let tmp = TempDir::new().unwrap();
    let gait_dir = tmp.path().to_path_buf();

    // Initialize git repo.
    std::process::Command::new("git")
        .args(["init", "--initial-branch=main"])
        .current_dir(&gait_dir)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["commit", "--allow-empty", "-m", "init"])
        .current_dir(&gait_dir)
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "t@t.com")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "t@t.com")
        .current_dir(&gait_dir)
        .output()
        .unwrap();

    let wf: WorkflowDef = serde_yaml::from_str(GATE_YAML).unwrap();
    let run_id = "test-gate-001".to_string();

    let mut sink = gait_core::embed::CliSink::stdio(true);
    let outcome = run_workflow(
        WorkflowRunOpts {
            workflow_def: to_proto(&wf),
            yaml_path: "gate-test.yaml".to_string(),
            run_id: run_id.clone(),
            gait_dir: gait_dir.clone(),
            codebase: gait_dir.clone(),
            args: vec![],
            ..Default::default()
        },
        &mut sink,
        &mut CliObserver,
        None,
    )
    .await
    .unwrap();

    // (b-1) Gate exits GatePaused, NOT NodeOpen.
    assert_eq!(
        outcome,
        WorkflowOutcome::GatePaused,
        "gate node must exit GatePaused"
    );
    assert_ne!(outcome, WorkflowOutcome::NodeOpen);

    // (b-2) Run-log has WorkflowGate event (specific to gate path).
    let run_log = WorkflowRunLog::open(&gait_dir.join("workflow-runs"), &run_id).unwrap();
    let events = run_log.read_events().unwrap_or_default();
    let has_workflow_gate = events.iter().any(|e| {
        matches!(
            &e.kind,
            Some(workflow_event::Kind::WorkflowGate(wg)) if wg.node_id == "approve"
        )
    });
    assert!(
        has_workflow_gate,
        "run-log must have WorkflowGate(approve) for gate path"
    );

    // (b-3) Gate does NOT leave an open node.
    let state = reconstruct_state(&events);
    // open_node() returns Running nodes; gate sets Paused, so it should be None.
    assert_eq!(
        state.open_node(),
        None,
        "gate path must not leave an open (Running) node"
    );
    // But gate_paused should be set.
    assert_eq!(
        state.gate_paused.as_deref(),
        Some("approve"),
        "state.gate_paused must be 'approve'"
    );
}

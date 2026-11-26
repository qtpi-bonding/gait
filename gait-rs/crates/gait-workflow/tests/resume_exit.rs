//! Resume/exit integration tests for the script engine's open-node and gate
//! paths.
//!
//! (a) A promptless turn node (`gait.wf.turn({ id })` with no prompt) leaves
//!     the run open: the run exits with `WorkflowOutcome::NodeOpen`, the
//!     run-log has `NodeStart("chat")` but **no** `NodeComplete("chat")`, and
//!     `reconstruct_state.open_node() == Some("chat")`.
//!
//! (b) A gate node (`gait.wf.gate({ id, ... })`) → `WorkflowOutcome::GatePaused`,
//!     distinct from `NodeOpen`; no `NodeOpen` event is emitted.
//!
//! Both tests run fully offline (no model call) — promptless turn and gate
//! nodes don't require an LLM.

use gait_core::proto::gait::workflow::v1::workflow_event;

use gait_workflow::outcome::WorkflowOutcome;
use gait_workflow::run_log::WorkflowRunLog;
use gait_workflow::script::{run_script, ScriptRunOpts};
use gait_workflow::state::reconstruct_state;
use gait_workflow::turn_exec::testing::ScriptedTurnExecutor;
use tempfile::TempDir;

/// A script with a single promptless turn node named "chat". Running
/// headless (no user input) should exit with NodeOpen.
const OPEN_NODE_SCRIPT: &str = r#"
await gait.wf.config({ name: "open-node-test" });
await gait.wf.turn({ id: "chat" });
"#;

/// A script with a single gate node.
const GATE_SCRIPT: &str = r#"
await gait.wf.config({ name: "gate-test" });
await gait.wf.gate({ id: "approve", message: "Ready to proceed?" });
"#;

fn init_git_repo(gait_dir: &std::path::Path) {
    std::process::Command::new("git")
        .args(["init", "--initial-branch=main"])
        .current_dir(gait_dir)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["commit", "--allow-empty", "-m", "init"])
        .current_dir(gait_dir)
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "t@t.com")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "t@t.com")
        .output()
        .unwrap();
}

async fn run_js(
    tmp: &TempDir,
    run_id: &str,
    source: &str,
    exec: std::sync::Arc<dyn gait_workflow::turn_exec::TurnExecutor>,
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
            turns: exec,
        },
        &mut sink,
        &mut gait_workflow::observer::CliObserver,
    )
    .await
}

fn log_events(
    gait_dir: &std::path::Path,
    run_id: &str,
) -> Vec<gait_core::proto::gait::workflow::v1::WorkflowEvent> {
    WorkflowRunLog::open(&gait_dir.join("workflow-runs"), run_id)
        .unwrap()
        .read_events()
        .unwrap()
}

#[tokio::test]
async fn absent_prompt_headless_exits_node_open() {
    let tmp = TempDir::new().unwrap();
    let gait_dir = tmp.path().to_path_buf();

    // Initialize the git repo that session worktrees need.
    init_git_repo(&gait_dir);

    let run_id = "test-open-node-001".to_string();

    let exec = std::sync::Arc::new(ScriptedTurnExecutor::new())
        as std::sync::Arc<dyn gait_workflow::turn_exec::TurnExecutor>;
    let outcome = run_js(&tmp, &run_id, OPEN_NODE_SCRIPT, exec).await.unwrap();

    // (a-1) Outcome is NodeOpen, NOT GatePaused.
    assert_eq!(
        outcome,
        WorkflowOutcome::NodeOpen,
        "headless absent-prompt node must exit NodeOpen"
    );
    assert_ne!(outcome, WorkflowOutcome::GatePaused);

    // (a-2) Run-log has NodeStart("chat") but NO NodeComplete("chat").
    let events = log_events(&gait_dir, &run_id);
    let has_node_start = events.iter().any(
        |e| matches!(&e.kind, Some(workflow_event::Kind::NodeStart(ns)) if ns.node_id == "chat"),
    );
    let has_node_complete = events.iter().any(
        |e| matches!(&e.kind, Some(workflow_event::Kind::NodeComplete(nc)) if nc.node_id == "chat"),
    );
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
    init_git_repo(&gait_dir);

    let run_id = "test-gate-001".to_string();

    let exec = std::sync::Arc::new(ScriptedTurnExecutor::new())
        as std::sync::Arc<dyn gait_workflow::turn_exec::TurnExecutor>;
    let outcome = run_js(&tmp, &run_id, GATE_SCRIPT, exec).await.unwrap();

    // (b-1) Gate exits GatePaused, NOT NodeOpen.
    assert_eq!(
        outcome,
        WorkflowOutcome::GatePaused,
        "gate node must exit GatePaused"
    );
    assert_ne!(outcome, WorkflowOutcome::NodeOpen);

    // (b-2) Run-log has WorkflowGate event (specific to gate path).
    let events = log_events(&gait_dir, &run_id);
    let has_workflow_gate = events
        .iter()
        .any(|e| matches!(&e.kind, Some(workflow_event::Kind::WorkflowGate(wg)) if wg.node_id == "approve"));
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

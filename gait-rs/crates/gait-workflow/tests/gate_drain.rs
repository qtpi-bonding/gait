//! In-flight steps must fully drain before a gate pauses the run and it
//! returns GatePaused; the gate step has NodeStart and no NodeComplete.
//!
//! Script: two shell steps a and b started together (each `sleep 1; echo ...`,
//! so both are genuinely in flight), then an independent gate h started while
//! a and b are still running, all three awaited together.
//! Expectation: start:h is recorded BEFORE complete:a and complete:b —
//! proving a and b were still executing when h was reached — yet
//! NodeComplete for a and b are both recorded before the run returns
//! GatePaused, proving the in-flight steps drain before the pause.

use gait_core::proto::gait::workflow::v1::{workflow_event, WorkflowEvent};
use gait_workflow::observer::CliObserver;
use gait_workflow::outcome::WorkflowOutcome;
use gait_workflow::run_log::WorkflowRunLog;
use gait_workflow::script::{run_script, ScriptRunOpts};
use gait_workflow::turn_exec::testing::ScriptedTurnExecutor;

use tempfile::TempDir;

const WORKFLOW_JS: &str = r#"
await gait.wf.config({ name: "gate-drain-test" });
const a = gait.wf.shell({ id: "a", script: "sleep 1; echo alpha" });
const b = gait.wf.shell({ id: "b", script: "sleep 1; echo beta" });
const h = gait.wf.gate({ id: "h", message: "waiting for a person" });
await Promise.all([a, b, h]);
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

async fn run_js(tmp: &TempDir, run_id: &str, source: &str) -> anyhow::Result<WorkflowOutcome> {
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
            turns: std::sync::Arc::new(ScriptedTurnExecutor::new()),
        },
        &mut sink,
        &mut CliObserver,
    )
    .await
}

/// Maps a NodeStart/NodeComplete event to a `"start:<id>"`/`"complete:<id>"`
/// label; other event kinds map to `None`.
fn node_event_label(e: &WorkflowEvent) -> Option<String> {
    match &e.kind {
        Some(workflow_event::Kind::NodeStart(ns)) => Some(format!("start:{}", ns.node_id)),
        Some(workflow_event::Kind::NodeComplete(nc)) => Some(format!("complete:{}", nc.node_id)),
        _ => None,
    }
}

#[tokio::test]
async fn automated_nodes_drain_before_gate_pause() {
    let tmp = TempDir::new().unwrap();
    let gait_dir = tmp.path().to_path_buf();
    setup_gait_dir(&gait_dir);

    let run_id = "test-gate-drain-001".to_string();

    let outcome = run_js(&tmp, &run_id, WORKFLOW_JS).await.unwrap();

    assert_eq!(
        outcome,
        WorkflowOutcome::GatePaused,
        "run must pause at the gate"
    );

    let run_log = WorkflowRunLog::open(&gait_dir.join("workflow-runs"), &run_id).unwrap();
    let events = run_log.read_events().unwrap_or_default();
    let order: Vec<String> = events.iter().filter_map(node_event_label).collect();

    let a_done = order
        .iter()
        .position(|e| e == "complete:a")
        .expect("a must have completed");
    let b_done = order
        .iter()
        .position(|e| e == "complete:b")
        .expect("b must have completed");
    let h_start = order
        .iter()
        .position(|e| e == "start:h")
        .expect("h must have started");
    let a_start = order
        .iter()
        .position(|e| e == "start:a")
        .expect("a must have started");
    let b_start = order
        .iter()
        .position(|e| e == "start:b")
        .expect("b must have started");

    assert!(
        a_start < h_start && b_start < h_start,
        "a and b must have been started (in flight) before h: {order:?}"
    );
    assert!(
        h_start < a_done && h_start < b_done,
        "h must start while a and b are still executing: {order:?}"
    );
    // The gate step itself must have NodeStart and no NodeComplete.
    let h_done = order.iter().any(|e| e == "complete:h");
    assert!(!h_done, "gate node h must not be completed: {order:?}");
}

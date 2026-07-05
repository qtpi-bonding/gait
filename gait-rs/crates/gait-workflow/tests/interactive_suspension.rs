//! Regression test for item 8: automated nodes must fully drain before an
//! independent interactive (absent-prompt) node suspends the run.
//!
//! Graph: automated chain a -> b (shell, no model needed) plus an
//! independent interactive node h (empty prompt), no live input source.
//! Expectation: NodeComplete for a and b are both recorded BEFORE the run
//! returns NodeOpen for h — proving the automated frontier drains first,
//! not merely that h eventually opens.

use gait_workflow::convert::to_proto;
use gait_workflow::observer::CliObserver;
use gait_workflow::run_log::WorkflowRunLog;
use gait_workflow::runner::{run_workflow, WorkflowOutcome, WorkflowRunOpts};
use gait_workflow::schema::WorkflowDef;

use gait_core::proto::gait::workflow::v1::{workflow_event, WorkflowEvent};

use tempfile::TempDir;

const WORKFLOW_YAML: &str = r#"
name: interactive-suspension-test
nodes:
  - id: a
    shell: "echo alpha"
  - id: b
    depends_on: [a]
    shell: "echo beta"
  - id: h
    prompt: ""
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
async fn automated_nodes_drain_before_interactive_suspension() {
    let tmp = TempDir::new().unwrap();
    let gait_dir = tmp.path().to_path_buf();
    setup_gait_dir(&gait_dir);

    let wf: WorkflowDef = serde_yaml::from_str(WORKFLOW_YAML).unwrap();
    let run_id = "test-interactive-suspension-001".to_string();

    let mut sink = gait_core::embed::CliSink::stdio(true);
    let outcome = run_workflow(
        WorkflowRunOpts {
            workflow_def: to_proto(&wf),
            yaml_path: "interactive-suspension-test.yaml".to_string(),
            run_id: run_id.clone(),
            gait_dir: gait_dir.clone(),
            codebase: gait_dir.clone(),
            args: vec![],
            ..Default::default()
        },
        &mut sink,
        &mut CliObserver,
        None, // headless: no live input source
    )
    .await
    .unwrap();

    assert_eq!(
        outcome,
        WorkflowOutcome::NodeOpen,
        "run must suspend at the interactive node"
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

    assert!(
        a_done < h_start && b_done < h_start,
        "automated nodes must complete before interactive suspension: {order:?}"
    );

    // h itself must be left open, not completed.
    let h_done = order.iter().any(|e| e == "complete:h");
    assert!(
        !h_done,
        "interactive node h must not be completed: {order:?}"
    );
}

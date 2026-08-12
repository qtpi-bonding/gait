//! Integration test: a workflow graph with a depends_on cycle is rejected by
//! run_workflow before any node executes, any run-log is created, or any
//! worktree/session is touched — the actual fix for audit finding #1
//! (silent WorkflowOutcome::Completed having run zero nodes).

use gait_core::proto::gait::workflow::v1::{Node, Workflow};
use gait_workflow::observer::NullObserver;
use gait_workflow::runner::{run_workflow, WorkflowRunOpts};
use tempfile::TempDir;

fn node_with_depends_on(id: &str, deps: &[&str]) -> Node {
    Node {
        id: id.to_string(),
        depends_on: deps.iter().map(|s| s.to_string()).collect(),
        ..Default::default()
    }
}

#[tokio::test]
async fn cyclic_depends_on_graph_is_rejected_before_any_run_log_is_created() {
    let dir = TempDir::new().unwrap();
    let gait_dir = dir.path().to_path_buf();
    std::fs::create_dir_all(gait_dir.join("workflow-runs")).unwrap();

    let cyclic_wf = Workflow {
        name: "cyclic".to_string(),
        nodes: vec![
            node_with_depends_on("a", &["b"]),
            node_with_depends_on("b", &["a"]),
        ],
        ..Default::default()
    };

    let run_id = "cyclic-run-01".to_string();
    let mut observer = NullObserver;
    let mut sink = gait_core::embed::CliSink::stdio(false);

    let result = run_workflow(
        WorkflowRunOpts {
            workflow_def: cyclic_wf,
            yaml_path: "cyclic.yaml".to_string(),
            run_id: run_id.clone(),
            gait_dir: gait_dir.clone(),
            codebase: gait_dir.join("codebase"),
            args: vec![],
            ..Default::default()
        },
        &mut sink,
        &mut observer,
        None,
    )
    .await;

    assert!(
        result.is_err(),
        "cyclic graph must be rejected, got: {result:?}"
    );
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("depends_on cycle"),
        "error should name the actual problem, got: {err_msg}"
    );

    // Zero run-log activity: WorkflowRunLog::create (runner.rs) never ran.
    let run_log_path = gait_dir
        .join("workflow-runs")
        .join(&run_id)
        .join("log.jsonl");
    assert!(
        !run_log_path.exists(),
        "no run-log should be created for a rejected graph"
    );
}

#[tokio::test]
async fn invalid_branch_edge_is_rejected() {
    let dir = TempDir::new().unwrap();
    let gait_dir = dir.path().to_path_buf();
    std::fs::create_dir_all(gait_dir.join("workflow-runs")).unwrap();

    let wf = Workflow {
        name: "dangling-recovery".to_string(),
        nodes: vec![Node {
            id: "a".to_string(),
            body: Some(gait_core::proto::gait::workflow::v1::node::Body::Branch(
                gait_core::proto::gait::workflow::v1::BranchNode {
                    then: Some("nonexistent-recovery-node".to_string()),
                    ..Default::default()
                },
            )),
            ..Default::default()
        }],
        ..Default::default()
    };

    let run_id = "dangling-recovery-run-01".to_string();
    let mut observer = NullObserver;
    let mut sink = gait_core::embed::CliSink::stdio(false);

    let result = run_workflow(
        WorkflowRunOpts {
            workflow_def: wf,
            yaml_path: "dangling.yaml".to_string(),
            run_id: run_id.clone(),
            gait_dir: gait_dir.clone(),
            codebase: gait_dir.join("codebase"),
            args: vec![],
            ..Default::default()
        },
        &mut sink,
        &mut observer,
        None,
    )
    .await;

    assert!(
        result.is_err(),
        "dangling branch edge must be rejected, got: {result:?}"
    );
    assert!(result.unwrap_err().to_string().contains("branch.then"));
}

//! Integration test: a branch node whose `on:` expression references a node
//! that hasn't produced output yet fails the branch node explicitly instead
//! of silently routing down an arbitrary arm — the fix for audit finding #6
//! (branch conditions fail open on unresolvable expressions).

use gait_core::proto::gait::workflow::v1::{node, BranchNode, Node, ShellNode, Workflow};
use gait_workflow::observer::NullObserver;
use gait_workflow::runner::{run_workflow, WorkflowRunOpts};
use tempfile::TempDir;

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

#[tokio::test]
async fn branch_condition_referencing_a_node_that_has_not_run_fails_the_node() {
    let dir = TempDir::new().unwrap();
    let gait_dir = dir.path().to_path_buf();
    init_git_repo(&gait_dir);

    // "producer" and "check" have no depends_on between them, so both are in
    // the initial ready-set and "check"'s VarContext snapshot (taken at
    // enqueue time, before either node has completed) never contains
    // "producer"'s output — deterministic regardless of execution order.
    let wf = Workflow {
        name: "branch-unresolved".to_string(),
        nodes: vec![
            Node {
                id: "producer".to_string(),
                body: Some(node::Body::Shell(ShellNode {
                    command: "exit 0".to_string(),
                    timeout_ms: None,
                })),
                ..Default::default()
            },
            Node {
                id: "check".to_string(),
                body: Some(node::Body::Branch(BranchNode {
                    on: "$producer.exit == 0".to_string(),
                    then: Some("ship".to_string()),
                    r#else: Some("fix".to_string()),
                    cases: Default::default(),
                })),
                ..Default::default()
            },
            Node {
                id: "ship".to_string(),
                body: Some(node::Body::Shell(ShellNode {
                    command: "echo ship".to_string(),
                    timeout_ms: None,
                })),
                depends_on: vec!["check".to_string()],
                ..Default::default()
            },
            Node {
                id: "fix".to_string(),
                body: Some(node::Body::Shell(ShellNode {
                    command: "echo fix".to_string(),
                    timeout_ms: None,
                })),
                depends_on: vec!["check".to_string()],
                ..Default::default()
            },
        ],
        ..Default::default()
    };

    let mut observer = NullObserver;
    let mut sink = gait_core::embed::CliSink::stdio(false);
    let result = run_workflow(
        WorkflowRunOpts {
            workflow_def: wf,
            yaml_path: "branch-unresolved.yaml".to_string(),
            run_id: "branch-unresolved-01".to_string(),
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
        "unresolvable branch condition must fail the workflow, got: {result:?}"
    );
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("check") && msg.contains("producer") && msg.contains("has not completed"),
        "error should name the branch node and the unresolved reference, got: {msg}"
    );
}

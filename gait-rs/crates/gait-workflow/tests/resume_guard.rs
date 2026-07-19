//! Integration test: a shell node left `Running` by an interrupted workflow
//! is not silently re-run on `gait workflow continue` — the fix for audit
//! finding #2 (resume idempotency). Mirrors the run-log hand-construction
//! pattern already used in `crates/gait-cli/src/cli/workflow.rs`'s
//! `abort_folds_run_to_failed` test.

use gait_core::proto::gait::workflow::v1::{Node, ShellNode, Workflow};
use gait_workflow::observer::NullObserver;
use gait_workflow::run_log::WorkflowRunLog;
use gait_workflow::runner::{run_workflow, WorkflowRunOpts};
use tempfile::TempDir;

fn shell_node(id: &str, command: &str) -> Node {
    Node {
        id: id.to_string(),
        body: Some(gait_core::proto::gait::workflow::v1::node::Body::Shell(
            ShellNode {
                command: command.to_string(),
                timeout_ms: None,
            },
        )),
        ..Default::default()
    }
}

/// `run_workflow` creates a branch/worktree off the gait-dir's own git repo
/// (`create_branch_worktree`, before the resume guard ever runs) whenever the
/// graph needs run workspace — which every shell-node workflow here does. A
/// bare TempDir isn't a git repo, so every test below must initialize one
/// first, exactly like `crates/gait-workflow/tests/run_lock.rs`'s own helper.
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

/// Write a run log with a WorkflowStart for the given graph, then a NodeStart
/// for every node in `running_node_ids` with no matching completion event —
/// simulating a process killed mid-execution.
fn write_interrupted_run_log(
    gait_dir: &std::path::Path,
    run_id: &str,
    wf: &Workflow,
    running_node_ids: &[&str],
) {
    let runs_dir = gait_dir.join("workflow-runs");
    let log = WorkflowRunLog::create(&runs_dir, run_id).unwrap();
    log.workflow_start(run_id, &wf.name, "wf.yaml", &[], wf)
        .unwrap();
    for id in running_node_ids {
        log.node_start(id, "shell", 1, "").unwrap();
    }
}

#[tokio::test]
async fn stuck_shell_node_is_rejected_without_force_rerun() {
    let dir = TempDir::new().unwrap();
    let gait_dir = dir.path().to_path_buf();
    init_git_repo(&gait_dir);
    std::fs::create_dir_all(gait_dir.join("workflow-runs")).unwrap();

    let wf = Workflow {
        name: "deploy".to_string(),
        nodes: vec![shell_node("a", "echo hi")],
        ..Default::default()
    };
    let run_id = "resume-guard-01".to_string();
    write_interrupted_run_log(&gait_dir, &run_id, &wf, &["a"]);

    let mut observer = NullObserver;
    let mut sink = gait_core::embed::CliSink::stdio(false);
    let result = run_workflow(
        WorkflowRunOpts {
            workflow_def: wf,
            yaml_path: "wf.yaml".to_string(),
            run_id: run_id.clone(),
            gait_dir: gait_dir.clone(),
            codebase: gait_dir.join("codebase"),
            args: vec![],
            force_rerun_nodes: vec![],
            ..Default::default()
        },
        &mut sink,
        &mut observer,
        None,
    )
    .await;

    assert!(
        result.is_err(),
        "stuck shell node must be rejected, got: {result:?}"
    );
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("'a'"),
        "error should name the stuck node, got: {msg}"
    );
    assert!(
        msg.contains("--force-rerun"),
        "error should mention the escape hatch, got: {msg}"
    );
}

#[tokio::test]
async fn stuck_shell_node_reruns_with_force_rerun() {
    let dir = TempDir::new().unwrap();
    let gait_dir = dir.path().to_path_buf();
    init_git_repo(&gait_dir);
    std::fs::create_dir_all(gait_dir.join("workflow-runs")).unwrap();

    let wf = Workflow {
        name: "deploy".to_string(),
        nodes: vec![shell_node("a", "echo hi")],
        ..Default::default()
    };
    let run_id = "resume-guard-02".to_string();
    write_interrupted_run_log(&gait_dir, &run_id, &wf, &["a"]);

    let mut observer = NullObserver;
    let mut sink = gait_core::embed::CliSink::stdio(false);
    let result = run_workflow(
        WorkflowRunOpts {
            workflow_def: wf,
            yaml_path: "wf.yaml".to_string(),
            run_id: run_id.clone(),
            gait_dir: gait_dir.clone(),
            codebase: gait_dir.join("codebase"),
            args: vec![],
            force_rerun_nodes: vec!["a".to_string()],
            ..Default::default()
        },
        &mut sink,
        &mut observer,
        None,
    )
    .await;

    assert!(
        result.is_ok(),
        "force-rerun must let the workflow proceed, got: {result:?}"
    );
}

#[tokio::test]
async fn naming_only_one_of_two_stuck_nodes_still_rejects_and_lists_both() {
    let dir = TempDir::new().unwrap();
    let gait_dir = dir.path().to_path_buf();
    init_git_repo(&gait_dir);
    std::fs::create_dir_all(gait_dir.join("workflow-runs")).unwrap();

    // Two independent shell nodes (no depends_on) both left Running —
    // simulates a parallel-crash scenario.
    let wf = Workflow {
        name: "deploy".to_string(),
        nodes: vec![shell_node("a", "echo a"), shell_node("b", "echo b")],
        ..Default::default()
    };
    let run_id = "resume-guard-03".to_string();
    write_interrupted_run_log(&gait_dir, &run_id, &wf, &["a", "b"]);

    let mut observer = NullObserver;
    let mut sink = gait_core::embed::CliSink::stdio(false);
    let result = run_workflow(
        WorkflowRunOpts {
            workflow_def: wf,
            yaml_path: "wf.yaml".to_string(),
            run_id: run_id.clone(),
            gait_dir: gait_dir.clone(),
            codebase: gait_dir.join("codebase"),
            args: vec![],
            force_rerun_nodes: vec!["a".to_string()], // only names one of the two
            ..Default::default()
        },
        &mut sink,
        &mut observer,
        None,
    )
    .await;

    assert!(
        result.is_err(),
        "partial consent must still reject, got: {result:?}"
    );
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("'a'") && msg.contains("'b'"),
        "error should name both stuck nodes, got: {msg}"
    );
}

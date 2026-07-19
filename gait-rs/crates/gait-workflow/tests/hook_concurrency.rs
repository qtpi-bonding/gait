//! Regression test for gait-workflow audit finding #7: a hook on one node
//! must not stall the scheduler for every sibling node. Two independent
//! (no depends_on) shell nodes each carry a before-hook that sleeps 1s.
//! `run_workflow`'s initial ready-node loop awaits `enqueue_node_task`
//! SEQUENTIALLY for each ready node (runner.rs's `for idx in initially_ready`
//! loop) — before this fix, that sequential await included the before-hook
//! itself, so two 1s hooks serialize to ~2s. After the fix, hook execution
//! moves into each node's own pooled future, so both hooks run concurrently
//! and the whole run completes in ~1s.

use gait_core::proto::gait::workflow::v1::{node, Hooks, Node, ShellNode, Workflow};
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

fn slow_hook_node(id: &str) -> Node {
    Node {
        id: id.to_string(),
        hooks: Some(Hooks {
            before: Some("sleep 1".to_string()),
            after: None,
            error: None,
            timeout_ms: None,
        }),
        body: Some(node::Body::Shell(ShellNode {
            command: "true".to_string(),
            timeout_ms: None,
        })),
        ..Default::default()
    }
}

#[tokio::test]
async fn independent_slow_hooks_run_concurrently_not_serialized() {
    let dir = TempDir::new().unwrap();
    let gait_dir = dir.path().to_path_buf();
    init_git_repo(&gait_dir);

    let wf = Workflow {
        name: "hook-concurrency".to_string(),
        nodes: vec![slow_hook_node("a"), slow_hook_node("b")],
        ..Default::default()
    };

    let mut observer = NullObserver;
    let mut sink = gait_core::embed::CliSink::stdio(false);
    let start = tokio::time::Instant::now();
    run_workflow(
        WorkflowRunOpts {
            workflow_def: wf,
            yaml_path: "wf.yaml".to_string(),
            run_id: "hook-concurrency-01".to_string(),
            gait_dir: gait_dir.clone(),
            codebase: gait_dir.join("codebase"),
            args: vec![],
            ..Default::default()
        },
        &mut sink,
        &mut observer,
        None,
    )
    .await
    .unwrap();
    let elapsed = start.elapsed();

    assert!(
        elapsed < std::time::Duration::from_millis(1_500),
        "two independent 1s before-hooks must run concurrently (~1s total), \
         not serialized (~2s total) — took {elapsed:?}"
    );
}

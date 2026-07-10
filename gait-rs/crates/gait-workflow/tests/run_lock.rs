//! Task 1.3: run-level branch + worktree + run-lock under `workflow-runs/<run_id>/`.
//!
//! (a) `SessionLock` (reused as a generic RAII dir lock) rejects a second
//!     concurrent acquire on the same run dir — one executor per run.
//! (b) The positive path: after a headless `run_workflow` call, the run dir
//!     holds a `worktree/` on branch `run/<run_id>`.

use gait_core::session::SessionLock;

use gait_workflow::convert::to_proto;
use gait_workflow::observer::CliObserver;
use gait_workflow::runner::{run_workflow, WorkflowOutcome, WorkflowRunOpts};
use gait_workflow::schema::WorkflowDef;

use tempfile::TempDir;

/// A trivial single-node shell workflow that completes offline (no model call).
const SHELL_YAML: &str = r#"
name: run-lock-test
nodes:
  - id: step
    shell: "true"
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

#[tokio::test]
async fn second_executor_on_same_run_is_rejected() {
    let tmp = TempDir::new().unwrap();
    let gait_dir = tmp.path().to_path_buf();
    init_git_repo(&gait_dir);

    let run_dir = gait_dir.join("workflow-runs").join("run-xyz");
    std::fs::create_dir_all(&run_dir).unwrap();
    let held = SessionLock::acquire(&run_dir);
    assert!(held.is_ok());
    let _held = held.ok();
    // A second acquire on the same run dir must hard-fail (one executor per run).
    let second = SessionLock::acquire(&run_dir);
    let err = second.err().unwrap();
    assert!(err.to_string().contains("already running"), "got: {err}");
}

#[tokio::test]
async fn run_workflow_creates_run_branch_and_worktree() {
    let tmp = TempDir::new().unwrap();
    let gait_dir = tmp.path().to_path_buf();
    init_git_repo(&gait_dir);

    let wf: WorkflowDef = serde_yaml::from_str(SHELL_YAML).unwrap();
    let run_id = "test-run-lock-001".to_string();

    let mut sink = gait_core::embed::CliSink::stdio(true);
    let outcome = run_workflow(
        WorkflowRunOpts {
            workflow_def: to_proto(&wf),
            yaml_path: "run-lock-test.yaml".to_string(),
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

    assert_eq!(outcome, WorkflowOutcome::Completed);

    let run_dir = gait_dir.join("workflow-runs").join(&run_id);
    let worktree = run_dir.join("worktree");
    assert!(
        worktree.join(".git").exists(),
        "expected worktree/.git to exist at {}",
        worktree.display()
    );

    // Branch `run/<run_id>` must exist in the main repo.
    let repo = git2::Repository::open(&gait_dir).unwrap();
    assert!(
        repo.find_branch(&format!("run/{run_id}"), git2::BranchType::Local)
            .is_ok(),
        "expected branch run/{run_id} to exist"
    );

    // The lock must have been released once run_workflow returned.
    let _reacquire = SessionLock::acquire(&run_dir).unwrap();
}

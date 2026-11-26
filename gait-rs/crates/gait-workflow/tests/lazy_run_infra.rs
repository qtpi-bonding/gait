//! Spec-06 §2 (script engine): a degenerate single-prompt script run creates
//! run_id + run log only — no run worktree, no run lock. (The positive/shell
//! case lives in run_lock.rs.)
use std::sync::Arc;

use gait_workflow::observer::NullObserver;
use gait_workflow::outcome::WorkflowOutcome;
use gait_workflow::script::{run_script, ScriptRunOpts};
use gait_workflow::turn_exec::testing::ScriptedTurnExecutor;

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

/// A degenerate script workflow: one step whose turn carries no prompt,
/// which leaves the run open (`WorkflowOutcome::NodeOpen`) before any model
/// turn is executed.
fn degenerate_script() -> String {
    r#"
        const cfg = await gait.wf.config({ name: "chat" });
        const t = await gait.wf.turn({ id: "chat" });
    "#
    .to_string()
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
            turns: Arc::new(ScriptedTurnExecutor::new()),
        },
        &mut sink,
        &mut NullObserver,
    )
    .await
}

#[tokio::test]
async fn degenerate_single_prompt_run_creates_no_run_worktree_or_lock() {
    let tmp = TempDir::new().unwrap();
    let gait_dir = tmp.path().to_path_buf();
    init_git_repo(&gait_dir);
    let run_id = "chat-lazy-001".to_string();
    let outcome = run_js(&tmp, &run_id, &degenerate_script()).await.unwrap();
    assert_eq!(outcome, WorkflowOutcome::NodeOpen);
    let run_dir = gait_dir.join("workflow-runs").join(&run_id);
    assert!(run_dir.join("log.jsonl").exists(), "run log must exist");
    assert!(
        !run_dir.join("worktree").exists(),
        "degenerate run must NOT mint a run worktree"
    );
    // M2 (review): do NOT assert `!run_dir.join("lock").exists()` — SessionLock deletes its
    // file on Drop, so post-return it is always absent even for a run that DID lock. The
    // worktree- and branch-absence assertions are the meaningful laziness checks.
    let repo = git2::Repository::open(&gait_dir).unwrap();
    assert!(
        repo.find_branch(&format!("run/{run_id}"), git2::BranchType::Local)
            .is_err(),
        "degenerate run must NOT create branch run/{run_id}"
    );
}

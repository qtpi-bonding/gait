//! Spec-06 §2: a degenerate single-prompt run creates run_id + run log only —
//! no run worktree, no run lock. (The positive/shell case lives in run_lock.rs.)
use gait_core::proto::gait::workflow::v1::{node, Node, PromptNode, Workflow};
use gait_turn_runner::TurnProfileOverride;
use gait_workflow::observer::CliObserver;
use gait_workflow::runner::{run_workflow, WorkflowOutcome, WorkflowRunOpts};

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

fn degenerate_wf() -> Workflow {
    Workflow {
        name: "chat".into(),
        nodes: vec![Node {
            id: "chat".into(),
            body: Some(node::Body::Prompt(PromptNode {
                prompt: String::new(),
                continue_session: String::new(),
                codebase: None,
            })),
            ..Default::default()
        }],
        ..Default::default()
    }
}

#[tokio::test]
async fn degenerate_single_prompt_run_creates_no_run_worktree_or_lock() {
    let tmp = TempDir::new().unwrap();
    let gait_dir = tmp.path().to_path_buf();
    init_git_repo(&gait_dir);
    let run_id = "chat-lazy-001".to_string();
    let mut sink = gait_core::embed::CliSink::stdio(true);
    let outcome = run_workflow(
        WorkflowRunOpts {
            workflow_def: degenerate_wf(),
            yaml_path: String::new(),
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

// Task 3: `single_prompt_run`/`SinglePromptOpts` are gone — the degenerate
// one-node graph they used to build is now exercised directly through
// `run_workflow`, same as every other node. This test also carries a
// `profile: Some(TurnProfileOverride)` override (unlike the sibling test
// above, which passes none) to prove `exec_prompt_node`'s shared-runner
// profile plumbing doesn't disturb the run's laziness — the degenerate node
// here is absent-prompt/no-input, so it returns `NodeOutput::open()` before
// the override's allow/deny/inference fields are ever consulted.
#[tokio::test]
async fn degenerate_graph_with_profile_override_stays_lazy() {
    let tmp = TempDir::new().unwrap();
    let gait_dir = tmp.path().to_path_buf();
    init_git_repo(&gait_dir);
    let run_id = "chat-spr-001".to_string();
    let mut sink = gait_core::embed::CliSink::stdio(true);
    let outcome = run_workflow(
        WorkflowRunOpts {
            workflow_def: degenerate_wf(),
            yaml_path: String::new(),
            run_id: run_id.clone(),
            gait_dir: gait_dir.clone(),
            codebase: gait_dir.clone(),
            args: vec![],
            profile: Some(TurnProfileOverride {
                inference: None,
                explicit_inference_override: false,
                session_updates: vec![],
                allow: vec![],
                deny: vec![],
                agent: None,
                max_iter: None,
                result_schema: None,
                auto: None,
            }),
            ..Default::default()
        },
        &mut sink,
        &mut CliObserver,
        None,
    )
    .await
    .unwrap();
    assert_eq!(
        outcome,
        WorkflowOutcome::NodeOpen,
        "prompt:None + input:None → open node"
    );
    let run_dir = gait_dir.join("workflow-runs").join(&run_id);
    assert!(run_dir.join("log.jsonl").exists());
    assert!(
        !run_dir.join("worktree").exists(),
        "a degenerate node stays lazy through the shared runner"
    );
    // (No `lock`-absence assertion — see M2 note in Task 1.1: it's vacuous post-Drop.)
}

//! C1 regression: a SECOND `run_workflow` invocation with the SAME `run_id`
//! (i.e. what `gait workflow continue <run_id>` does) must SUCCEED.
//!
//! Before the fix, `run_workflow` chose open-vs-create for the run log by
//! testing the (now never-written) session-transcript path, so the second
//! invocation always hit `WorkflowRunLog::create` (`O_CREAT | O_EXCL`) and
//! hard-errored "workflow run log already exists". The predicate now keys on the
//! run log's OWN existence, so resume works. This test drives a gate workflow to
//! a pause, then resumes it to completion on the same run_id.

use gait_workflow::convert::to_proto;
use gait_workflow::run_log::WorkflowRunLog;
use gait_workflow::runner::{run_workflow, WorkflowOutcome, WorkflowRunOpts};
use gait_workflow::schema::WorkflowDef;
use gait_workflow::state::reconstruct_state;

use gait_core::proto::gait::workflow::v1::workflow_event;
use gait_workflow::observer::CliObserver;
use tempfile::TempDir;

/// Gate node, then a shell node that depends on it. First run pauses at the
/// gate; the resume must clear the gate and run `after` to completion.
const GATE_THEN_SHELL_YAML: &str = r#"
name: continue-test
nodes:
  - id: approve
    gate: "Ready?"
  - id: after
    depends_on: [approve]
    shell: "echo resumed"
"#;

fn init_git(gait_dir: &std::path::Path) {
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
async fn second_same_run_id_invocation_resumes_instead_of_erroring() {
    let tmp = TempDir::new().unwrap();
    let gait_dir = tmp.path().to_path_buf();
    init_git(&gait_dir);

    let wf: WorkflowDef = serde_yaml::from_str(GATE_THEN_SHELL_YAML).unwrap();
    let run_id = "continue-test-001".to_string();

    // --- First invocation: pauses at the gate.
    let mut sink = gait_core::embed::CliSink::stdio(true);
    let first = run_workflow(
        WorkflowRunOpts {
            workflow_def: to_proto(&wf),
            yaml_path: "continue-test.yaml".to_string(),
            run_id: run_id.clone(),
            gait_dir: gait_dir.clone(),
            codebase: gait_dir.clone(),
            args: vec![],
        },
        &mut sink,
        &mut CliObserver,
        None,
    )
    .await
    .expect("first run should pause cleanly, not error");
    assert_eq!(first, WorkflowOutcome::GatePaused);

    // --- Second invocation with the SAME run_id (simulates `workflow continue`).
    // This is the exact path C1 broke: it MUST NOT error "already exists".
    let mut sink2 = gait_core::embed::CliSink::stdio(true);
    let second = run_workflow(
        WorkflowRunOpts {
            workflow_def: to_proto(&wf),
            yaml_path: "continue-test.yaml".to_string(),
            run_id: run_id.clone(),
            gait_dir: gait_dir.clone(),
            codebase: gait_dir.clone(),
            args: vec![],
        },
        &mut sink2,
        &mut CliObserver,
        None,
    )
    .await
    .expect("second same-run_id invocation must resume, not error 'already exists'");
    assert_eq!(
        second,
        WorkflowOutcome::Completed,
        "resumed run must clear the gate and complete"
    );

    // The resumed run actually ran the post-gate node and finished.
    let events = WorkflowRunLog::open(&gait_dir.join("workflow-runs"), &run_id)
        .unwrap()
        .read_events()
        .unwrap();
    assert!(
        events.iter().any(|e| matches!(
            &e.kind,
            Some(workflow_event::Kind::NodeComplete(nc)) if nc.node_id == "after"
        )),
        "resume must have executed the post-gate 'after' node"
    );
    let state = reconstruct_state(&events);
    assert_eq!(state.workflow_status(), "done");
}

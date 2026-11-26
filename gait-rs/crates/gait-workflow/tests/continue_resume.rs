//! C1 regression, script engine edition: a SECOND `run_script` invocation with
//! the SAME `run_id` (i.e. what `gait workflow continue <run_id>` does) must
//! SUCCEED.
//!
//! The original YAML-engine bug was that the open-vs-create predicate for the
//! run log keyed on the (now never-written) session-transcript path, so the
//! second invocation always hit `WorkflowRunLog::create` (`O_CREAT | O_EXCL`)
//! and hard-errored "workflow run log already exists". This test drives a
//! gate script to a pause, then resumes it to completion on the same run_id.

use std::path::Path;
use std::sync::Arc;

use gait_core::proto::gait::workflow::v1::workflow_event;
use gait_workflow::observer::CliObserver;
use gait_workflow::run_log::WorkflowRunLog;
use gait_workflow::runner::WorkflowOutcome;
use gait_workflow::script::{run_script, ScriptRunOpts};
use gait_workflow::state::reconstruct_state;
use gait_workflow::turn_exec::testing::ScriptedTurnExecutor;
use tempfile::TempDir;

fn init_git(gait_dir: &Path) {
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

/// Gate step, then a shell step that depends on it (plain `await` ordering).
/// First run pauses at the gate; the resume must clear the gate and run
/// `after` to completion.
const GATE_THEN_SHELL_JS: &str = r#"
    await gait.wf.config({ name: "continue-test" });
    await gait.wf.gate({ id: "approve", message: "Ready?" });
    await gait.wf.shell({ id: "after", script: "echo resumed" });
"#;

async fn run_js(gait_dir: &Path, run_id: &str, source: &str) -> anyhow::Result<WorkflowOutcome> {
    let script_path = gait_dir.join("wf.js");
    std::fs::write(&script_path, source).unwrap();
    let mut sink = gait_core::embed::CliSink::stdio(true);
    run_script(
        ScriptRunOpts {
            script_path,
            run_id: run_id.to_string(),
            gait_dir: gait_dir.to_path_buf(),
            codebase: gait_dir.to_path_buf(),
            args: vec![],
            force_rerun_nodes: vec![],
            force: false,
            turns: Arc::new(ScriptedTurnExecutor::new()),
        },
        &mut sink,
        &mut CliObserver,
    )
    .await
}

#[tokio::test]
async fn second_same_run_id_invocation_resumes_instead_of_erroring() {
    let tmp = TempDir::new().unwrap();
    let gait_dir = tmp.path().to_path_buf();
    init_git(&gait_dir);
    let run_id = "continue-test-001";

    // --- First invocation: pauses at the gate.
    let first = run_js(&gait_dir, run_id, GATE_THEN_SHELL_JS)
        .await
        .expect("first run should pause cleanly, not error");
    assert_eq!(first, WorkflowOutcome::GatePaused);

    // --- Second invocation with the SAME run_id (simulates `workflow continue`).
    // This is the exact path C1 broke: it MUST NOT error "already exists".
    let second = run_js(&gait_dir, run_id, GATE_THEN_SHELL_JS)
        .await
        .expect("second same-run_id invocation must resume, not error 'already exists'");
    assert_eq!(
        second,
        WorkflowOutcome::Completed,
        "resumed run must clear the gate and complete"
    );

    // The resumed run actually ran the post-gate node and finished.
    let events = WorkflowRunLog::open(&gait_dir.join("workflow-runs"), run_id)
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

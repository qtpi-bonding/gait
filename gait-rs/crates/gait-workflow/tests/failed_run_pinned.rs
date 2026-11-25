//! Pinned today-behavior: a workflow whose FIRST node fails, then is
//! continued with the same run_id (what `gait workflow continue <run_id>`
//! does — a second `run_workflow` call with the same `run_id`).
//!
//! TODAY-BEHAVIOR (observed, pinned below):
//!
//! A shell node fails in today's engine only when the shell command TIMES
//! OUT (a nonzero exit code is treated as a *successful* completion —
//! `exec_shell_node` returns `Ok` with `exit_code: Some(3)`, so `exit 3`
//! completes the node normally). Here `first` is a shell node whose command
//! (`sleep 5`) exceeds its 50ms timeout, so it genuinely fails, with no
//! `on_error` configured (default: no retries, no continue).
//!
//! Today a failed run that is continued with the same run_id:
//!   1. FIRST run returns `Err` with the exact text
//!      `workflow aborted at node 'first': shell timed out after 50ms`
//!      (run_workflow does NOT return `Ok(WorkflowOutcome::Failed)`; there
//!      is no `Failed` variant — failure surfaces as `Err`, after the
//!      scheduler has already appended `node_error(first)` and
//!      `workflow_error(failed_node=first)` to the run log).
//!   2. The log after the first run contains, in order, exactly:
//!      `workflow_start`, `node_start(first)`, `node_error(first)`,
//!      `workflow_error(failed_node=first)`.
//!   3. The reconstructed state after the first run has
//!      `workflow_status() == "failed"` and `failed_node == Some("first")`.
//!   4. The SECOND run (continue) returns `Ok(WorkflowOutcome::Completed)`
//!      WITHOUT re-executing `first`: because a `NodeError` status is
//!      terminal, `first` counts as completed for readiness, so no second
//!      `workflow_start` and no second `node_start(first)` are appended
//!      (workflow_start is only emitted when the log was empty), and the
//!      failed node is never retried on resume.
//!   5. `second` (depends_on: [first]) DOES run on the continue: the
//!      second run appends exactly `node_start(second)`,
//!      `node_complete(second)`, `workflow_complete`.
//!   6. The final reconstructed state over the whole log reports
//!      `workflow_status() == "done"` even though `failed_node` still
//!      carries `Some("first")` from the first run (the later
//!      `workflow_complete` overwrites the status, but nothing clears
//!      `failed_node`).
//!
//! CAVEAT: the planned JS engine may change this — in particular it may
//! treat nonzero shell exit codes as failures, re-execute the failed node
//! on continue, or keep the workflow in a `failed` state after a resumed
//! run completes. This test pins the Rust engine's behavior as of today so
//! any such change is a conscious, visible decision, not an accident.

use gait_workflow::convert::to_proto;
use gait_workflow::run_log::WorkflowRunLog;
use gait_workflow::runner::{run_workflow, WorkflowOutcome, WorkflowRunOpts};
use gait_workflow::schema::WorkflowDef;
use gait_workflow::state::reconstruct_state;

use gait_core::proto::gait::workflow::v1::workflow_event;
use gait_workflow::observer::CliObserver;
use tempfile::TempDir;

/// A workflow whose first shell node fails by exceeding its own timeout
/// (50ms against a 5s sleep — the only offline way a shell node fails in
/// today's engine), and a dependent node that must only run via continue.
const FAIL_FIRST_YAML: &str = r#"
name: fail-first-test
nodes:
  - id: first
    shell:
      run: "sleep 5"
      timeout: 50
  - id: second
    depends_on: [first]
    shell: "echo second"
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

/// A stable, human-readable name for an event's kind, used for the
/// exact-sequence assertions below.
fn kind_name(e: &gait_core::proto::gait::workflow::v1::WorkflowEvent) -> String {
    match &e.kind {
        Some(workflow_event::Kind::WorkflowStart(_)) => "workflow_start".to_string(),
        Some(workflow_event::Kind::NodeStart(ns)) => {
            format!("node_start({})", ns.node_id)
        }
        Some(workflow_event::Kind::NodeComplete(nc)) => {
            format!("node_complete({})", nc.node_id)
        }
        Some(workflow_event::Kind::NodeSkip(ns)) => format!("node_skip({})", ns.node_id),
        Some(workflow_event::Kind::NodeError(ne)) => format!("node_error({})", ne.node_id),
        Some(workflow_event::Kind::WorkflowGate(wg)) => {
            format!("workflow_gate({})", wg.node_id)
        }
        Some(workflow_event::Kind::WorkflowError(we)) => {
            format!("workflow_error(failed_node={})", we.failed_node)
        }
        Some(workflow_event::Kind::WorkflowComplete(_)) => "workflow_complete".to_string(),
        Some(workflow_event::Kind::HookRun(_)) => "hook_run".to_string(),
        None => "<none>".to_string(),
    }
}

fn read_log(gait_dir: &std::path::Path, run_id: &str) -> Vec<String> {
    WorkflowRunLog::open(&gait_dir.join("workflow-runs"), run_id)
        .unwrap()
        .read_events()
        .unwrap()
        .iter()
        .map(kind_name)
        .collect()
}

#[tokio::test]
async fn a_failed_first_node_then_continue_skips_it_and_completes_the_rest() {
    let tmp = TempDir::new().unwrap();
    let gait_dir = tmp.path().to_path_buf();
    init_git(&gait_dir);

    let wf: WorkflowDef = serde_yaml::from_str(FAIL_FIRST_YAML).unwrap();
    let run_id = "fail-first-001".to_string();

    // --- First invocation: `first` fails (shell timeout), the run aborts.
    let mut sink = gait_core::embed::CliSink::stdio(true);
    let first = run_workflow(
        WorkflowRunOpts {
            workflow_def: to_proto(&wf),
            yaml_path: "fail-first-test.yaml".to_string(),
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
    .await;

    // (1) Failure surfaces as Err with an exact message — there is no
    //     `WorkflowOutcome::Failed` variant today.
    let err = first
        .expect_err("first run must abort because 'first' fails")
        .to_string();
    assert_eq!(
        err, "workflow aborted at node 'first': shell timed out after 50ms",
        "the abort error must name the failed node and the underlying cause"
    );

    // (2) Exact ordered event sequence after the failed run.
    let after_first = read_log(&gait_dir, &run_id);
    assert_eq!(
        after_first,
        vec![
            "workflow_start".to_string(),
            "node_start(first)".to_string(),
            "node_error(first)".to_string(),
            "workflow_error(failed_node=first)".to_string(),
        ],
        "failed run's log must be exactly start, node start, node error, workflow error"
    );

    // (3) Reconstructed state: failed.
    let state_first = reconstruct_state(
        &WorkflowRunLog::open(&gait_dir.join("workflow-runs"), &run_id)
            .unwrap()
            .read_events()
            .unwrap(),
    );
    assert_eq!(state_first.workflow_status(), "failed");
    assert_eq!(state_first.failed_node.as_deref(), Some("first"));

    // --- Second invocation with the SAME run_id (simulates `workflow continue`).
    let mut sink2 = gait_core::embed::CliSink::stdio(true);
    let second = run_workflow(
        WorkflowRunOpts {
            workflow_def: to_proto(&wf),
            yaml_path: "fail-first-test.yaml".to_string(),
            run_id: run_id.clone(),
            gait_dir: gait_dir.clone(),
            codebase: gait_dir.clone(),
            args: vec![],
            ..Default::default()
        },
        &mut sink2,
        &mut CliObserver,
        None,
    )
    .await
    .expect("continue after a failed run must not error");

    // (4) The continue COMPLETES without re-running the failed node.
    assert_eq!(
        second,
        WorkflowOutcome::Completed,
        "continue must complete by skipping the already-terminal failed node"
    );

    // (2+4+5) Exact full log after both runs: NO second workflow_start, NO
    // second node_start(first) — the failed node is not retried — and
    // `second` runs exactly once, start through complete.
    let after_second = read_log(&gait_dir, &run_id);
    assert_eq!(
        after_second,
        vec![
            "workflow_start".to_string(),
            "node_start(first)".to_string(),
            "node_error(first)".to_string(),
            "workflow_error(failed_node=first)".to_string(),
            "node_start(second)".to_string(),
            "node_complete(second)".to_string(),
            "workflow_complete".to_string(),
        ],
        "continue must only append second's execution, not re-run 'first'"
    );
    let first_starts = after_second
        .iter()
        .filter(|k| *k == "node_start(first)")
        .count();
    assert_eq!(
        first_starts, 1,
        "the failed node must never be re-started on continue"
    );

    // (6) Final reconstructed state: status flipped to done by the resumed
    // run's workflow_complete, while failed_node still remembers 'first'.
    let state_final = reconstruct_state(
        &WorkflowRunLog::open(&gait_dir.join("workflow-runs"), &run_id)
            .unwrap()
            .read_events()
            .unwrap(),
    );
    assert_eq!(state_final.workflow_status(), "done");
    assert_eq!(
        state_final.failed_node.as_deref(),
        Some("first"),
        "workflow_complete clears the status but not failed_node"
    );
}

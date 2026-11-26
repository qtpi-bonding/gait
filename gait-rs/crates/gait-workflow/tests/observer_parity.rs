//! Observer parity test: run a two-shell-node JavaScript workflow with a
//! `ChannelObserver` and assert that:
//! 1. The run-log (folded via `reconstruct_state`) reports both nodes as Done.
//! 2. The event channel received `WorkflowStart` and `WorkflowComplete` events.
//! 3. The evidence record (configured via `gait.wf.config`) captured the
//!    record commands and tree hashes.
//!
//! Shell nodes require no model, so this test runs fully offline.

use std::path::Path;

use gait_core::proto::gait::workflow::v1::workflow_event;
use gait_workflow::run_log::WorkflowRunLog;
use gait_workflow::runner::WorkflowOutcome;
use gait_workflow::script::{run_script, ScriptRunOpts};
use gait_workflow::state::{reconstruct_state, NodeStatus};
use gait_workflow::turn_exec::testing::ScriptedTurnExecutor;
use tempfile::TempDir;

/// Minimal two-node sequential shell workflow with a runtime record command.
const SCRIPT_JS: &str = r#"
await gait.wf.config({
  name: "parity-test",
  record: {
    commands: [{ id: "runtime", command: "printf record-ok" }],
  },
});
await gait.wf.shell({ id: "step1", script: "echo hello" });
await gait.wf.shell({ id: "step2", script: "echo world" });
"#;

fn init_git_repo(gait_dir: &Path) {
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

/// Set up a real git-backed gait-dir (the same shape the parity tests build)
/// so a shell workflow can create session worktrees offline.
fn setup_git_gait_dir(gait_dir: &std::path::Path) {
    std::fs::create_dir_all(gait_dir.join(".active")).unwrap();
    std::fs::create_dir_all(gait_dir.join("workflow-runs")).unwrap();
    init_git_repo(gait_dir);
}

/// Write the script and run it once through the script engine with the given
/// observer and sink.
async fn run_js_with_observer(
    tmp: &TempDir,
    run_id: &str,
    source: &str,
    observer: &mut dyn gait_workflow::observer::WorkflowObserver,
    sink: &mut dyn gait_core::embed::TurnSink,
) -> anyhow::Result<WorkflowOutcome> {
    let script_path = tmp.path().join("wf.js");
    std::fs::write(&script_path, source).unwrap();
    run_script(
        ScriptRunOpts {
            script_path,
            run_id: run_id.to_string(),
            gait_dir: tmp.path().to_path_buf(),
            codebase: tmp.path().to_path_buf(),
            args: vec![],
            force_rerun_nodes: vec![],
            force: false,
            turns: std::sync::Arc::new(ScriptedTurnExecutor::new()),
        },
        sink,
        observer,
    )
    .await
}

#[tokio::test]
async fn observer_parity_shell_workflow() {
    let dir = TempDir::new().unwrap();
    let gait_dir = dir.path().to_path_buf();
    setup_git_gait_dir(&gait_dir);

    let run_id = format!(
        "parity-test-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );

    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut obs = gait_workflow::observer::ChannelObserver::new(event_tx);

    let mut sink = gait_core::embed::CliSink::stdio(true);
    let outcome = run_js_with_observer(&dir, &run_id, SCRIPT_JS, &mut obs, &mut sink)
        .await
        .unwrap();

    assert_eq!(outcome, WorkflowOutcome::Completed);

    let record: serde_json::Value = serde_json::from_slice(
        &std::fs::read(
            gait_dir
                .join("workflow-runs")
                .join(&run_id)
                .join("record.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(record["workflow_status"], "completed");
    assert_eq!(record["commands"][0]["id"], "runtime");
    assert_eq!(record["commands"][0]["stdout"], "record-ok");
    assert_eq!(record["commands"][0]["exit_code"], 0);
    assert_ne!(record["initial_tree_hash"], "");
    assert_ne!(record["final_tree_hash"], "");

    // ── Fold the run-log and assert node statuses ──────────────────────────
    let workflow_runs_dir = gait_dir.join("workflow-runs");
    let log = WorkflowRunLog::open(&workflow_runs_dir, &run_id).unwrap();
    let events = log.read_events().unwrap();
    let state = reconstruct_state(&events);

    assert_eq!(
        state.node_status("step1"),
        NodeStatus::Done,
        "step1 should be Done"
    );
    assert_eq!(
        state.node_status("step2"),
        NodeStatus::Done,
        "step2 should be Done"
    );

    // ── Assert event channel saw WorkflowStart and WorkflowComplete ────────
    let mut saw_start = false;
    let mut saw_complete = false;
    while let Ok(ev) = event_rx.try_recv() {
        match &ev.kind {
            Some(workflow_event::Kind::WorkflowStart(_)) => saw_start = true,
            Some(workflow_event::Kind::WorkflowComplete(_)) => saw_complete = true,
            _ => {}
        }
    }
    assert!(saw_start, "event channel should have seen WorkflowStart");
    assert!(
        saw_complete,
        "event channel should have seen WorkflowComplete"
    );
}

/// Orchestrator regression: `run_script` accepts a `RecordingSink` and drives
/// a real (offline, shell) workflow to completion with both nodes Done. This
/// exercises the `sink` parameter and the event drain loop end-to-end.
#[tokio::test]
async fn run_workflow_accepts_recording_sink() {
    use gait_core::embed::RecordingSink;
    let dir = TempDir::new().unwrap();
    let gait_dir = dir.path().to_path_buf();
    setup_git_gait_dir(&gait_dir);

    let run_id = format!("rec{}", std::process::id());
    let mut sink = RecordingSink::default();
    let outcome = run_js_with_observer(
        &dir,
        &run_id,
        SCRIPT_JS,
        &mut gait_workflow::observer::NullObserver,
        &mut sink,
    )
    .await
    .expect("run should succeed");
    assert_eq!(outcome, WorkflowOutcome::Completed);

    let log = WorkflowRunLog::open(&gait_dir.join("workflow-runs"), &run_id).unwrap();
    let state = reconstruct_state(&log.read_events().unwrap());
    assert_eq!(state.node_status("step1"), NodeStatus::Done);
    assert_eq!(state.node_status("step2"), NodeStatus::Done);
}

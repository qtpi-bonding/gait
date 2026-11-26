//! Task JS-5b (edit-only, red step): resume edge cases — continuing a
//! COMPLETED run must not append a second WorkflowComplete, and a run that
//! failed before `gait.wf.config` even ran can be continued with a corrected
//! source.

use std::path::Path;
use std::sync::Arc;

use gait_core::proto::gait::workflow::v1::workflow_event;
use gait_workflow::outcome::WorkflowOutcome;
use gait_workflow::run_log::WorkflowRunLog;
use gait_workflow::script::{run_script, ScriptRunOpts};
use gait_workflow::turn_exec::testing::ScriptedTurnExecutor;
use gait_workflow::turn_exec::TurnExecutor;
use tempfile::TempDir;

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

async fn run_js_full(
    tmp: &TempDir,
    run_id: &str,
    source: &str,
    exec: Arc<dyn TurnExecutor>,
) -> anyhow::Result<WorkflowOutcome> {
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
            turns: exec,
        },
        &mut sink,
        &mut gait_workflow::observer::NullObserver,
    )
    .await
}

async fn continue_js(
    tmp: &TempDir,
    run_id: &str,
    source: &str,
    exec: Arc<dyn TurnExecutor>,
) -> anyhow::Result<WorkflowOutcome> {
    run_js_full(tmp, run_id, source, exec).await
}

fn log_events(gait_dir: &Path, run_id: &str) -> Vec<workflow_event::Kind> {
    WorkflowRunLog::open(&gait_dir.join("workflow-runs"), run_id)
        .unwrap()
        .read_events()
        .unwrap()
        .into_iter()
        .map(|ev| ev.kind.expect("event must have a kind"))
        .collect()
}

fn marker_lines(marker: &Path) -> Vec<String> {
    std::fs::read_to_string(marker)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.to_string())
        .collect()
}

fn assert_completed(res: anyhow::Result<WorkflowOutcome>, what: &str) {
    assert!(
        matches!(res, Ok(WorkflowOutcome::Completed)),
        "{what} must complete: {res:?}"
    );
}

// ── 1 ───────────────────────────────────────────────────────────────────────

/// Continuing an already-completed run is a no-op replay: no new step runs
/// and the log does NOT gain a second WorkflowComplete.
#[tokio::test]
async fn continue_of_completed_run_appends_no_second_workflow_complete() {
    let tmp = TempDir::new().unwrap();
    init_git_repo(tmp.path());
    let marker = tmp.path().join("MARKER");
    let script = serde_json::to_string(&format!("echo tick >> '{}'", marker.display())).unwrap();
    let source = format!(
        r#"
        await gait.wf.config({{ name: "t" }});
        await gait.wf.shell({{
            id: "s",
            script: {script},
        }});
    "#
    );
    let run_id = "resume-completed-001";

    let first = continue_js(&tmp, run_id, &source, Arc::new(ScriptedTurnExecutor::new())).await;
    assert_completed(first, "the first run");

    let second = continue_js(&tmp, run_id, &source, Arc::new(ScriptedTurnExecutor::new())).await;
    assert_completed(second, "the continue of a completed run");

    let events = log_events(tmp.path(), run_id);
    assert_eq!(
        events
            .iter()
            .filter(|k| matches!(k, workflow_event::Kind::WorkflowComplete(_)))
            .count(),
        1,
        "exactly one WorkflowComplete after continue of a completed run: {events:?}"
    );
    let lines = marker_lines(&marker);
    assert_eq!(lines.len(), 1, "the step must not run again: {lines:?}");
}

// ── 2 ───────────────────────────────────────────────────────────────────────

/// A run that failed before config ran (e.g. the source itself threw) can be
/// continued with a corrected source: one WorkflowStart total, then it
/// completes and the corrected steps run.
#[tokio::test]
async fn run_that_failed_before_config_can_be_continued() {
    let tmp = TempDir::new().unwrap();
    init_git_repo(tmp.path());
    let marker = tmp.path().join("MARKER");
    let script = serde_json::to_string(&format!("echo tick >> '{}'", marker.display())).unwrap();
    let bad_source = r#"
        throw new Error("boom");
    "#;
    let fixed_source = format!(
        r#"
        await gait.wf.config({{ name: "t" }});
        await gait.wf.shell({{
            id: "s",
            script: {script},
        }});
    "#
    );
    let run_id = "resume-before-config-001";

    let first = continue_js(
        &tmp,
        run_id,
        bad_source,
        Arc::new(ScriptedTurnExecutor::new()),
    )
    .await;
    assert!(
        first.is_err(),
        "the first run (throwing source) must fail: {first:?}"
    );

    let second = continue_js(
        &tmp,
        run_id,
        &fixed_source,
        Arc::new(ScriptedTurnExecutor::new()),
    )
    .await;
    assert_completed(second, "the continue with corrected source");

    let events = log_events(tmp.path(), run_id);
    assert_eq!(
        events
            .iter()
            .filter(|k| matches!(k, workflow_event::Kind::WorkflowStart(_)))
            .count(),
        1,
        "exactly one WorkflowStart across both runs: {events:?}"
    );
    let lines = marker_lines(&marker);
    assert_eq!(lines.len(), 1, "the corrected step ran once: {lines:?}");
}

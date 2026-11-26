//! Task JS-5a (edit-only, red step): tests for script-run GATES, interactive
//! steps (`gait.wf.turn({ id })` with no prompt), cancellation kinds, and the
//! crash guard. These are written BEFORE the implementation: today
//! `gait.wf.gate` and promptless `gait.wf.turn` do not exist, so the tests
//! fail at run time until the gate implementation lands. They must COMPILE
//! against the current public API.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use gait_core::proto::gait::workflow::v1::workflow_event;
use gait_core::proto::gait::workflow::v1::NodeErrorKind;
use gait_workflow::run_log::WorkflowRunLog;
use gait_workflow::runner::WorkflowOutcome;
use gait_workflow::script::{run_script, ScriptRunOpts};
use gait_workflow::turn_exec::testing::ScriptedTurnExecutor;
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
    exec: Arc<ScriptedTurnExecutor>,
    force: bool,
    force_rerun_nodes: Vec<String>,
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
            force_rerun_nodes,
            force,
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
    exec: Arc<ScriptedTurnExecutor>,
) -> anyhow::Result<WorkflowOutcome> {
    run_js_full(tmp, run_id, source, exec, false, vec![]).await
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

fn err_text(res: anyhow::Result<WorkflowOutcome>) -> String {
    match res {
        Ok(_) => panic!("expected Err"),
        Err(e) => format!("{e:#}"),
    }
}

fn truncate_log(gait_dir: &Path, run_id: &str, keep: usize) {
    let log = gait_dir
        .join("workflow-runs")
        .join(run_id)
        .join("log.jsonl");
    let text = std::fs::read_to_string(&log).unwrap();
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    assert!(
        keep <= lines.len(),
        "cannot keep {keep} of {} lines",
        lines.len()
    );
    let mut out = lines[..keep].join("\n");
    out.push('\n');
    std::fs::write(&log, out).unwrap();
}

fn marker_lines(marker: &Path) -> usize {
    std::fs::read_to_string(marker)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .count()
}

/// A shell step that appends one line to the marker file each time it is
/// actually executed. Same rule as `marker_script` in script_replay.rs: the
/// path is JSON-encoded and passed as an env var, never spliced into the JS
/// script text.
fn shell_step(id: &str, marker: &Path) -> String {
    let m = serde_json::to_string(&marker.to_string_lossy().to_string()).unwrap();
    format!(
        r#"gait.wf.shell({{
            id: "{id}",
            script: "echo tick >> \"$MARKER_PATH\"",
            env: {{ MARKER_PATH: {m} }},
        }})"#
    )
}

// ── 1 ───────────────────────────────────────────────────────────────────────

/// A gate pauses the run: earlier steps ran, later steps did not, the log has
/// NodeStart/WorkflowGate but no NodeComplete for the gate and no
/// WorkflowComplete. A plain continue (same run id, no force flags) resolves
/// the gate and completes, logging exactly one NodeComplete for the gate and
/// running the later step.
#[tokio::test]
async fn gate_pauses_and_continue_logs_gate_complete() {
    let tmp = TempDir::new().unwrap();
    init_git_repo(tmp.path());
    let marker_a = tmp.path().join("MARKER_A");
    let marker_b = tmp.path().join("MARKER_B");
    let source = format!(
        r#"
        await gait.wf.config({{ name: "t" }});
        await {};
        await gait.wf.gate({{ id: "review", message: "look" }});
        await {};
    "#,
        shell_step("before", &marker_a),
        shell_step("after", &marker_b),
    );
    let run_id = "gate-pause-001";

    let first = continue_js(&tmp, run_id, &source, Arc::new(ScriptedTurnExecutor::new())).await;
    assert!(
        matches!(first, Ok(WorkflowOutcome::GatePaused)),
        "the first run must pause at the gate: {first:?}"
    );
    assert_eq!(marker_lines(&marker_a), 1, "the step before the gate ran");
    assert_eq!(
        marker_lines(&marker_b),
        0,
        "the step after the gate did not run"
    );

    let events = log_events(tmp.path(), run_id);
    assert!(
        events
            .iter()
            .any(|k| matches!(k, workflow_event::Kind::NodeStart(ns) if ns.node_id == "review" && ns.node_type == "gate")),
        "the gate must log a NodeStart with node_type gate"
    );
    assert!(
        events.iter().any(|k| matches!(k, workflow_event::Kind::WorkflowGate(g) if g.node_id == "review" && g.message == "look")),
        "the gate must log a WorkflowGate event"
    );
    assert!(
        !events
            .iter()
            .any(|k| matches!(k, workflow_event::Kind::NodeComplete(nc) if nc.node_id == "review")),
        "the gate must not complete while paused"
    );
    assert!(
        !events
            .iter()
            .any(|k| matches!(k, workflow_event::Kind::WorkflowComplete(_))),
        "the paused run must not log WorkflowComplete"
    );

    let second = continue_js(&tmp, run_id, &source, Arc::new(ScriptedTurnExecutor::new())).await;
    assert!(
        matches!(second, Ok(WorkflowOutcome::Completed)),
        "the continue must resolve the gate and complete: {second:?}"
    );
    assert_eq!(
        marker_lines(&marker_a),
        1,
        "the replayed step must not run again"
    );
    assert_eq!(
        marker_lines(&marker_b),
        1,
        "the step after the gate must run on continue"
    );

    let events = log_events(tmp.path(), run_id);
    assert_eq!(
        events
            .iter()
            .filter(
                |k| matches!(k, workflow_event::Kind::NodeComplete(nc) if nc.node_id == "review")
            )
            .count(),
        1,
        "the continue must log exactly one NodeComplete for the gate"
    );
}

// ── 2 ───────────────────────────────────────────────────────────────────────

/// A paused run is not an exception the script can catch: the gate pause must
/// abort the script before any catch/finally body runs.
#[tokio::test]
async fn script_try_catch_cannot_swallow_pause() {
    let tmp = TempDir::new().unwrap();
    init_git_repo(tmp.path());
    let marker = tmp.path().join("MARKER");
    let m = serde_json::to_string(&marker.to_string_lossy().to_string()).unwrap();
    let source = format!(
        r#"
        await gait.wf.config({{ name: "t" }});
        try {{
            await gait.wf.gate({{ id: "g", message: "m" }});
        }} catch (e) {{
            await gait.wf.shell({{
                id: "in-catch",
                script: "echo tick >> \"$MARKER_PATH\"",
                env: {{ MARKER_PATH: {m} }},
            }});
        }} finally {{
            await gait.wf.shell({{
                id: "in-finally",
                script: "echo tick >> \"$MARKER_PATH\"",
                env: {{ MARKER_PATH: {m} }},
            }});
        }}
        await gait.wf.shell({{
            id: "after",
            script: "echo tick >> \"$MARKER_PATH\"",
            env: {{ MARKER_PATH: {m} }},
        }});
    "#
    );
    let run_id = "gate-catch-001";

    let res = continue_js(&tmp, run_id, &source, Arc::new(ScriptedTurnExecutor::new())).await;
    assert!(
        matches!(res, Ok(WorkflowOutcome::GatePaused)),
        "the gate pause must surface as GatePaused: {res:?}"
    );
    assert_eq!(
        marker_lines(&marker),
        0,
        "no handler/finally/after step may run"
    );

    let events = log_events(tmp.path(), run_id);
    for id in ["in-catch", "in-finally", "after"] {
        assert!(
            !events
                .iter()
                .any(|k| matches!(k, workflow_event::Kind::NodeStart(ns) if ns.node_id == id)),
            "no NodeStart for {id}"
        );
    }
    assert!(
        !events
            .iter()
            .any(|k| matches!(k, workflow_event::Kind::WorkflowComplete(_))),
        "a paused run must not log WorkflowComplete"
    );
    assert!(
        !events
            .iter()
            .any(|k| matches!(k, workflow_event::Kind::WorkflowError(_))),
        "a paused run must not log WorkflowError"
    );
}

// ── 3 ───────────────────────────────────────────────────────────────────────

/// Under Promise.all a gate must wait for already-running siblings to settle,
/// must not start steps that would come after the siblings, and must log the
/// WorkflowGate event after the sibling's NodeComplete.
#[tokio::test]
async fn gate_waits_for_running_siblings_and_starts_no_new_steps() {
    let tmp = TempDir::new().unwrap();
    init_git_repo(tmp.path());
    let marker_a = tmp.path().join("MARKER_A");
    let marker_b = tmp.path().join("MARKER_B");
    let a = serde_json::to_string(&marker_a.to_string_lossy().to_string()).unwrap();
    let b = serde_json::to_string(&marker_b.to_string_lossy().to_string()).unwrap();
    let source = format!(
        r#"
        await gait.wf.config({{ name: "t" }});
        await Promise.all([
            gait.wf.shell({{ id: "slow", script: "sleep 0.5 && echo slow >> \"$MARKER_A\"", env: {{ MARKER_A: {a} }} }})
                .then(() => gait.wf.shell({{ id: "after-slow", script: "echo x >> \"$MARKER_B\"", env: {{ MARKER_B: {b} }} }})),
            gait.wf.gate({{ id: "g", message: "m" }}),
        ]);
    "#
    );
    let run_id = "gate-siblings-001";

    let res = continue_js(&tmp, run_id, &source, Arc::new(ScriptedTurnExecutor::new())).await;
    assert!(
        matches!(res, Ok(WorkflowOutcome::GatePaused)),
        "the run must pause at the gate: {res:?}"
    );
    assert_eq!(marker_lines(&marker_a), 1, "the running sibling settled");
    assert_eq!(
        marker_lines(&marker_b),
        0,
        "the sibling chained after the gate must not start"
    );

    let events = log_events(tmp.path(), run_id);
    assert!(
        events
            .iter()
            .any(|k| matches!(k, workflow_event::Kind::NodeComplete(nc) if nc.node_id == "slow")),
        "the slow sibling must complete"
    );
    assert!(
        !events.iter().any(
            |k| matches!(k, workflow_event::Kind::NodeStart(ns) if ns.node_id == "after-slow")
        ),
        "the gate must not start the chained sibling"
    );
    let gate_idx = events
        .iter()
        .position(|k| matches!(k, workflow_event::Kind::WorkflowGate(g) if g.node_id == "g"))
        .expect("WorkflowGate event must be logged");
    let slow_idx = events
        .iter()
        .position(|k| matches!(k, workflow_event::Kind::NodeComplete(nc) if nc.node_id == "slow"))
        .expect("NodeComplete(slow) must be logged");
    assert!(
        gate_idx > slow_idx,
        "the gate event must come after the sibling completes"
    );
    assert!(
        !events
            .iter()
            .any(|k| matches!(k, workflow_event::Kind::NodeError(_))),
        "waiting for siblings must not cancel anything"
    );
}

// ── 4 ───────────────────────────────────────────────────────────────────────

/// A promptless `gait.wf.turn({ id })` is an interactive step: it suspends the
/// run with NodeOpen (not Completed, not GatePaused), leaves the step open in
/// the log, and a plain continue re-suspends with NodeOpen instead of tripping
/// the crash guard.
#[tokio::test]
async fn interactive_step_returns_node_open_and_is_exempt_from_guard() {
    let tmp = TempDir::new().unwrap();
    init_git_repo(tmp.path());
    let marker = tmp.path().join("MARKER");
    let source = format!(
        r#"
        await gait.wf.config({{ name: "t" }});
        await gait.wf.turn({{ id: "chat" }});
        await {};
    "#,
        shell_step("after", &marker),
    );
    let run_id = "interactive-001";

    let first = continue_js(&tmp, run_id, &source, Arc::new(ScriptedTurnExecutor::new())).await;
    assert!(
        matches!(first, Ok(WorkflowOutcome::NodeOpen)),
        "the first run must suspend with NodeOpen: {first:?}"
    );
    assert_eq!(
        marker_lines(&marker),
        0,
        "the step after the chat must not run"
    );

    let events = log_events(tmp.path(), run_id);
    assert!(
        events
            .iter()
            .any(|k| matches!(k, workflow_event::Kind::NodeStart(ns) if ns.node_id == "chat" && ns.node_type == "turn")),
        "the interactive step must log NodeStart with node_type turn"
    );
    assert!(
        !events
            .iter()
            .any(|k| matches!(k, workflow_event::Kind::NodeComplete(nc) if nc.node_id == "chat")),
        "the interactive step must not complete while suspended"
    );
    assert!(
        !events
            .iter()
            .any(|k| matches!(k, workflow_event::Kind::NodeError(ne) if ne.node_id == "chat")),
        "the interactive step must not error while suspended"
    );

    let second = continue_js(&tmp, run_id, &source, Arc::new(ScriptedTurnExecutor::new())).await;
    assert!(
        matches!(second, Ok(WorkflowOutcome::NodeOpen)),
        "a plain continue over a still-open interactive step must re-suspend with NodeOpen, not hit the crash guard: {second:?}"
    );
    assert_eq!(marker_lines(&marker), 0);
}

// ── 5 ───────────────────────────────────────────────────────────────────────

/// A turn WITH a prompt is a normal step: if it crashed (NodeStart with no
/// terminal event) a plain continue trips the crash guard and names the step
/// and the remedy. This passes already today; it pins that the interactive
/// exemption stays narrow.
#[tokio::test]
async fn crashed_prompt_turn_is_still_guarded() {
    let tmp = TempDir::new().unwrap();
    init_git_repo(tmp.path());
    let source = r#"
        await gait.wf.config({ name: "t" });
        await gait.wf.turn({ id: "t1", prompt: "do it" });
    "#;
    let run_id = "interactive-guard-001";

    let mut exec = ScriptedTurnExecutor::new();
    exec.script_text("done");
    let first = continue_js(&tmp, run_id, source, Arc::new(exec)).await;
    assert!(
        matches!(first, Ok(WorkflowOutcome::Completed)),
        "the first run must complete: {first:?}"
    );

    let events = log_events(tmp.path(), run_id);
    let cut = events
        .iter()
        .position(|k| matches!(k, workflow_event::Kind::NodeStart(ns) if ns.node_id == "t1"))
        .expect("NodeStart(t1) must be in the log");
    truncate_log(tmp.path(), run_id, cut + 1);

    let guarded = continue_js(&tmp, run_id, source, Arc::new(ScriptedTurnExecutor::new())).await;
    let text = err_text(guarded);
    assert!(
        text.contains("t1") && text.contains("--force-rerun"),
        "the guard error must name the crashed step and the remedy: {text}"
    );
}

// ── 6 ───────────────────────────────────────────────────────────────────────

/// When one step of a Promise.all fails, its still-running siblings are
/// cancelled with NodeErrorKind::Cancelled (the failing step keeps its real
/// error kind); a plain continue re-runs the cancelled sibling to completion.
#[tokio::test]
async fn failure_cancels_siblings_with_cancelled_kind_and_continue_reruns_them() {
    let tmp = TempDir::new().unwrap();
    init_git_repo(tmp.path());
    let marker_a = tmp.path().join("MARKER_A");
    let a = serde_json::to_string(&marker_a.to_string_lossy().to_string()).unwrap();
    let source = format!(
        r#"
        await gait.wf.config({{ name: "t" }});
        await Promise.all([
            gait.wf.shell({{ id: "slow", script: "sleep 2 && echo s >> \"$MARKER_A\"", env: {{ MARKER_A: {a} }} }}),
            gait.wf.turn({{ id: "bad", prompt: "x" }}),
        ]);
    "#
    );
    let run_id = "cancel-siblings-001";

    let mut exec = ScriptedTurnExecutor::new();
    exec.script_failure("boom");
    let first = continue_js(&tmp, run_id, &source, Arc::new(exec)).await;
    let _ = err_text(first);
    assert_eq!(
        marker_lines(&marker_a),
        0,
        "the cancelled sibling must not write"
    );

    let events = log_events(tmp.path(), run_id);
    let slow_err = events
        .iter()
        .find_map(|k| match k {
            workflow_event::Kind::NodeError(ne) if ne.node_id == "slow" => Some(ne),
            _ => None,
        })
        .expect("the cancelled sibling must log a NodeError");
    assert_eq!(
        slow_err.kind,
        NodeErrorKind::Cancelled as i32,
        "the sibling must be cancelled, not failed"
    );
    let bad_err = events
        .iter()
        .find_map(|k| match k {
            workflow_event::Kind::NodeError(ne) if ne.node_id == "bad" => Some(ne),
            _ => None,
        })
        .expect("the failing step must log a NodeError");
    assert_ne!(
        bad_err.kind,
        NodeErrorKind::Cancelled as i32,
        "the failing step keeps its real error kind"
    );

    let second = continue_js(&tmp, run_id, &source, Arc::new(ScriptedTurnExecutor::new())).await;
    assert!(
        matches!(second, Ok(WorkflowOutcome::Completed)),
        "the continue must re-run the cancelled sibling and complete: {second:?}"
    );
    assert_eq!(marker_lines(&marker_a), 1, "the re-run sibling must write");
}

// ── 7 ───────────────────────────────────────────────────────────────────────

/// A step created but never awaited before the script ends is an error, and
/// that orphan step is cancelled.
#[tokio::test]
async fn unawaited_step_at_script_end_is_an_error_and_cancelled() {
    let tmp = TempDir::new().unwrap();
    init_git_repo(tmp.path());
    let marker_a = tmp.path().join("MARKER_A");
    let a = serde_json::to_string(&marker_a.to_string_lossy().to_string()).unwrap();
    let source = format!(
        r#"
        await gait.wf.config({{ name: "t" }});
        gait.wf.shell({{ id: "orphan", script: "sleep 2 && echo o >> \"$MARKER_A\"", env: {{ MARKER_A: {a} }} }});
        await gait.wf.shell({{ id: "s", script: "true" }});
    "#
    );
    let run_id = "orphan-001";

    let res = continue_js(&tmp, run_id, &source, Arc::new(ScriptedTurnExecutor::new())).await;
    let text = err_text(res);
    assert!(
        text.contains("step orphan was not awaited"),
        "the error must name the unawaited step: {text}"
    );
    assert_eq!(marker_lines(&marker_a), 0, "the orphan must not write");

    let events = log_events(tmp.path(), run_id);
    assert!(
        events
            .iter()
            .any(|k| matches!(k, workflow_event::Kind::NodeError(ne) if ne.node_id == "orphan" && ne.kind == NodeErrorKind::Cancelled as i32)),
        "the orphan must be cancelled"
    );
    assert!(
        !events
            .iter()
            .any(|k| matches!(k, workflow_event::Kind::WorkflowComplete(_))),
        "an errored run must not log WorkflowComplete"
    );
}

// ── 8 ───────────────────────────────────────────────────────────────────────

/// A step promise whose rejection happens late but is still handled by a
/// try/catch must not fail the run: the run ends Completed.
#[tokio::test]
async fn handled_late_rejection_is_not_a_failure() {
    let tmp = TempDir::new().unwrap();
    init_git_repo(tmp.path());
    let source = r#"
        await gait.wf.config({ name: "t" });
        const p = gait.wf.turn({ id: "bad", prompt: "x" });
        await gait.wf.shell({ id: "s", script: "sleep 0.3" });
        try { await p; } catch (e) {}
    "#;
    let run_id = "late-rejection-001";

    let mut exec = ScriptedTurnExecutor::new();
    exec.script_failure("boom");
    let res = continue_js(&tmp, run_id, source, Arc::new(exec)).await;
    assert!(
        matches!(res, Ok(WorkflowOutcome::Completed)),
        "a handled late rejection must not fail the run: {res:?}"
    );
    assert!(
        log_events(tmp.path(), run_id)
            .iter()
            .any(|k| matches!(k, workflow_event::Kind::WorkflowComplete(_))),
        "the run must log WorkflowComplete"
    );
}

// ── 9 ───────────────────────────────────────────────────────────────────────

/// Ctrl-C is modelled as the caller dropping the `run_script` future
/// mid-run: the in-flight step must be recorded as cancelled in the log, and
/// a plain continue must re-run it to completion.
#[tokio::test]
async fn ctrl_c_cascade_marks_in_flight_steps_cancelled() {
    let tmp = TempDir::new().unwrap();
    init_git_repo(tmp.path());
    let marker_a = tmp.path().join("MARKER_A");
    let a = serde_json::to_string(&marker_a.to_string_lossy().to_string()).unwrap();
    let source = format!(
        r#"
        await gait.wf.config({{ name: "t" }});
        await gait.wf.shell({{ id: "long", script: "sleep 1 && echo l >> \"$MARKER_A\"", env: {{ MARKER_A: {a} }} }});
    "#
    );
    let run_id = "ctrl-c-001";
    let runs_dir = tmp.path().join("workflow-runs");

    // The poller wins (i.e. "Ctrl-C" arrives while the step is running),
    // which drops the run future.
    let poller = async {
        loop {
            if let Ok(log) = WorkflowRunLog::open(&runs_dir, run_id) {
                if let Ok(events) = log.read_events() {
                    let started = events.iter().any(
                        |ev| matches!(&ev.kind, Some(workflow_event::Kind::NodeStart(ns)) if ns.node_id == "long"),
                    );
                    if started {
                        return;
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    let run = continue_js(&tmp, run_id, &source, Arc::new(ScriptedTurnExecutor::new()));
    tokio::select! {
        _ = run => panic!("the run finished before the simulated Ctrl-C; the poller must win"),
        _ = poller => {}
    }
    tokio::time::sleep(Duration::from_millis(100)).await;

    assert_eq!(
        marker_lines(&marker_a),
        0,
        "the dropped step must not write"
    );
    let events = log_events(tmp.path(), run_id);
    assert!(
        events
            .iter()
            .any(|k| matches!(k, workflow_event::Kind::NodeError(ne) if ne.node_id == "long" && ne.kind == NodeErrorKind::Cancelled as i32)),
        "the in-flight step must be recorded as cancelled"
    );

    let second = continue_js(&tmp, run_id, &source, Arc::new(ScriptedTurnExecutor::new())).await;
    assert!(
        matches!(second, Ok(WorkflowOutcome::Completed)),
        "the continue must re-run the cancelled step and complete: {second:?}"
    );
    assert_eq!(marker_lines(&marker_a), 1, "the re-run step must write");
}

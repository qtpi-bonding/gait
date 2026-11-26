//! Task 4b (js-workflows): script-run REPLAY. These tests are written BEFORE
//! the implementation (the red step): today `run_script` on an existing run
//! id errors with "resuming script runs is not implemented yet", and
//! `gait.wf.now` / `gait.wf.random` do not exist, so every test here fails at
//! run time until the replay implementation lands. They must COMPILE against
//! the current public API.
//!
//! A "second run" / "continue" is a second call of `run_script` with the SAME
//! `run_id` and `gait_dir`.

use std::path::Path;
use std::sync::Arc;

use gait_core::proto::gait::workflow::v1::workflow_event;
use gait_workflow::outcome::WorkflowOutcome;
use gait_workflow::run_log::WorkflowRunLog;
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

/// Write `source` to `<tmp>/wf.js` and run it under `run_id`, with full
/// control over the replay-relevant opts (`force`, `force_rerun_nodes`).
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

/// Continue flavor: same run id, no force flags (the plain
/// `gait workflow continue <run_id>` case).
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

/// Overwrite the run's log.jsonl with only its first `keep` event lines —
/// simulating a crash that lost everything after the kept prefix.
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

/// A shell step that appends one line to the marker file each time it is
/// actually executed. Replayed steps must NOT execute, so the marker's line
/// count is the execution count.
fn marker_script(marker: &Path, step_id: &str) -> String {
    let m = serde_json::to_string(&marker.to_string_lossy().to_string()).unwrap();
    format!(
        r#"
        await gait.wf.config({{ name: "t" }});
        await gait.wf.shell({{
            id: "{step_id}",
            script: "echo tick >> \"$MARKER_PATH\"",
            env: {{ MARKER_PATH: {m} }},
        }});
    "#
    )
}

fn marker_lines(marker: &Path) -> usize {
    std::fs::read_to_string(marker)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .count()
}

/// A completed step replays without executing again: the marker is written
/// exactly once across two runs of the same run id, and the second run
/// succeeds.
#[tokio::test]
async fn completed_step_replays_without_running() {
    let tmp = TempDir::new().unwrap();
    init_git_repo(tmp.path());
    let marker = tmp.path().join("MARKER");
    let source = marker_script(&marker, "s1");
    let run_id = "replay-once-001";

    let first = continue_js(&tmp, run_id, &source, Arc::new(ScriptedTurnExecutor::new())).await;
    assert!(
        matches!(first, Ok(WorkflowOutcome::Completed)),
        "first run must complete"
    );
    assert_eq!(marker_lines(&marker), 1, "first run writes once");

    let second = continue_js(&tmp, run_id, &source, Arc::new(ScriptedTurnExecutor::new())).await;
    assert!(
        matches!(second, Ok(WorkflowOutcome::Completed)),
        "second run must complete, not error with 'resuming script runs is not implemented yet'"
    );
    assert_eq!(
        marker_lines(&marker),
        1,
        "the replayed step must not execute again"
    );
}

/// Two steps settle in either order under Promise.all; a continue must accept
/// a settle order that differs from the script's textual order and must not
/// report a "script changed" drift.
#[tokio::test]
async fn parallel_steps_replay_in_any_settle_order() {
    let tmp = TempDir::new().unwrap();
    init_git_repo(tmp.path());
    let source = r#"
        await gait.wf.config({ name: "t" });
        await Promise.all([
            gait.wf.shell({ id: "slow", script: "sleep 0.3 && echo one" }),
            gait.wf.shell({ id: "fast", script: "echo two" }),
        ]);
    "#;
    let run_id = "replay-par-001";

    let first = continue_js(&tmp, run_id, source, Arc::new(ScriptedTurnExecutor::new())).await;
    assert!(matches!(first, Ok(WorkflowOutcome::Completed)));

    // On run one the log's completion order is fast then slow (sleep 0.3
    // loses the race). A replay must accept that order regardless of which
    // step the script awaits first.
    let events = log_events(tmp.path(), run_id);
    let completes: Vec<&str> = events
        .iter()
        .filter_map(|k| match k {
            workflow_event::Kind::NodeComplete(nc) => Some(nc.node_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        completes.first(),
        Some(&"fast"),
        "run one must settle fast before slow for this test to be meaningful: {completes:?}"
    );

    let second = continue_js(&tmp, run_id, source, Arc::new(ScriptedTurnExecutor::new())).await;
    match second {
        Ok(WorkflowOutcome::Completed) => {}
        Err(e) => {
            let text = format!("{e:#}");
            assert!(
                !text.contains("script changed"),
                "replay must not report drift for a merely reordered settle: {text}"
            );
            panic!("second run must succeed: {text}");
        }
        other => panic!("second run must succeed, got unexpected outcome: {other:?}"),
    }
}

/// A completed step whose KIND changed between the LOGGED source and the
/// source used for the continue (shell -> turn) must fail the continue,
/// naming the step. `continue` always runs the LOGGED source, so the drift
/// check passes (disk file untouched); the kind change must still be caught
/// against the journal.
#[tokio::test]
async fn changed_kind_for_same_id_fails() {
    let tmp = TempDir::new().unwrap();
    init_git_repo(tmp.path());
    // The step id is long and unique so it cannot appear in the run id or in
    // unrelated error text; "a" alone would be too weak a probe.
    let original = r#"
        await gait.wf.config({ name: "t" });
        await gait.wf.shell({ id: "step-kind-changed", argv: ["true"] });
    "#;
    let run_id = "replay-kind-001";

    let first = continue_js(
        &tmp,
        run_id,
        original,
        Arc::new(ScriptedTurnExecutor::new()),
    )
    .await;
    assert!(matches!(first, Ok(WorkflowOutcome::Completed)));

    // Edit the run log so the LOGGED source uses a turn for the same id.
    // JSON-escape each JS line the way serde_json would (strip the wrapping
    // quotes), then swap the logged shell line for the turn line.
    let logged_shell = r#"await gait.wf.shell({ id: "step-kind-changed", argv: ["true"] });"#;
    let logged_turn = r#"await gait.wf.turn({ id: "step-kind-changed", prompt: "now a turn" });"#;
    fn escaped(s: &str) -> String {
        let q = serde_json::to_string(s).unwrap();
        q[1..q.len() - 1].to_string()
    }
    let log_path = tmp
        .path()
        .join("workflow-runs")
        .join(run_id)
        .join("log.jsonl");
    let log_text = std::fs::read_to_string(&log_path).unwrap();
    assert!(
        log_text.contains(&escaped(logged_shell)),
        "logged source line must be present in log.jsonl"
    );
    let new_log_text = log_text.replacen(&escaped(logged_shell), &escaped(logged_turn), 1);
    assert_ne!(
        new_log_text, log_text,
        "the log edit must actually replace the line"
    );
    std::fs::write(&log_path, new_log_text).unwrap();

    // Disk wf.js still matches the (edited) logged source, so no drift
    // refusal — the failure must come from kind-change detection alone.
    let second = continue_js(
        &tmp,
        run_id,
        original,
        Arc::new(ScriptedTurnExecutor::new()),
    )
    .await;
    let text = err_text(second);
    assert!(
        text.contains("workflow script changed (step step-kind-changed)"),
        "the error must be the kind-change failure: {text}"
    );
}

/// A crash that leaves a NodeStart with no terminal event trips a guard on
/// continue; `force_rerun_nodes = [that id]` clears the guard and the step
/// runs again.
#[tokio::test]
async fn crashed_step_trips_guard_and_force_rerun_clears_it() {
    let tmp = TempDir::new().unwrap();
    init_git_repo(tmp.path());
    let marker = tmp.path().join("MARKER");
    // Same rule as `marker_script`: never splice a path into a JS string;
    // pass it as an env var instead.
    let m = serde_json::to_string(&marker.to_string_lossy().to_string()).unwrap();
    let source = format!(
        r#"
        await gait.wf.config({{ name: "t" }});
        await gait.wf.shell({{ id: "done-step", argv: ["true"] }});
        await gait.wf.shell({{
            id: "crashed",
            script: "echo tick >> \"$MARKER_PATH\"",
            env: {{ MARKER_PATH: {m} }},
        }});
    "#
    );
    let run_id = "replay-crash-001";

    let first = continue_js(&tmp, run_id, &source, Arc::new(ScriptedTurnExecutor::new())).await;
    assert!(matches!(first, Ok(WorkflowOutcome::Completed)));

    // Truncate the log right after NodeStart(crashed): drop its
    // NodeComplete, the later events, and workflow_complete.
    let events = log_events(tmp.path(), run_id);
    let cut = events
        .iter()
        .position(|k| matches!(k, workflow_event::Kind::NodeStart(ns) if ns.node_id == "crashed"))
        .expect("NodeStart(crashed) must be in the log");
    truncate_log(tmp.path(), run_id, cut + 1);
    assert_eq!(
        marker_lines(&marker),
        1,
        "the crashed step ran exactly once before the simulated crash"
    );

    // Plain continue refuses: the open node has no terminal event.
    let guarded = continue_js(&tmp, run_id, &source, Arc::new(ScriptedTurnExecutor::new())).await;
    let text = err_text(guarded);
    assert!(
        text.contains("crashed"),
        "the guard error must name the crashed step: {text}"
    );

    // force_rerun_nodes clears the guard and re-executes the step.
    let second = run_js_full(
        &tmp,
        run_id,
        &source,
        Arc::new(ScriptedTurnExecutor::new()),
        false,
        vec!["crashed".to_string()],
    )
    .await;
    assert!(
        matches!(second, Ok(WorkflowOutcome::Completed)),
        "force_rerun_nodes continue must succeed"
    );
    assert_eq!(
        marker_lines(&marker),
        2,
        "the force-rerun step must have executed a second time"
    );
}

/// A changed script file is refused on continue (drift); with `force = true`
/// the run uses the LOGGED source, proven by an observable difference
/// between the two sources.
#[tokio::test]
async fn drift_refused_and_force_uses_logged_source() {
    let tmp = TempDir::new().unwrap();
    init_git_repo(tmp.path());
    let out = tmp.path().join("out.txt");
    let o = serde_json::to_string(&out.to_string_lossy().to_string()).unwrap();
    // Same rule as `marker_script`: the path goes through an env var, never
    // into the JS script text (a JSON-quoted path inside a JS string literal
    // is a syntax error).
    let original = format!(
        r#"
        await gait.wf.config({{ name: "t" }});
        await gait.wf.shell({{
            id: "w",
            script: "echo v1 > \"$OUT_PATH\"",
            env: {{ OUT_PATH: {o} }},
        }});
    "#
    );
    let changed = format!(
        r#"
        await gait.wf.config({{ name: "t" }});
        await gait.wf.shell({{
            id: "w",
            script: "echo v2 > \"$OUT_PATH\"",
            env: {{ OUT_PATH: {o} }},
        }});
    "#
    );
    let run_id = "replay-drift-001";

    let first = continue_js(
        &tmp,
        run_id,
        &original,
        Arc::new(ScriptedTurnExecutor::new()),
    )
    .await;
    assert!(matches!(first, Ok(WorkflowOutcome::Completed)));
    assert_eq!(
        std::fs::read_to_string(&out).unwrap().trim(),
        "v1",
        "first run writes the original value"
    );

    // Continue with the changed file on disk: refused as drift.
    let refused = continue_js(
        &tmp,
        run_id,
        &changed,
        Arc::new(ScriptedTurnExecutor::new()),
    )
    .await;
    let text = err_text(refused);
    assert!(
        text.to_lowercase().contains("drift") || text.contains("changed"),
        "continue must be refused with a drift error: {text}"
    );

    // force = true: the run proceeds with the LOGGED (original) source.
    let forced = run_js_full(
        &tmp,
        run_id,
        &changed,
        Arc::new(ScriptedTurnExecutor::new()),
        true,
        vec![],
    )
    .await;
    assert!(
        matches!(forced, Ok(WorkflowOutcome::Completed)),
        "forced continue must succeed"
    );
    assert_eq!(
        std::fs::read_to_string(&out).unwrap().trim(),
        "v1",
        "the replayed step must run the logged source (v1), not the changed file (v2)"
    );
}

/// `gait.wf.now()` and `gait.wf.random()` are journaled: a replayed script
/// sees the SAME values it saw on the first run.
#[tokio::test]
async fn now_and_random_replay_same_values() {
    let tmp = TempDir::new().unwrap();
    init_git_repo(tmp.path());
    let source = r#"
        await gait.wf.config({ name: "t" });
        const t = await gait.wf.now();
        const r = await gait.wf.random();
        throw new Error(JSON.stringify([t, r]));
    "#;
    let run_id = "replay-now-001";

    let first = continue_js(&tmp, run_id, source, Arc::new(ScriptedTurnExecutor::new())).await;
    let text1 = err_text(first);
    let s = text1.find('[').expect("JSON array in first error");
    let e = text1.rfind(']').expect("JSON array in first error");
    let values1 = text1[s..=e].to_string();
    assert!(
        values1 != "[null,null]" && values1.len() > 4,
        "now/random must produce real values: {values1}"
    );

    let second = continue_js(&tmp, run_id, source, Arc::new(ScriptedTurnExecutor::new())).await;
    let text2 = err_text(second);
    let s2 = text2.find('[').expect("JSON array in second error");
    let e2 = text2.rfind(']').expect("JSON array in second error");
    let values2 = text2[s2..=e2].to_string();
    assert_eq!(
        values1, values2,
        "replayed now/random must return the same journaled values"
    );
}

/// Matches the pinned Rust-engine behavior in `failed_run_pinned.rs`: a
/// failed step is NOT re-run on continue, the remaining steps run, the run
/// ends Completed, and the failed step is still reported by the log.
#[tokio::test]
async fn continue_after_failed_run_matches_pinned_behavior() {
    let tmp = TempDir::new().unwrap();
    init_git_repo(tmp.path());
    // The first run is deliberately about a failing step, so its failure is
    // expected. The script text mirrors the pinned form documented in
    // tests/failed_run_pinned.rs (shell `sleep 5` with `timeout: 50`).
    let source = r#"
        await gait.wf.config({ name: "t" });
        await gait.wf.shell({ id: "first", script: "sleep 5", timeout: 50 });
        await gait.wf.shell({ id: "second", script: "echo second-ran" });
    "#;
    let run_id = "replay-fail-001";

    let first = continue_js(&tmp, run_id, source, Arc::new(ScriptedTurnExecutor::new())).await;
    let text = err_text(first);
    assert!(
        // Today's shell-timeout error is "Code Mode runtime error: shell
        // timed out after 50ms" and does NOT name the step, so only assert
        // that the first run failed on the timeout.
        text.contains("timed out"),
        "the first run must fail on the shell timeout: {text}"
    );

    let second = continue_js(&tmp, run_id, source, Arc::new(ScriptedTurnExecutor::new())).await;
    assert!(
        matches!(second, Ok(WorkflowOutcome::Completed)),
        "continue after a failed run must complete"
    );

    let events = log_events(tmp.path(), run_id);
    let first_starts = events
        .iter()
        .filter(|k| matches!(k, workflow_event::Kind::NodeStart(ns) if ns.node_id == "first"))
        .count();
    assert_eq!(
        first_starts, 1,
        "the failed step must never be re-run on continue"
    );
    assert!(
        events.iter().any(
            |k| matches!(k, workflow_event::Kind::NodeComplete(nc) if nc.node_id == "second"
                && nc.output.contains("second-ran"))
        ),
        "the remaining step must run on the continue"
    );
    assert!(
        events
            .iter()
            .any(|k| matches!(k, workflow_event::Kind::NodeError(ne) if ne.node_id == "first")),
        "the failed step must still be reported after the continue"
    );
    assert!(
        events
            .iter()
            .any(|k| matches!(k, workflow_event::Kind::WorkflowComplete(_))),
        "the continued run must end Completed"
    );
}

/// Steps replayed from the log do not consume the step cap: a continue over
/// already-completed steps finishes the remaining work instead of tripping
/// the cap.
#[tokio::test]
async fn replayed_steps_do_not_count_toward_the_step_cap() {
    let tmp = TempDir::new().unwrap();
    init_git_repo(tmp.path());
    let source = r#"
        await gait.wf.config({ name: "t", maxSteps: 2 });
        await gait.wf.shell({ id: "s1", argv: ["true"] });
        await gait.wf.shell({ id: "s2", argv: ["true"] });
        await gait.wf.shell({ id: "s3", argv: ["true"] });
    "#;
    let run_id = "replay-cap-001";

    let first = continue_js(&tmp, run_id, source, Arc::new(ScriptedTurnExecutor::new())).await;
    assert!(
        err_text(first).contains("step cap"),
        "the first run must hit the cap on s3"
    );

    let second = continue_js(&tmp, run_id, source, Arc::new(ScriptedTurnExecutor::new())).await;
    assert!(
        matches!(second, Ok(WorkflowOutcome::Completed)),
        "the continue must not count replayed steps against the cap"
    );
    assert!(
        log_events(tmp.path(), run_id)
            .iter()
            .any(|k| matches!(k, workflow_event::Kind::NodeComplete(nc) if nc.node_id == "s3")),
        "s3 must run on the continue"
    );
}

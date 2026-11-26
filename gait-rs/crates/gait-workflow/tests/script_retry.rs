//! Task JS-5b (edit-only, red step): tests for retry, onFail, step hooks,
//! config record, and shell outputSchema. Most of these fail at run time
//! until the implementation lands; they must COMPILE against today's code.

use std::path::Path;
use std::sync::{Arc, Mutex};

use gait_core::proto::gait::workflow::v1::workflow_event;
use gait_core::proto::gait::workflow::v1::NodeErrorKind;
use gait_workflow::outcome::WorkflowOutcome;
use gait_workflow::run_log::WorkflowRunLog;
use gait_workflow::script::{run_script, ScriptRunOpts};
use gait_workflow::turn_exec::testing::ScriptedTurnExecutor;
use gait_workflow::turn_exec::{TurnExecutor, TurnOutcome, TurnRequest};
use tempfile::TempDir;

/// Executor that records every `TurnRequest` before delegating to an inner
/// scripted executor.
struct Recording {
    requests: Mutex<Vec<String>>,
    inner: ScriptedTurnExecutor,
}

#[async_trait::async_trait(?Send)]
impl TurnExecutor for Recording {
    async fn run_turn(&self, req: TurnRequest) -> anyhow::Result<TurnOutcome> {
        self.requests.lock().unwrap().push(format!("{req:?}"));
        self.inner.run_turn(req).await
    }
}

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

fn err_text(res: anyhow::Result<WorkflowOutcome>) -> String {
    match res {
        Ok(_) => panic!("expected Err"),
        Err(e) => format!("{e:#}"),
    }
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

fn count_starts(events: &[workflow_event::Kind], node: &str) -> usize {
    events
        .iter()
        .filter(|k| matches!(k, workflow_event::Kind::NodeStart(ns) if ns.node_id == node))
        .count()
}

fn node_errors<'a>(
    events: &'a [workflow_event::Kind],
    node: &str,
) -> Vec<&'a gait_core::proto::gait::workflow::v1::NodeError> {
    events
        .iter()
        .filter_map(|k| match k {
            workflow_event::Kind::NodeError(ne) if ne.node_id == node => Some(ne),
            _ => None,
        })
        .collect()
}

// ── 1 ───────────────────────────────────────────────────────────────────────

/// Retry: the executor fails three times ("boom1", "boom2", "boom3"); the run
/// fails once with the LAST error, logs exactly one NodeStart (attempt 1) and
/// exactly one NodeError (kind Failed, attempt 1, error = the last failure).
#[tokio::test]
async fn retry_emits_one_error_after_exhaustion_attempt_one() {
    let tmp = TempDir::new().unwrap();
    init_git_repo(tmp.path());
    let source = r#"
        await gait.wf.config({ name: "t" });
        await gait.wf.turn({ id: "t", prompt: "x", retry: { times: 2, delay: 1 } });
    "#;
    let run_id = "retry-exhaust-001";

    let mut exec = ScriptedTurnExecutor::new();
    exec.script_failure("boom1");
    exec.script_failure("boom2");
    exec.script_failure("boom3");

    let res = continue_js(&tmp, run_id, source, Arc::new(exec)).await;
    let text = err_text(res);
    assert!(
        text.contains("boom3"),
        "the run error must carry the LAST failure: {text}"
    );

    let events = log_events(tmp.path(), run_id);
    assert_eq!(
        count_starts(&events, "t"),
        1,
        "exactly one NodeStart for t regardless of retries"
    );
    let errs = node_errors(&events, "t");
    assert_eq!(
        errs.len(),
        1,
        "exactly one NodeError for t after exhaustion: {events:?}"
    );
    assert_eq!(errs[0].attempt, 1, "the NodeError carries attempt 1");
    assert_eq!(
        errs[0].kind,
        NodeErrorKind::Failed as i32,
        "the NodeError kind must be Failed"
    );
    let err_str = errs[0].error.clone();
    assert!(
        err_str.contains("boom3"),
        "the NodeError error must contain boom3: {err_str}"
    );
}

// ── 2 ───────────────────────────────────────────────────────────────────────

/// Retry reuses one session: every retry attempt goes to the SAME session, so
/// the run records it once in NodeStart and all executor requests carry it.
#[tokio::test]
async fn retries_reuse_one_session() {
    let tmp = TempDir::new().unwrap();
    init_git_repo(tmp.path());
    let source = r#"
        await gait.wf.config({ name: "t" });
        await gait.wf.turn({ id: "t", prompt: "x", retry: { times: 2, delay: 1 } });
    "#;
    let run_id = "retry-session-001";

    let mut inner = ScriptedTurnExecutor::new();
    inner.script_failure("f1");
    inner.script_failure("f2");
    inner.script_text("ok");
    let recording = Arc::new(Recording {
        requests: Mutex::new(vec![]),
        inner,
    });

    let res = continue_js(
        &tmp,
        run_id,
        source,
        recording.clone() as Arc<dyn TurnExecutor>,
    )
    .await;
    assert_completed(res, "the run (fail, fail, ok)");

    let requests = recording.requests.lock().unwrap().clone();
    assert_eq!(
        requests.len(),
        3,
        "three executor calls: fail, fail, ok: {requests:?}"
    );

    let events = log_events(tmp.path(), run_id);
    assert_eq!(count_starts(&events, "t"), 1);
    assert_eq!(
        events
            .iter()
            .filter(|k| matches!(k, workflow_event::Kind::NodeComplete(nc) if nc.node_id == "t"))
            .count(),
        1,
        "exactly one NodeComplete for t"
    );
    assert!(
        node_errors(&events, "t").is_empty(),
        "a retried-then-succeeded step must not log NodeError"
    );

    let session_id = events
        .iter()
        .find_map(|k| match k {
            workflow_event::Kind::NodeStart(ns) if ns.node_id == "t" => Some(ns.session_id.clone()),
            _ => None,
        })
        .expect("NodeStart(t) must be logged");
    assert!(
        !session_id.is_empty(),
        "the NodeStart session_id must not be empty"
    );
    for req in &requests {
        assert!(
            req.contains(&session_id),
            "every executor request must carry the session id {session_id}: {req}"
        );
    }
    let first = &requests[0];
    for req in &requests[1..] {
        assert_eq!(
            req, first,
            "all three requests must be identical (same session, same prompt): {requests:?}"
        );
    }
}

// ── 3 ───────────────────────────────────────────────────────────────────────

/// continueSession overrides the session on EVERY retry attempt: both executor
/// requests and the logged NodeStart carry the given session id.
#[tokio::test]
async fn continue_session_is_the_session_on_every_attempt() {
    let tmp = TempDir::new().unwrap();
    init_git_repo(tmp.path());
    let source = r#"
        await gait.wf.config({ name: "t" });
        await gait.wf.turn({ id: "t", prompt: "x", continueSession: "prior-sess", retry: { times: 2, delay: 1 } });
    "#;
    let run_id = "retry-cont-session-001";

    let mut inner = ScriptedTurnExecutor::new();
    inner.script_failure("f1");
    inner.script_text("ok");
    let recording = Arc::new(Recording {
        requests: Mutex::new(vec![]),
        inner,
    });

    let res = continue_js(
        &tmp,
        run_id,
        source,
        recording.clone() as Arc<dyn TurnExecutor>,
    )
    .await;
    assert_completed(res, "the run (fail, then ok)");

    let requests = recording.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 2, "two executor calls: {requests:?}");
    for req in &requests {
        assert!(
            req.contains("prior-sess"),
            "every request must carry continueSession prior-sess: {req}"
        );
    }

    let events = log_events(tmp.path(), run_id);
    let session_id = events
        .iter()
        .find_map(|k| match k {
            workflow_event::Kind::NodeStart(ns) if ns.node_id == "t" => Some(ns.session_id.clone()),
            _ => None,
        })
        .expect("NodeStart(t) must be logged");
    assert_eq!(
        session_id, "prior-sess",
        "the logged session must be the continueSession value"
    );
}

// ── 4 ───────────────────────────────────────────────────────────────────────

/// Hooks run once per step, not once per retry attempt, and onFail "continue"
/// lets the run finish after the failure: before then error, no after (the
/// step did not succeed).
#[tokio::test]
async fn hooks_run_once_not_per_retry_attempt() {
    let tmp = TempDir::new().unwrap();
    init_git_repo(tmp.path());
    let marker = tmp.path().join("MARKER");
    let before = serde_json::to_string(&format!("echo before >> '{}'", marker.display())).unwrap();
    let error = serde_json::to_string(&format!("echo error >> '{}'", marker.display())).unwrap();
    let after = serde_json::to_string(&format!("echo after >> '{}'", marker.display())).unwrap();
    let source = format!(
        r#"
        await gait.wf.config({{ name: "t" }});
        await gait.wf.turn({{
            id: "t",
            prompt: "x",
            retry: {{ times: 2, delay: 1 }},
            onFail: "continue",
            hooks: {{
                before: {before},
                error: {error},
                after: {after},
            }},
        }});
    "#
    );
    let run_id = "hooks-once-001";

    let mut exec = ScriptedTurnExecutor::new();
    exec.script_failure("f1");
    exec.script_failure("f2");
    exec.script_failure("f3");

    let res = continue_js(&tmp, run_id, &source, Arc::new(exec)).await;
    assert_completed(res, "the run (onFail continue)");

    let lines = marker_lines(&marker);
    assert_eq!(
        lines,
        vec!["before".to_string(), "error".to_string()],
        "hooks must run once per step (before, error; no after), not per attempt: {lines:?}"
    );
}

// ── 5 ───────────────────────────────────────────────────────────────────────

/// A shell step that times out is retried: the script runs 3 times (initial +
/// 2 retries), each timing out, and the run fails with exactly one NodeError
/// for the shell (attempt 1).
#[tokio::test]
async fn shell_timeout_is_retried() {
    let tmp = TempDir::new().unwrap();
    init_git_repo(tmp.path());
    let marker = tmp.path().join("MARKER");
    let script =
        serde_json::to_string(&format!("echo a >> '{}' && sleep 5", marker.display())).unwrap();
    let source = format!(
        r#"
        await gait.wf.config({{ name: "t" }});
        await gait.wf.shell({{
            id: "s",
            script: {script},
            timeout: 500,
            retry: {{ times: 2, delay: 1 }},
        }});
    "#
    );
    let run_id = "shell-timeout-001";

    let res = continue_js(&tmp, run_id, &source, Arc::new(ScriptedTurnExecutor::new())).await;
    let _ = err_text(res);

    let lines = marker_lines(&marker);
    assert_eq!(
        lines.len(),
        3,
        "the shell script must run 3 times (initial + 2 retries): {lines:?}"
    );

    let events = log_events(tmp.path(), run_id);
    let errs = node_errors(&events, "s");
    assert_eq!(
        errs.len(),
        1,
        "exactly one NodeError for s after timeout exhaustion: {events:?}"
    );
    assert_eq!(errs[0].attempt, 1, "the NodeError carries attempt 1");
}

// ── 6 ───────────────────────────────────────────────────────────────────────

/// onFail "continue" gives the script an error object ({ error }), logs
/// NodeError with kind Continued, and the replay does NOT re-run the step.
#[tokio::test]
async fn on_fail_continue_resolves_to_error_object_and_replays_as_such() {
    let tmp = TempDir::new().unwrap();
    init_git_repo(tmp.path());
    let marker = tmp.path().join("MARKER");
    let script = serde_json::to_string(&format!("echo tick >> '{}'", marker.display())).unwrap();
    let source = format!(
        r#"
        await gait.wf.config({{ name: "t" }});
        const r = await gait.wf.turn({{ id: "t", prompt: "x", onFail: "continue" }});
        if (!r.error || !String(r.error).includes("boom")) {{
            throw new Error("bad result: " + JSON.stringify(r));
        }}
        await gait.wf.shell({{
            id: "after",
            script: {script},
        }});
    "#
    );
    let run_id = "onfail-continue-001";

    let mut exec = ScriptedTurnExecutor::new();
    exec.script_failure("boom");

    let first = continue_js(&tmp, run_id, &source, Arc::new(exec)).await;
    assert_completed(first, "the first run (failure continued)");

    let events = log_events(tmp.path(), run_id);
    let errs = node_errors(&events, "t");
    assert_eq!(errs.len(), 1, "exactly one NodeError for t: {events:?}");
    assert_eq!(
        errs[0].kind,
        NodeErrorKind::Continued as i32,
        "a continued failure must log kind Continued"
    );
    assert_eq!(marker_lines(&marker).len(), 1, "the after step ran");

    let second = continue_js(&tmp, run_id, &source, Arc::new(ScriptedTurnExecutor::new())).await;
    assert_completed(second, "the replay (nothing scripted)");
    assert_eq!(
        marker_lines(&marker).len(),
        1,
        "the replay must not re-run the after step"
    );

    let events = log_events(tmp.path(), run_id);
    assert_eq!(
        node_errors(&events, "t").len(),
        1,
        "the replay must not log a second NodeError for t"
    );
}

// ── 7 ───────────────────────────────────────────────────────────────────────

/// A nonzero shell exit is a RESULT (r.exit), not an error: no retry, the run
/// completes, and a NodeComplete with exit_code Some(3) is logged.
#[tokio::test]
async fn nonzero_shell_exit_is_a_result_not_an_error() {
    let tmp = TempDir::new().unwrap();
    init_git_repo(tmp.path());
    let marker = tmp.path().join("MARKER");
    let script =
        serde_json::to_string(&format!("echo tick >> '{}' && exit 3", marker.display())).unwrap();
    let source = format!(
        r#"
        await gait.wf.config({{ name: "t" }});
        const r = await gait.wf.shell({{
            id: "s",
            script: {script},
            retry: {{ times: 2, delay: 1 }},
        }});
        if (r.exit !== 3) throw new Error("exit " + r.exit);
    "#
    );
    let run_id = "shell-exit3-001";

    let res = continue_js(&tmp, run_id, &source, Arc::new(ScriptedTurnExecutor::new())).await;
    assert_completed(res, "the run (exit 3 is a result)");

    let lines = marker_lines(&marker);
    assert_eq!(
        lines.len(),
        1,
        "a nonzero exit must not be retried: {lines:?}"
    );

    let events = log_events(tmp.path(), run_id);
    assert!(
        events
            .iter()
            .any(|k| matches!(k, workflow_event::Kind::NodeComplete(nc) if nc.node_id == "s" && nc.exit_code == Some(3))),
        "NodeComplete(s) with exit_code Some(3) must be logged: {events:?}"
    );
    assert!(
        node_errors(&events, "s").is_empty(),
        "a nonzero exit must not log NodeError"
    );
}

// ── 8 ───────────────────────────────────────────────────────────────────────

/// Hook commands see SESSION_ID, NODE_ID and FAILED_NODE env vars; hooks run
/// once per executed step and are NOT re-run on replay.
#[tokio::test]
async fn step_hooks_get_session_and_failed_node_env_and_do_not_rerun_on_replay() {
    let tmp = TempDir::new().unwrap();
    init_git_repo(tmp.path());
    let marker = tmp.path().join("MARKER");
    let hook = |phase: &str| {
        serde_json::to_string(&format!(
            "echo \"{phase}|$SESSION_ID|$NODE_ID|$FAILED_NODE\" >> '{}'",
            marker.display()
        ))
        .unwrap()
    };
    let source = format!(
        r#"
        await gait.wf.config({{ name: "t" }});
        await gait.wf.turn({{
            id: "t",
            prompt: "x",
            onFail: "continue",
            hooks: {{
                before: {before},
                after: {after},
                error: {error},
            }},
        }});
        await gait.wf.shell({{
            id: "s",
            script: "true",
            hooks: {{
                before: {before},
                after: {after},
                error: {error},
            }},
        }});
    "#,
        before = hook("before"),
        after = hook("after"),
        error = hook("error"),
    );
    let run_id = "run-hooks";

    let mut exec = ScriptedTurnExecutor::new();
    exec.script_failure("boom");

    let first = continue_js(&tmp, run_id, &source, Arc::new(exec)).await;
    assert_completed(first, "the first run");

    let lines = marker_lines(&marker);
    assert_eq!(
        lines,
        vec![
            format!("before|{run_id}|t|"),
            format!("error|{run_id}|t|t"),
            format!("before|{run_id}|s|"),
            format!("after|{run_id}|s|"),
        ],
        "hook lines must carry SESSION_ID/NODE_ID/FAILED_NODE exactly: {lines:?}"
    );

    let second = continue_js(&tmp, run_id, &source, Arc::new(ScriptedTurnExecutor::new())).await;
    assert_completed(second, "the replay");
    let lines_after = marker_lines(&marker);
    assert_eq!(
        lines_after, lines,
        "the replay must not re-run any hook: {lines_after:?}"
    );
}

// ── 9 ───────────────────────────────────────────────────────────────────────

/// config record commands run once at the start of the workflow; record.json
/// in the run dir carries the recorded command results and the tree hashes.
#[tokio::test]
async fn record_runs_once_at_start_and_writes_record_json_with_final_tree_hash() {
    let tmp = TempDir::new().unwrap();
    init_git_repo(tmp.path());
    let marker = tmp.path().join("MARKER");
    let command =
        serde_json::to_string(&format!("echo rec >> '{}' && echo out", marker.display())).unwrap();
    let source = format!(
        r#"
        await gait.wf.config({{
            name: "t",
            record: {{
                commands: [
                    {{ id: "head", command: {command}, timeout: 10000 }},
                ],
            }},
        }});
        await gait.wf.shell({{ id: "w", script: "echo hi > newfile" }});
    "#
    );
    let run_id = "record-001";

    let first = continue_js(&tmp, run_id, &source, Arc::new(ScriptedTurnExecutor::new())).await;
    assert_completed(first, "the first run");

    let lines = marker_lines(&marker);
    assert_eq!(
        lines.len(),
        1,
        "the record command ran exactly once: {lines:?}"
    );

    let record_path = tmp
        .path()
        .join("workflow-runs")
        .join(run_id)
        .join("record.json");
    let record: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&record_path).unwrap())
            .expect("record.json must parse");

    assert_eq!(
        record["run_id"], run_id,
        "record.json must carry the run id"
    );
    assert_eq!(
        record["workflow_status"], "completed",
        "record.json must carry workflow_status completed"
    );
    let commands = record["commands"]
        .as_array()
        .expect("record.json must have commands");
    assert_eq!(commands.len(), 1, "exactly one recorded command");
    assert_eq!(commands[0]["id"], "head");
    assert_eq!(commands[0]["exit_code"], 0);
    let stdout = commands[0]["stdout"].as_str().expect("stdout is a string");
    assert!(
        stdout.contains("out"),
        "the recorded stdout must contain the echo output: {stdout}"
    );

    let initial = record["initial_tree_hash"]
        .as_str()
        .expect("initial_tree_hash is a string");
    assert!(
        !initial.is_empty(),
        "initial_tree_hash must be a non-empty string"
    );
    let final_hash = record["final_tree_hash"]
        .as_str()
        .expect("final_tree_hash is a string");
    assert_ne!(
        initial, final_hash,
        "final_tree_hash must differ (newfile was created)"
    );

    let second = continue_js(&tmp, run_id, &source, Arc::new(ScriptedTurnExecutor::new())).await;
    assert_completed(second, "the replay");
    assert_eq!(
        marker_lines(&marker).len(),
        1,
        "the replay must not re-run the record command"
    );
    let record: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&record_path).unwrap())
            .expect("record.json must still parse");
    assert_eq!(
        record["commands"].as_array().unwrap().len(),
        1,
        "the replay must not add a second recorded command"
    );
}

// ── 10 ──────────────────────────────────────────────────────────────────────

/// shell outputSchema: valid stdout parses into r.json; invalid stdout or
/// schema-violating stdout raises a JS error (NodeError), not a NodeComplete.
#[tokio::test]
async fn shell_output_schema_validates() {
    let tmp = TempDir::new().unwrap();
    init_git_repo(tmp.path());
    let source = r#"
        await gait.wf.config({ name: "t" });
        const S = { type: "object", required: ["n"], properties: { n: { type: "number" } } };
        const ok = await gait.wf.shell({
            id: "good",
            script: "printf '{\"n\":1}'",
            outputSchema: S,
        });
        if (!ok.json || ok.json.n !== 1) throw new Error("no json");
        for (const spec of [
            { id: "wrong-type", script: "printf '{\"n\":\"x\"}'" },
            { id: "not-json", script: "printf 'nope'" },
        ]) {
            let m = "";
            try {
                await gait.wf.shell({ id: spec.id, script: spec.script, outputSchema: S });
            } catch (e) { m = String(e); }
            if (!m) throw new Error("accepted " + spec.id);
        }
    "#;
    let run_id = "shell-schema-001";

    let res = continue_js(&tmp, run_id, source, Arc::new(ScriptedTurnExecutor::new())).await;
    assert_completed(res, "the run (schema failures caught in the script)");

    let events = log_events(tmp.path(), run_id);
    assert!(
        events
            .iter()
            .any(|k| matches!(k, workflow_event::Kind::NodeComplete(nc) if nc.node_id == "good")),
        "the valid shell must complete"
    );
    for id in ["wrong-type", "not-json"] {
        let errs = node_errors(&events, id);
        assert_eq!(
            errs.len(),
            1,
            "exactly one NodeError for {id} (schema violation)"
        );
        assert!(
            !events
                .iter()
                .any(|k| matches!(k, workflow_event::Kind::NodeComplete(nc) if nc.node_id == id)),
            "{id} must not log NodeComplete"
        );
    }
}

// ── 11 ──────────────────────────────────────────────────────────────────────

/// config-level model/auto/allow apply to every turn step; step-level values
/// override the config defaults.
#[tokio::test]
async fn config_defaults_apply_to_turn_steps_and_step_values_win() {
    let tmp = TempDir::new().unwrap();
    init_git_repo(tmp.path());
    let source = r#"
        await gait.wf.config({
            name: "t",
            model: "cfg-model",
            auto: true,
            allow: ["cfgtool"],
        });
        await gait.wf.turn({ id: "a", prompt: "x" });
        await gait.wf.turn({
            id: "b",
            prompt: "y",
            model: "step-model",
            auto: false,
            allow: ["steptool"],
        });
    "#;
    let run_id = "config-defaults-001";

    let mut inner = ScriptedTurnExecutor::new();
    inner.script_text("ok-a");
    inner.script_text("ok-b");
    let recording = Arc::new(Recording {
        requests: Mutex::new(vec![]),
        inner,
    });

    let res = continue_js(
        &tmp,
        run_id,
        source,
        recording.clone() as Arc<dyn TurnExecutor>,
    )
    .await;
    assert_completed(res, "the run (two turn steps)");

    let requests = recording.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 2, "two executor calls: {requests:?}");

    let first = &requests[0];
    assert!(
        first.contains("model: Some(\"cfg-model\")"),
        "request 0 uses config model: {first}"
    );
    assert!(
        first.contains("auto: Some(true)"),
        "request 0 uses config auto: {first}"
    );
    assert!(
        first.contains("cfgtool"),
        "request 0 uses config allow list: {first}"
    );

    let second = &requests[1];
    assert!(
        second.contains("model: Some(\"step-model\")"),
        "request 1 uses the step model: {second}"
    );
    assert!(
        second.contains("auto: Some(false)"),
        "request 1 uses the step auto flag: {second}"
    );
    assert!(
        second.contains("steptool"),
        "request 1 uses the step allow list: {second}"
    );
    assert!(
        !second.contains("cfg-model") && !second.contains("cfgtool"),
        "request 1 must not leak config defaults: {second}"
    );
}

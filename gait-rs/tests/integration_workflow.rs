//! End-to-end coverage of the JS script workflow engine: each test writes a
//! script into `<gait_dir>/workflows/<name>.js` and runs it through the public
//! entry point `gait_workflow::launch::run_named`, then asserts on the run log
//! under `<gait_dir>/workflow-runs/<run_id>/`.

use gait_workflow::{
    launch::{run_named, LaunchEnv},
    observer::CliObserver,
    run_log::WorkflowRunLog,
    state::{reconstruct_state, NodeStatus},
};
use tempfile::TempDir;

fn init_git_repo(dir: &std::path::Path) {
    let repo = git2::Repository::init(dir).unwrap();
    let mut cfg = repo.config().unwrap();
    cfg.set_str("user.name", "test").unwrap();
    cfg.set_str("user.email", "test@test.com").unwrap();
    let sig = git2::Signature::now("test", "test@test.com").unwrap();
    let tree_id = repo.index().unwrap().write_tree().unwrap();
    let tree = repo.find_tree(tree_id).unwrap();
    repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
        .unwrap();
    repo.find_branch("master", git2::BranchType::Local)
        .and_then(|mut b| {
            b.rename("main", false)?;
            Ok(())
        })
        .ok();
}

/// Write `source` as `<gait_dir>/workflows/<name>.js` and run it through the
/// public launch entry. Returns the run id so the test can open the run log.
async fn run_script_workflow(ws: &TempDir, name: &str, source: &str) -> String {
    let dir = ws.path().join("workflows");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(format!("{name}.js")), source).unwrap();

    let mut sink = gait_core::embed::CliSink::stdio(true);
    let mut started = |_: &str, _: &str| {};
    let (run_id, _outcome) = run_named(
        name,
        vec![],
        LaunchEnv {
            gait_dir: ws.path().to_path_buf(),
            codebase: ws.path().to_path_buf(),
            started: &mut started,
            sink: &mut sink,
            observer: &mut CliObserver,
        },
    )
    .await
    .unwrap();
    run_id
}

#[tokio::test]
async fn shell_only_workflow_runs_and_commits() {
    let ws = TempDir::new().unwrap();
    init_git_repo(ws.path());
    let session_id = run_script_workflow(
        &ws,
        "test-shell",
        r#"
        await gait.wf.config({ name: "test-shell" });
        await gait.wf.shell({ id: "greet", script: "echo hello" });
    "#,
    )
    .await;

    let run_log = WorkflowRunLog::open(&ws.path().join("workflow-runs"), &session_id).unwrap();
    let events = run_log.read_events().unwrap();
    let state = reconstruct_state(&events);
    assert_eq!(
        state.node_status("greet"),
        NodeStatus::Done,
        "greet node must be done"
    );

    // Sessions must not know workflows exist (item 9 / I1): the run owns its own
    // `run/<run_id>` branch, and NO bookkeeping `session/<run_id>` branch is created.
    let repo = git2::Repository::open(ws.path()).unwrap();
    assert!(
        repo.find_branch(&format!("run/{}", session_id), git2::BranchType::Local)
            .is_ok(),
        "run branch must exist"
    );
    assert!(
        repo.find_branch(&format!("session/{}", session_id), git2::BranchType::Local)
            .is_err(),
        "no bookkeeping session branch must be created for a workflow run"
    );
}

#[tokio::test]
async fn script_if_runs_only_the_taken_step() {
    let ws = TempDir::new().unwrap();
    init_git_repo(ws.path());
    let session_id = run_script_workflow(
        &ws,
        "script-if-test",
        r#"
        await gait.wf.config({ name: "script-if-test" });
        const check = await gait.wf.shell({ id: "check", script: "echo 0" });
        if (check.output.trim() === "0") {
            await gait.wf.shell({ id: "pass-node", script: "echo pass" });
        } else {
            await gait.wf.shell({ id: "fail-node", script: "echo fail" });
        }
    "#,
    )
    .await;

    let run_log = WorkflowRunLog::open(&ws.path().join("workflow-runs"), &session_id).unwrap();
    let events = run_log.read_events().unwrap();
    let state = reconstruct_state(&events);
    assert_eq!(
        state.node_status("pass-node"),
        NodeStatus::Done,
        "pass-node must be done"
    );
    // Scripts have no Skipped state: the untaken branch is simply never run,
    // so `fail-node` stays Pending and has no NodeStart/NodeComplete/NodeError
    // events in the run log.
    assert_eq!(
        state.node_status("fail-node"),
        NodeStatus::Pending,
        "fail-node must never have been run"
    );
    assert!(
        events.iter().all(|ev| match &ev.kind {
            Some(gait_core::proto::gait::workflow::v1::workflow_event::Kind::NodeStart(ns)) => {
                ns.node_id != "fail-node"
            }
            Some(gait_core::proto::gait::workflow::v1::workflow_event::Kind::NodeComplete(nc)) =>
                nc.node_id != "fail-node",
            Some(gait_core::proto::gait::workflow::v1::workflow_event::Kind::NodeError(ne)) => {
                ne.node_id != "fail-node"
            }
            _ => true,
        }),
        "fail-node must have no node events in the run log: {events:?}"
    );
}

/// Phase 3a proof: the run records its events through a `Recorder` + a spawned
/// `StdioSink` consumer (no `println!` to stdout). Two guarantees:
///   1. The run's own typed run log (`workflow-runs/<run_id>/log.jsonl`) carries
///      `WorkflowStart`/`WorkflowComplete` — the durable transcript of the run
///      (item 9: sessions must not know workflows exist, so this is no longer
///      the session's `log.jsonl`).
///   2. `.await.unwrap()` returns — the run drops the `Recorder` (closing the
///      channel) and joins the consumer cleanly, i.e. no deadlock on exit.
#[tokio::test]
async fn workflow_events_recorded_through_channel_no_stdout_echo() {
    let ws = TempDir::new().unwrap();
    init_git_repo(ws.path());
    let run_id = run_script_workflow(
        &ws,
        "rec-test",
        r#"
        await gait.wf.config({ name: "rec-test" });
        await gait.wf.shell({ id: "greet", script: "echo hi" });
    "#,
    )
    .await;

    let run_log = ws
        .path()
        .join("workflow-runs")
        .join(&run_id)
        .join("log.jsonl");
    let content = std::fs::read_to_string(&run_log).unwrap();
    assert!(
        content.contains("\"workflowStart\""),
        "run log must contain workflowStart: {content}"
    );
    assert!(
        content.contains("\"workflowComplete\""),
        "run log must contain workflowComplete: {content}"
    );

    // item 9: the session log must NOT carry workflow lifecycle events.
    let session_log = gait_core::session::live::log_path(ws.path(), &run_id);
    if session_log.exists() {
        let session_content = std::fs::read_to_string(&session_log).unwrap();
        assert!(
            !session_content.contains("workflow_start")
                && !session_content.contains("workflow_complete"),
            "session log must not leak workflow events: {session_content}"
        );
    }
}

#[tokio::test]
async fn smoke_parallel_workflow_completes() {
    let ws = TempDir::new().unwrap();
    init_git_repo(ws.path());
    let session_id = run_script_workflow(
        &ws,
        "smoke-parallel",
        include_str!("fixtures/workflows/smoke-parallel.js"),
    )
    .await;

    let run_log = WorkflowRunLog::open(&ws.path().join("workflow-runs"), &session_id).unwrap();
    let events = run_log.read_events().unwrap();
    let state = reconstruct_state(&events);
    assert_eq!(
        state.node_status("branch-a"),
        NodeStatus::Done,
        "branch-a must be done"
    );
    assert_eq!(
        state.node_status("branch-b"),
        NodeStatus::Done,
        "branch-b must be done"
    );
    assert_eq!(
        state.node_status("join"),
        NodeStatus::Done,
        "join must be done"
    );

    // join's NodeStart must come after BOTH branches' NodeComplete.
    let mut branch_a_complete = None;
    let mut branch_b_complete = None;
    let mut join_start = None;
    for (i, ev) in events.iter().enumerate() {
        match &ev.kind {
            Some(gait_core::proto::gait::workflow::v1::workflow_event::Kind::NodeComplete(nc))
                if nc.node_id == "branch-a" =>
            {
                branch_a_complete = Some(i);
            }
            Some(gait_core::proto::gait::workflow::v1::workflow_event::Kind::NodeComplete(nc))
                if nc.node_id == "branch-b" =>
            {
                branch_b_complete = Some(i);
            }
            Some(gait_core::proto::gait::workflow::v1::workflow_event::Kind::NodeStart(ns))
                if ns.node_id == "join" =>
            {
                join_start = Some(i);
            }
            _ => {}
        }
    }
    let (a, b, j) = (
        branch_a_complete.unwrap(),
        branch_b_complete.unwrap(),
        join_start.unwrap(),
    );
    assert!(
        j > a && j > b,
        "join must start after both branches complete"
    );
}

// Regression test for retry recovery. The flaky node's command result depends
// only on a marker file (deterministic: first attempt fails, retry succeeds),
// so the retry budget is exercised with no wall-clock/timing race, and the
// downstream `recovery` step still waits for the recovery.
#[tokio::test]
async fn retry_recovers_from_transient_failure() {
    let ws = TempDir::new().unwrap();
    init_git_repo(ws.path());
    let session_id = run_script_workflow(
        &ws,
        "smoke-retry",
        include_str!("fixtures/workflows/smoke-retry.js"),
    )
    .await;

    let log = std::fs::read_to_string(
        ws.path()
            .join("workflow-runs")
            .join(&session_id)
            .join("log.jsonl"),
    )
    .unwrap();
    assert!(
        !log.contains("\"nodeError\""),
        "a node that recovers within its retry budget must not log nodeError: {log}"
    );
    let run_log = WorkflowRunLog::open(&ws.path().join("workflow-runs"), &session_id).unwrap();
    let events = run_log.read_events().unwrap();
    let state = reconstruct_state(&events);
    assert_eq!(
        state.node_status("flaky"),
        NodeStatus::Done,
        "flaky must recover via retry and complete"
    );
    assert_eq!(
        state.node_status("recovery"),
        NodeStatus::Done,
        "recovery must run after flaky's retry-recovery completes"
    );
}

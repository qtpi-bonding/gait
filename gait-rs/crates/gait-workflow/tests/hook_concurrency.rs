//! Regression test for gait-workflow audit finding #7: a hook on one node
//! must not stall the scheduler for every sibling node. Two independent
//! shell steps each carry a before-hook that sleeps 1s. In the script engine
//! the steps are started together with `Promise.all`, so each hook runs in
//! its own pooled future; before this fix the hooks serialized to ~2s.
//! After the fix, both hooks run concurrently and the whole run completes
//! in ~1s.

use gait_workflow::observer::NullObserver;
use gait_workflow::script::{run_script, ScriptRunOpts};
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

fn run_opts(
    script_path: std::path::PathBuf,
    run_id: &str,
    gait_dir: &std::path::Path,
) -> ScriptRunOpts {
    ScriptRunOpts {
        script_path,
        run_id: run_id.to_string(),
        gait_dir: gait_dir.to_path_buf(),
        codebase: gait_dir.join("codebase"),
        args: vec![],
        force_rerun_nodes: vec![],
        force: false,
        turns: std::sync::Arc::new(
            gait_workflow::turn_exec::testing::ScriptedTurnExecutor::default(),
        ),
    }
}

#[tokio::test]
async fn independent_slow_hooks_run_concurrently_not_serialized() {
    let dir = TempDir::new().unwrap();
    let gait_dir = dir.path().to_path_buf();
    init_git_repo(&gait_dir);

    let script_path = dir.path().join("wf.js");
    std::fs::write(
        &script_path,
        r#"
await gait.wf.config({ name: "hook-concurrency" });
const slowHook = (id) =>
  gait.wf.shell({
    id,
    script: "true",
    hooks: { before: "sleep 1" },
  });
await Promise.all([slowHook("a"), slowHook("b")]);
"#,
    )
    .unwrap();

    let mut observer = NullObserver;
    let mut sink = gait_core::embed::CliSink::stdio(false);

    // Warm-up run: the first script-engine run in a test process spends
    // ~10s compiling the engine, which would otherwise be measured by the
    // timing assert below. Run a trivial script first so the timed run
    // measures only the workflow itself.
    let warmup_script_path = dir.path().join("warmup.js");
    std::fs::write(
        &warmup_script_path,
        r#"await gait.wf.config({ name: "warmup" });"#,
    )
    .unwrap();
    let warmup_outcome = run_script(
        run_opts(warmup_script_path, "hook-concurrency-warmup", &gait_dir),
        &mut sink,
        &mut observer,
    )
    .await
    .unwrap();
    assert!(
        matches!(
            warmup_outcome,
            gait_workflow::runner::WorkflowOutcome::Completed
        ),
        "warm-up run must complete: {warmup_outcome:?}"
    );

    let start = tokio::time::Instant::now();
    let outcome = run_script(
        run_opts(script_path, "hook-concurrency-01", &gait_dir),
        &mut sink,
        &mut observer,
    )
    .await
    .unwrap();
    let elapsed = start.elapsed();

    assert!(
        matches!(outcome, gait_workflow::runner::WorkflowOutcome::Completed),
        "the run must complete: {outcome:?}"
    );
    assert!(
        elapsed < std::time::Duration::from_millis(1_500),
        "two independent 1s before-hooks must run concurrently (~1s total), \
         not serialized (~2s total) — took {elapsed:?}"
    );
}

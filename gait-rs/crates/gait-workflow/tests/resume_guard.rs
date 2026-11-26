//! Integration test: a shell step left `Running` by an interrupted workflow
//! is not silently re-run on a plain script resume — the fix for audit
//! finding #2 (resume idempotency). Ported from the earlier `run_workflow` API
//! to the JS script engine (`run_script`): the interrupted log is simulated
//! by running a script and truncating its log right after a `NodeStart`, the
//! same hand-truncation pattern used in `script_gate.rs`'s
//! `crashed_prompt_turn_is_still_guarded`.

use std::path::Path;

use gait_workflow::observer::NullObserver;
use gait_workflow::outcome::WorkflowOutcome;
use gait_workflow::script::{run_script, ScriptRunOpts};
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

/// Write the workflow script for the given shell step ids and run it once.
/// Each id becomes one `gait.wf.shell` step whose script is
/// `echo {id} > /dev/null` — the output is discarded; step identity and
/// progress are recorded only through the run log's NodeStart/NodeComplete
/// events, not through anything written on disk by the steps themselves.
async fn run_js(
    tmp: &TempDir,
    run_id: &str,
    step_ids: &[&str],
    force_rerun_nodes: Vec<String>,
) -> anyhow::Result<WorkflowOutcome> {
    let mut body = String::from("await gait.wf.config({ name: \"deploy\" });\n");
    for id in step_ids {
        body.push_str(&format!(
            "await gait.wf.shell({{ id: \"{id}\", script: \"echo {id} > /dev/null\" }});\n"
        ));
    }
    let script_path = tmp.path().join("wf.js");
    std::fs::write(&script_path, body).unwrap();
    let mut sink = gait_core::embed::CliSink::stdio(false);
    run_script(
        ScriptRunOpts {
            script_path,
            run_id: run_id.to_string(),
            gait_dir: tmp.path().to_path_buf(),
            codebase: tmp.path().to_path_buf(),
            args: vec![],
            force_rerun_nodes,
            force: false,
            turns: std::sync::Arc::new(
                gait_workflow::turn_exec::testing::ScriptedTurnExecutor::new(),
            ),
        },
        &mut sink,
        &mut NullObserver,
    )
    .await
}

/// Truncate the run log so that only the given number of events remain,
/// simulating a process killed mid-execution (a `NodeStart` with no matching
/// completion event). Same helper as `script_gate.rs`.
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

/// Run the script to completion, then cut the log right after the `NodeStart`
/// of `stuck_node`, leaving that step open in the log.
async fn write_interrupted_run_log(
    tmp: &TempDir,
    run_id: &str,
    step_ids: &[&str],
    stuck_node: &str,
) {
    let first = run_js(tmp, run_id, step_ids, vec![]).await;
    assert!(
        matches!(first, Ok(WorkflowOutcome::Completed)),
        "the seed run must complete: {first:?}"
    );
    let text = std::fs::read_to_string(
        tmp.path()
            .join("workflow-runs")
            .join(run_id)
            .join("log.jsonl"),
    )
    .unwrap();
    let cut = text
        .lines()
        .position(|l| {
            l.contains("\"nodeStart\"") && l.contains(&format!("\"nodeId\":\"{stuck_node}\""))
        })
        .unwrap_or_else(|| panic!("log must contain a node_start for {stuck_node}"));
    truncate_log(tmp.path(), run_id, cut + 1);
}

/// Run two shell steps concurrently (so both are in-flight together), then
/// cut the log right after the SECOND `NodeStart`, before either node
/// completes — leaving BOTH steps open, simulating a parallel crash.
async fn write_parallel_interrupted_run_log(tmp: &TempDir, run_id: &str, step_ids: [&str; 2]) {
    let arms = step_ids
        .iter()
        .map(|id| format!("gait.wf.shell({{ id: \"{id}\", script: \"echo {id} > /dev/null\" }})"))
        .collect::<Vec<_>>()
        .join(", ");
    let body =
        format!("await gait.wf.config({{ name: \"deploy\" }});\nawait Promise.all([{arms}]);\n");
    let script_path = tmp.path().join("wf.js");
    std::fs::write(&script_path, body).unwrap();
    let mut sink = gait_core::embed::CliSink::stdio(false);
    let first = run_script(
        ScriptRunOpts {
            script_path,
            run_id: run_id.to_string(),
            gait_dir: tmp.path().to_path_buf(),
            codebase: tmp.path().to_path_buf(),
            args: vec![],
            force_rerun_nodes: vec![],
            force: false,
            turns: std::sync::Arc::new(
                gait_workflow::turn_exec::testing::ScriptedTurnExecutor::new(),
            ),
        },
        &mut sink,
        &mut NullObserver,
    )
    .await;
    assert!(
        matches!(first, Ok(WorkflowOutcome::Completed)),
        "the seed run must complete: {first:?}"
    );

    let log_path = tmp
        .path()
        .join("workflow-runs")
        .join(run_id)
        .join("log.jsonl");
    let text = std::fs::read_to_string(&log_path).unwrap();
    let mut node_start_positions = text
        .lines()
        .enumerate()
        .filter(|(_, l)| l.contains("\"nodeStart\""))
        .map(|(i, l)| {
            let id = step_ids
                .iter()
                .find(|id| l.contains(&format!("\"nodeId\":\"{id}\"")))
                .copied()
                .unwrap_or_else(|| panic!("nodeStart without a known nodeId: {l}"));
            (id, i)
        })
        .collect::<Vec<_>>();
    node_start_positions.sort_by_key(|(_, i)| *i);
    assert_eq!(
        node_start_positions.len(),
        2,
        "expected nodeStarts for both steps, got: {node_start_positions:?}"
    );
    assert_eq!(
        node_start_positions
            .iter()
            .map(|(id, _)| *id)
            .collect::<Vec<_>>(),
        step_ids.to_vec(),
        "both steps must have started"
    );

    // Cut right after the second nodeStart, before any completion event.
    let second_start = node_start_positions[1].1;
    let kept = text.lines().take(second_start + 1).collect::<Vec<_>>();
    for id in step_ids {
        assert!(
            kept.iter().any(|l| l.contains("\"nodeStart\"") && l.contains(&format!("\"nodeId\":\"{id}\""))),
            "kept log must contain a nodeStart for {id}"
        );
        assert!(
            !kept
                .iter()
                .any(|l| l.contains("\"nodeComplete\"")
                    && l.contains(&format!("\"nodeId\":\"{id}\""))),
            "kept log must not contain a completion for {id}"
        );
    }
    let mut out = kept.join("\n");
    out.push('\n');
    std::fs::write(&log_path, out).unwrap();
}

#[tokio::test]
async fn stuck_shell_node_is_rejected_without_force_rerun() {
    let dir = TempDir::new().unwrap();
    let gait_dir = dir.path().to_path_buf();
    init_git_repo(&gait_dir);

    let run_id = "resume-guard-01";
    write_interrupted_run_log(&dir, run_id, &["a"], "a").await;

    let result = run_js(&dir, run_id, &["a"], vec![]).await;

    assert!(
        result.is_err(),
        "stuck shell node must be rejected, got: {result:?}"
    );
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("'a'"),
        "error should name the stuck node, got: {msg}"
    );
    assert!(
        msg.contains("--force-rerun"),
        "error should mention the escape hatch, got: {msg}"
    );
}

#[tokio::test]
async fn stuck_shell_node_reruns_with_force_rerun() {
    let dir = TempDir::new().unwrap();
    let gait_dir = dir.path().to_path_buf();
    init_git_repo(&gait_dir);

    let run_id = "resume-guard-02";
    write_interrupted_run_log(&dir, run_id, &["a"], "a").await;

    let result = run_js(&dir, run_id, &["a"], vec!["a".to_string()]).await;

    assert!(
        matches!(result, Ok(WorkflowOutcome::Completed)),
        "force-rerun must let the workflow proceed, got: {result:?}"
    );
}

#[tokio::test]
async fn naming_only_one_of_two_stuck_nodes_still_rejects_and_lists_both() {
    let dir = TempDir::new().unwrap();
    let gait_dir = dir.path().to_path_buf();
    init_git_repo(&gait_dir);

    let run_id = "resume-guard-03";
    // Two independent shell steps both left Running — simulates a
    // parallel-crash scenario.
    // Both steps must be open simultaneously: run them concurrently and cut
    // the log right after the second nodeStart.
    write_parallel_interrupted_run_log(&dir, run_id, ["a", "b"]).await;

    // Only "a" is force-named; "b" stays stuck.
    let result = run_js(&dir, run_id, &["a", "b"], vec!["a".to_string()]).await;

    assert!(
        result.is_err(),
        "partial consent must still reject, got: {result:?}"
    );
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("'a'") && msg.contains("'b'"),
        "error should name both stuck nodes, got: {msg}"
    );
}

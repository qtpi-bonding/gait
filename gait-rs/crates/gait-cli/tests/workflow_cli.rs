//! `gait workflow run|continue|abort|list` wiring through
//! `gait_workflow::launch` — script workflow runs, drift handling, and log
//! round-trips, driven directly through the CLI command functions.

use std::path::Path;

use gait_cli::cli::workflow::{
    cmd_workflow_abort, cmd_workflow_continue, cmd_workflow_run, cmd_workflow_types,
    workflow_list_json,
};

/// A temp gait_dir with a git repo (main branch, one empty commit) — copied
/// from `crates/gait-workflow/tests/script_basic.rs`.
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

/// The script engine expects a `codebase` directory inside the gait_dir (the
/// script helper from script_basic.rs passes the gait_dir itself as the
/// codebase, so this mirrors that setup). Shell steps execute in the run
/// worktree, not here — this is just part of the expected directory layout.
fn init_codebase(gait_dir: &Path) {
    std::fs::create_dir_all(gait_dir.join("codebase")).unwrap();
}

/// Run ids from `workflow_list_json` (order as listed).
fn run_ids(gait_dir: &Path) -> Vec<String> {
    workflow_list_json(gait_dir)["runs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["run_id"].as_str().unwrap().to_string())
        .collect()
}

/// A corrupt (non-tail) line hard-errors the abort instead of being
/// swallowed, and the corrupt log file is left byte-identical.
#[test]
fn read_errors_are_not_swallowed_by_abort() {
    let td = tempfile::TempDir::new().unwrap();
    let gait_dir = td.path().to_path_buf();
    let run_dir = gait_dir.join("workflow-runs").join("mid-corrupt-run");
    std::fs::create_dir_all(&run_dir).unwrap();
    // Line 1 is corrupt garbage; line 2 is a valid-looking event. Because the
    // corrupt line is NOT last, read_events must hard-error (not self-heal).
    std::fs::write(
        run_dir.join("log.jsonl"),
        "{\"version\":1,garbage\n{\"version\":1,\"id\":\"b\",\"seq\":2,\"kind\":null}\n",
    )
    .unwrap();

    let before = std::fs::read(run_dir.join("log.jsonl")).unwrap();
    let err = cmd_workflow_abort(&["mid-corrupt-run".to_string()], gait_dir.clone(), false)
        .expect_err("abort of a corrupt (non-tail) log must be an error, not Ok");
    assert_eq!(
        std::fs::read(run_dir.join("log.jsonl")).unwrap(),
        before,
        "a failed abort must leave the corrupt log byte-identical (no error event appended)"
    );
    let message = format!("{err:#}");
    assert!(
        message.contains("corrupt workflow run log"),
        "abort surfaces the read error instead of swallowing it, got: {message}"
    );
}

fn write_script(gait_dir: &Path, name: &str, marker_file: &Path, marker_text: &str) {
    std::fs::create_dir_all(gait_dir.join("workflows")).unwrap();
    // `script` runs as `sh -c <script>`, so write the marker from there. The
    // step object is embedded with serde_json::to_string so all quoting
    // (marker text, paths with spaces) is correct.
    let step = serde_json::json!({
        "id": "mark",
        "script": format!("printf %s \"$MARKER\" > '{}'", marker_file.display()),
        "env": { "MARKER": marker_text },
    });
    std::fs::write(
        gait_dir.join("workflows").join(format!("{name}.js")),
        format!(
            "await gait.wf.config({{ name: {} }});\nawait gait.wf.shell({});\n",
            serde_json::to_string(name).unwrap(),
            serde_json::to_string(&step).unwrap(),
        ),
    )
    .unwrap();
}

/// `gait workflow types` writes the generated `gait.d.ts` into
/// `<gait_dir>/workflows/` (creating the directory on a fresh gait dir) and
/// the file matches `render_dts()` exactly.
#[test]
fn workflow_types_writes_dts_into_workflows_dir() {
    let td = tempfile::TempDir::new().unwrap();
    let gait_dir = td.path().to_path_buf();
    assert!(!gait_dir.join("workflows").exists(), "fresh gait dir");

    cmd_workflow_types(&[], gait_dir.clone(), false).expect("types succeeds");

    let written = std::fs::read_to_string(gait_dir.join("workflows").join("gait.d.ts"))
        .expect("gait.d.ts written");
    assert_eq!(written, gait_workflow::types::render_dts());
}

/// Only script workflows run: `<name>.js` runs and writes its marker; a name
/// that has only a `.yaml` file fails with `LaunchError::NotFound`; a
/// finished script run resumes successfully.
#[tokio::test]
async fn only_script_workflows_run_and_resume() {
    let td = tempfile::TempDir::new().unwrap();
    let gait_dir = td.path().to_path_buf();
    init_git_repo(&gait_dir);
    init_codebase(&gait_dir);

    let marker_x = td.path().join("marker-x");
    write_script(&gait_dir, "x", &marker_x, "from-script");
    std::fs::write(
        gait_dir.join("workflows").join("x.yaml"),
        "name: x\nnodes:\n  - id: a\n    shell: \"echo from-yaml\"\n",
    )
    .unwrap();

    cmd_workflow_run(&["x".to_string()], gait_dir.clone())
        .await
        .expect("run with both files present uses the script engine");

    let from_script = std::fs::read_to_string(&marker_x).unwrap();
    assert_eq!(from_script, "from-script");

    // YAML-only name: no engine runs it — the launch refuses with NotFound.
    std::fs::write(
        gait_dir.join("workflows").join("y.yaml"),
        "name: y\nnodes:\n  - id: a\n    shell: \"echo from-yaml\"\n",
    )
    .unwrap();
    let err = cmd_workflow_run(&["y".to_string()], gait_dir.clone())
        .await
        .expect_err("a YAML-only name is not runnable");
    assert!(
        matches!(
            coded_error::downcast_ref::<gait_workflow::launch::LaunchError>(&err),
            Some(gait_workflow::launch::LaunchError::NotFound { .. })
        ),
        "YAML-only name fails with LaunchError::NotFound, got: {err:#}"
    );

    let ids = run_ids(&gait_dir);
    let x_id = ids
        .iter()
        .find(|id| id.starts_with("x-"))
        .expect("script run id in list")
        .clone();
    cmd_workflow_continue(&[x_id], gait_dir.clone())
        .await
        .expect("continue of the finished script run succeeds");
}

/// Continuing after the script changed on disk is a drift error that names
/// `--force`; `--force` resumes the logged source and succeeds.
#[tokio::test]
async fn continue_of_changed_script_is_drift_without_force() {
    let td = tempfile::TempDir::new().unwrap();
    let gait_dir = td.path().to_path_buf();
    init_git_repo(&gait_dir);
    init_codebase(&gait_dir);

    let marker = td.path().join("marker-drift");
    write_script(&gait_dir, "d", &marker, "v1");
    cmd_workflow_run(&["d".to_string()], gait_dir.clone())
        .await
        .expect("first run completes");
    let run_id = run_ids(&gait_dir)[0].clone();

    write_script(&gait_dir, "d", &marker, "v2-changed");

    let err = cmd_workflow_continue(std::slice::from_ref(&run_id), gait_dir.clone())
        .await
        .expect_err("changed script is drift without --force");
    let text = format!("{err:#}");
    assert!(text.contains("changed"), "drift text: {text}");
    assert!(
        text.contains("--force"),
        "drift text mentions --force: {text}"
    );

    cmd_workflow_continue(&[run_id, "--force".to_string()], gait_dir.clone())
        .await
        .expect("--force resumes the logged source");
}

/// Script runs through the CLI: gate pause (exit 2) + resume, list shows the
/// runs, abort folds a run to failed, and the log records it all.
#[tokio::test]
async fn run_list_continue_abort_log_roundtrip() {
    let td = tempfile::TempDir::new().unwrap();
    let gait_dir = td.path().to_path_buf();
    init_git_repo(&gait_dir);
    init_codebase(&gait_dir);

    std::fs::create_dir_all(gait_dir.join("workflows")).unwrap();
    std::fs::write(
        gait_dir.join("workflows").join("g.js"),
        r#"
        await gait.wf.config({ name: "g" });
        await gait.wf.gate({ id: "g", message: "m" });
    "#,
    )
    .unwrap();

    let code = cmd_workflow_run(&["g".to_string()], gait_dir.clone())
        .await
        .expect("gated run pauses (not a command error)");
    assert_eq!(code, 2u8, "GatePaused exits 2");

    let ids = run_ids(&gait_dir);
    assert_eq!(ids.len(), 1, "list shows the gated run");
    let run_id = ids[0].clone();

    let code = cmd_workflow_continue(std::slice::from_ref(&run_id), gait_dir.clone())
        .await
        .expect("continue resumes through the gate");
    assert_eq!(code, 0u8);

    // A second gated run is aborted and shows as failed.
    cmd_workflow_run(&["g".to_string()], gait_dir.clone())
        .await
        .expect("second gated run");
    let second = run_ids(&gait_dir)
        .into_iter()
        .find(|id| *id != run_id)
        .expect("second run id");
    cmd_workflow_abort(std::slice::from_ref(&second), gait_dir.clone(), false)
        .expect("abort succeeds");

    let runs = workflow_list_json(&gait_dir)["runs"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(runs.len(), 2);
    assert!(
        runs.iter()
            .any(|r| r["run_id"].as_str() == Some(second.as_str())
                && r["status"].as_str() == Some("failed")),
        "aborted run shows failed: {runs:?}"
    );

    // A promptless turn is an interactive step: the script run suspends as
    // NodeOpen, which the CLI reports as exit 0.
    std::fs::write(
        gait_dir.join("workflows").join("chat.js"),
        r#"
        await gait.wf.config({ name: "chat" });
        await gait.wf.turn({ id: "chat" });
    "#,
    )
    .unwrap();
    let code = cmd_workflow_run(&["chat".to_string()], gait_dir.clone())
        .await
        .expect("turn-only script ends as a command result");
    assert_eq!(code, 0u8);
}

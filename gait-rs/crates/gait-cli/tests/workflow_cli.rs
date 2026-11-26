//! `gait workflow run|continue|abort|list` wiring through
//! `gait_workflow::launch` — engine choice (script vs YAML), drift handling,
//! and log round-trips, driven directly through the CLI command functions.

use std::path::Path;

use gait_cli::cli::workflow::{
    cmd_workflow_abort, cmd_workflow_continue, cmd_workflow_run, workflow_list_json,
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

/// The script engine is chosen whenever `<name>.js` exists — even when a
/// `<name>.yaml` also exists. A YAML-only name uses the YAML engine. The
/// engine choice is made at run time from the files present, not cached, so
/// after the YAML is deleted the finished script run still resumes as a
/// script run.
#[tokio::test]
async fn engine_chosen_by_script_source_presence() {
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

    // YAML-only name: the YAML engine runs, and its shell node writes the
    // YAML marker to prove it actually executed.
    let marker_y = td.path().join("marker-y");
    std::fs::write(
        gait_dir.join("workflows").join("y.yaml"),
        format!(
            "name: y\nnodes:\n  - id: a\n    shell: \"printf %s from-yaml > '{}'\"\n",
            marker_y.display()
        ),
    )
    .unwrap();
    cmd_workflow_run(&["y".to_string()], gait_dir.clone())
        .await
        .expect("YAML-only name runs the YAML engine");
    let from_yaml = std::fs::read_to_string(&marker_y).unwrap();
    assert_eq!(from_yaml, "from-yaml");

    // Finished script run resumes as a script run after the yaml is deleted.
    std::fs::remove_file(gait_dir.join("workflows").join("x.yaml")).unwrap();
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

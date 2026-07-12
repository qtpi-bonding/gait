//! Regression test: `gait workflow run <name> [args...] --gait-dir <path>`
//! — the exact usage README.md and docs/cli-audit.md document — used to
//! silently ignore the explicit `--gait-dir` and fall back to `$GAIT_DIR`,
//! because `WorkflowArgs.rest` is a `trailing_var_arg` field that swallows
//! every token after it starts consuming, including a later `--gait-dir`.
//! Fixed in crates/gait-cli/src/cli/workflow.rs::extract_gait_dir_from_rest.
use std::path::Path;

#[test]
fn gait_dir_trailing_after_workflow_run_args_is_still_used() {
    let ws = tempfile::TempDir::new().unwrap();
    // No `workflows/somename.yaml` under ws — we're not exercising a real
    // workflow run, only proving `cmd_workflow` resolved `gait_dir` to `ws`,
    // not `$GAIT_DIR` (which we deliberately don't set at all here).
    let exe = env!("CARGO_BIN_EXE_gait");
    let output = std::process::Command::new(exe)
        .args(["workflow", "run", "somename", "--gait-dir"])
        .arg(ws.path())
        .env_remove("GAIT_DIR")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("no gait dir"),
        "gait_dir must have been resolved from the trailing --gait-dir, not left unset: {stderr}"
    );
    let expected_path = Path::new(ws.path()).join("workflows").join("somename.yaml");
    assert!(
        stderr.contains(&expected_path.display().to_string()),
        "expected the error to reference the explicit --gait-dir path {}, got: {stderr}",
        expected_path.display()
    );
}

#[test]
fn gait_dir_equals_form_trailing_after_workflow_run_args_is_still_used() {
    let ws = tempfile::TempDir::new().unwrap();
    let exe = env!("CARGO_BIN_EXE_gait");
    let gait_dir_flag = format!("--gait-dir={}", ws.path().display());
    let output = std::process::Command::new(exe)
        .args(["workflow", "run", "somename", &gait_dir_flag])
        .env_remove("GAIT_DIR")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("no gait dir"),
        "gait_dir must have been resolved from the trailing --gait-dir=<path>: {stderr}"
    );
}

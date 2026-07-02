//! Task 3.4: `progress.yaml` must never be written. State is derived entirely
//! from the run log (`reconstruct_state`) — the legacy `progress.rs` writer
//! module has been deleted; this test guards against its reintroduction.

use gait_workflow::convert::to_proto;
use gait_workflow::observer::CliObserver;
use gait_workflow::runner::{run_workflow, WorkflowRunOpts};
use gait_workflow::schema::WorkflowDef;

use tempfile::TempDir;

const SHELL_YAML: &str = r#"
name: no-progress-yaml-test
nodes:
  - id: greet
    shell: "echo hello"
"#;

/// Recursively scan `dir` for any file named `progress.yaml`.
fn find_progress_yaml(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut hits = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.file_name().and_then(|n| n.to_str()) == Some("progress.yaml") {
                hits.push(path);
            }
        }
    }
    hits
}

/// Run a minimal single-shell-node workflow to completion and return
/// (gait_dir, run_id).
async fn run_minimal_shell_workflow() -> (std::path::PathBuf, String) {
    let tmp = TempDir::new().unwrap();
    let gait_dir = tmp.path().to_path_buf();
    // Keep the TempDir alive for the duration of the test by leaking it —
    // callers only need the path.
    std::mem::forget(tmp);

    std::process::Command::new("git")
        .args(["init", "--initial-branch=main"])
        .current_dir(&gait_dir)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["commit", "--allow-empty", "-m", "init"])
        .current_dir(&gait_dir)
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "t@t.com")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "t@t.com")
        .output()
        .unwrap();

    let wf: WorkflowDef = serde_yaml::from_str(SHELL_YAML).unwrap();
    let run_id = "test-no-progress-yaml-001".to_string();

    let mut sink = gait_core::embed::CliSink::stdio(true);
    run_workflow(
        WorkflowRunOpts {
            workflow_def: to_proto(&wf),
            yaml_path: "no-progress-yaml-test.yaml".to_string(),
            run_id: run_id.clone(),
            gait_dir: gait_dir.clone(),
            codebase: gait_dir.clone(),
            args: vec![],
        },
        &mut sink,
        &mut CliObserver,
        None,
    )
    .await
    .unwrap();

    (gait_dir, run_id)
}

#[tokio::test]
async fn no_progress_yaml_written_after_run() {
    let (gait_dir, _run_id) = run_minimal_shell_workflow().await;
    let stray = find_progress_yaml(&gait_dir);
    assert!(
        stray.is_empty(),
        "progress.yaml must never be created: {:?}",
        stray
    );
}

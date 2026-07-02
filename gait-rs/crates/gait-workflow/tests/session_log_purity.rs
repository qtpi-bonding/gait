//! Session log purity test: run a small shell workflow (with a before/after
//! hook, so the `hook_run` typed event is exercised) and assert that no
//! session transcript (`sessions/*/log.jsonl`) ever carries a raw event whose
//! type is workflow-scoped (`workflow_*`, `node_*`, or `hook_run`).
//!
//! Sessions must not know workflows exist: all workflow lifecycle events must
//! land exclusively in the typed run log (`workflow-runs/<id>/log.jsonl`),
//! never in the session transcript.

use gait_workflow::runner::{run_workflow, WorkflowOutcome, WorkflowRunOpts};
use gait_workflow::schema::WorkflowDef;

use tempfile::TempDir;

const WORKFLOW_YAML: &str = r#"
name: purity-test
nodes:
  - id: step1
    shell: "echo hello"
    hooks:
      before: "echo before-hook"
      after: "echo after-hook"
  - id: step2
    depends_on: [step1]
    shell: "echo world"
"#;

fn setup_git_gait_dir(gait_dir: &std::path::Path) {
    std::fs::create_dir_all(gait_dir.join("sessions")).unwrap();
    std::fs::create_dir_all(gait_dir.join("workflow-runs")).unwrap();
    for args in [
        vec!["init"],
        vec!["config", "user.email", "test@test.com"],
        vec!["config", "user.name", "Test"],
    ] {
        std::process::Command::new("git")
            .args(&args)
            .current_dir(gait_dir)
            .output()
            .unwrap();
    }
    std::fs::write(gait_dir.join(".gitkeep"), "").unwrap();
    for args in [vec!["add", ".gitkeep"], vec!["commit", "-m", "init"]] {
        std::process::Command::new("git")
            .args(&args)
            .current_dir(gait_dir)
            .output()
            .unwrap();
    }
}

#[tokio::test]
async fn session_log_carries_no_workflow_raws() {
    let dir = TempDir::new().unwrap();
    let gait_dir = dir.path().to_path_buf();
    setup_git_gait_dir(&gait_dir);

    let wf: WorkflowDef = serde_yaml::from_str(WORKFLOW_YAML).unwrap();
    let session = format!("purity-{}", std::process::id());

    let mut sink = gait_core::embed::CliSink::stdio(true);
    let outcome = run_workflow(
        WorkflowRunOpts {
            workflow_def: gait_workflow::convert::to_proto(&wf),
            yaml_path: "purity-test.yaml".to_string(),
            run_id: session.clone(),
            gait_dir: gait_dir.clone(),
            codebase: gait_dir.clone(),
            args: vec![],
        },
        &mut sink,
        &mut gait_workflow::observer::NullObserver,
        None,
    )
    .await
    .expect("run should succeed");
    assert_eq!(outcome, WorkflowOutcome::Completed);

    let sessions = gait_dir.join("sessions");
    for entry in std::fs::read_dir(&sessions).unwrap().flatten() {
        let log = entry.path().join("log.jsonl");
        if !log.exists() {
            continue;
        }
        for e in gait_core::proto::wire::read_events(&log).unwrap() {
            if let Some(gait_core::proto::gait::log::v1::event::Body::Payload(p)) = &e.body {
                if let Some(gait_core::proto::gait::log::v1::payload::Kind::Raw(r)) = &p.kind {
                    assert!(
                        !(r.r#type.starts_with("workflow_")
                            || r.r#type.starts_with("node_")
                            || r.r#type == "hook_run"),
                        "session log leaked workflow raw: {}",
                        r.r#type
                    );
                }
            }
        }
    }
}

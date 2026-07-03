//! "Sessions must not know workflows exist" (plan item 3 / item 9).
//!
//! After the Stage 2-3 cleanup (I1), a workflow run no longer creates any
//! bookkeeping `sessions/<run_id>/` worktree or `session/<run_id>` branch: the
//! run owns its own `workflow-runs/<run_id>/` log + `run/<run_id>` branch, and
//! only genuine PROMPT nodes mint their own per-node sessions. This test drives
//! a shell-only workflow (with before/after hooks, so `hook_run` is exercised)
//! and asserts:
//!
//!   1. NO `sessions/<run_id>/` directory is created for the run.
//!   2. NO `session/<run_id>` git branch is created for the run.
//!   3. Any session log that DOES exist carries no workflow-scoped raw event
//!      (`workflow_*`, `node_*`, `hook_run`) — the leak guard.
//!
//! Note on coverage: assertion (3) can only be exercised against a real
//! per-node session log by driving a PROMPT node, which requires a live LLM
//! (`run_loop` spawns a `gait __brain` subprocess needing an API key) — not
//! available in this offline suite. Assertions (1) and (2) are the strong,
//! now-provable form of the "sessions don't know workflows exist" guarantee and
//! fail loudly if the bookkeeping session ever comes back.

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
        vec!["init", "--initial-branch=main"],
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
async fn workflow_run_creates_no_bookkeeping_session_and_leaks_no_raws() {
    let dir = TempDir::new().unwrap();
    let gait_dir = dir.path().to_path_buf();
    setup_git_gait_dir(&gait_dir);

    let wf: WorkflowDef = serde_yaml::from_str(WORKFLOW_YAML).unwrap();
    let run_id = format!("purity-{}", std::process::id());

    let mut sink = gait_core::embed::CliSink::stdio(true);
    let outcome = run_workflow(
        WorkflowRunOpts {
            workflow_def: gait_workflow::convert::to_proto(&wf),
            yaml_path: "purity-test.yaml".to_string(),
            run_id: run_id.clone(),
            gait_dir: gait_dir.clone(),
            codebase: gait_dir.clone(),
            args: vec![],
            ..Default::default()
        },
        &mut sink,
        &mut gait_workflow::observer::NullObserver,
        None,
    )
    .await
    .expect("run should succeed");
    assert_eq!(outcome, WorkflowOutcome::Completed);

    // (1) No bookkeeping session directory for the run.
    let run_session_dir = gait_dir.join("sessions").join(&run_id);
    assert!(
        !run_session_dir.exists(),
        "workflow run must not create a bookkeeping session dir at {}",
        run_session_dir.display()
    );

    // (2) No `session/<run_id>` git branch for the run.
    let branches = std::process::Command::new("git")
        .args(["branch", "--list", "--format=%(refname:short)"])
        .current_dir(&gait_dir)
        .output()
        .unwrap();
    let branch_list = String::from_utf8_lossy(&branches.stdout);
    assert!(
        !branch_list
            .lines()
            .any(|b| b.trim() == format!("session/{run_id}")),
        "workflow run must not create a session/{run_id} branch; branches: {branch_list}"
    );

    // (3) Leak guard: any session log that exists must be free of workflow raws.
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

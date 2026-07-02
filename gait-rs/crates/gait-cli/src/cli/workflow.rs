//! `gait workflow <run|list|continue|abort|log>` commands.
//! Pulled out of gait-bin so the bin can become a thin dispatcher.

use clap::Parser;
use std::path::{Path, PathBuf};

use gait_workflow::observer::CliObserver;
use gait_workflow::run_log::WorkflowRunLog;
use gait_workflow::runner::{run_workflow, WorkflowOutcome, WorkflowRunOpts};
use gait_workflow::schema::WorkflowDef; // still needed for yaml parsing
use gait_workflow::state::reconstruct_state;

/// Read the `yaml_path` from a workflow run log.
fn read_yaml_path_from_run_log(gait_dir: &PathBuf, run_id: &str) -> String {
    let workflow_runs_dir = gait_dir.join("workflow-runs");
    let events = gait_workflow::run_log::WorkflowRunLog::open(&workflow_runs_dir, run_id)
        .and_then(|l| l.read_events())
        .unwrap_or_default();
    events
        .iter()
        .find_map(|e| {
            use gait_core::proto::gait::workflow::v1::workflow_event::Kind;
            match &e.kind {
                Some(Kind::WorkflowStart(ws)) if !ws.yaml_path.is_empty() => {
                    Some(ws.yaml_path.clone())
                }
                _ => None,
            }
        })
        .unwrap_or_default()
}

/// `gait workflow <run|list|continue|abort|log>` subcommand.
#[derive(Parser, Debug)]
#[command(no_binary_name = true)]
pub struct WorkflowArgs {
    /// Gait dir (falls back to $GAIT_DIR)
    #[arg(long, env = "GAIT_DIR")]
    pub gait_dir: Option<PathBuf>,
    /// Action: run | list | continue | abort | log
    pub action: Option<String>,
    /// Remaining args (workflow name, id, …)
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub rest: Vec<String>,
}
pub async fn cmd_workflow(args: WorkflowArgs) {
    let gait_dir: PathBuf = match args.gait_dir.clone().or_else(|| {
        std::env::var("GAIT_DIR")
            .ok()
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    }) {
        Some(d) => d,
        None => {
            eprintln!("[gait] no gait dir: pass --gait-dir <path> or set $GAIT_DIR");
            std::process::exit(1);
        }
    };
    let sub = args.action.as_deref().unwrap_or("");
    let rest: Vec<String> = args.rest.clone();
    match sub {
        "run" => cmd_workflow_run(&rest, gait_dir).await,
        "list" => cmd_workflow_list(&rest, gait_dir),
        "continue" => cmd_workflow_continue(&rest, gait_dir).await,
        "abort" => cmd_workflow_abort(&rest, gait_dir),
        "log" => cmd_workflow_log(&rest, gait_dir),
        _ => {
            eprintln!("[gait] usage: gait workflow <run|list|continue|abort|log>");
            std::process::exit(1);
        }
    }
}

pub async fn cmd_workflow_run(argv: &[String], gait_dir: PathBuf) {
    let name = match argv.first() {
        Some(n) => n.clone(),
        None => {
            eprintln!("[gait] usage: gait workflow run <name> [args...]");
            std::process::exit(1);
        }
    };
    let args: Vec<String> = argv[1..].to_vec();

    let yaml_path = gait_dir.join("workflows").join(format!("{}.yaml", name));
    let yaml_content = match std::fs::read_to_string(&yaml_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[gait] cannot read {}: {}", yaml_path.display(), e);
            std::process::exit(1);
        }
    };
    let wf: WorkflowDef = match serde_yaml::from_str(&yaml_content) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("[gait] invalid workflow YAML: {}", e);
            std::process::exit(1);
        }
    };

    let run_id = gait_workflow::convert::mint_run_id(&wf.name);
    eprintln!("[gait workflow] starting {} as session {}", name, run_id);

    let mut sink = gait_core::embed::CliSink::stdio(false);
    match run_workflow(
        WorkflowRunOpts {
            workflow_def: gait_workflow::convert::to_proto(&wf),
            yaml_path: yaml_path.to_string_lossy().to_string(),
            run_id,
            gait_dir: gait_dir.clone(),
            codebase: gait_dir.join("codebase"),
            args,
        },
        &mut sink,
        &mut CliObserver,
        None,
    )
    .await
    {
        Ok(WorkflowOutcome::Completed) => {}
        Ok(WorkflowOutcome::GatePaused) => {
            eprintln!("[gait workflow] paused at gate");
            std::process::exit(2);
        }
        Ok(WorkflowOutcome::NodeOpen) => {
            eprintln!("[gait workflow] paused at open node");
            std::process::exit(0);
        }
        Err(e) => {
            eprintln!("[gait workflow] error: {}", e);
            std::process::exit(1);
        }
    }
}

fn collect_workflow_summaries(workflow_runs_dir: &Path) -> Vec<(String, String, String)> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(workflow_runs_dir) else {
        return out;
    };
    for entry in rd.flatten() {
        if !entry.path().is_dir() {
            continue;
        }
        let Some(run_id) = entry.file_name().to_str().map(|s| s.to_string()) else {
            continue;
        };
        let Ok(log) = WorkflowRunLog::open(workflow_runs_dir, &run_id) else {
            continue;
        };
        let Ok(events) = log.read_events() else {
            continue;
        };
        if events.is_empty() {
            continue;
        }
        let state = reconstruct_state(&events);
        out.push((
            run_id,
            state.workflow_name.clone(),
            state.workflow_status().to_string(),
        ));
    }
    out
}

pub fn cmd_workflow_list(_argv: &[String], gait_dir: PathBuf) {
    let summaries = collect_workflow_summaries(&gait_dir.join("workflow-runs"));
    if summaries.is_empty() {
        println!("(no workflow runs)");
        return;
    }
    for (run_id, name, status) in summaries {
        println!("{} — {} ({})", run_id, name, status);
    }
}

pub async fn cmd_workflow_continue(argv: &[String], gait_dir: PathBuf) {
    let workflow_id = match argv.first() {
        Some(id) => id.clone(),
        None => {
            eprintln!("[gait] usage: gait workflow continue <workflow-id>");
            std::process::exit(1);
        }
    };
    let runs_dir = gait_dir.join("workflow-runs");
    let run_log = match gait_workflow::run_log::WorkflowRunLog::open(&runs_dir, &workflow_id) {
        Ok(l) => l,
        Err(_) => {
            eprintln!("[gait] no run log found for workflow {}", workflow_id);
            std::process::exit(1);
        }
    };
    let events = run_log.read_events().unwrap_or_default();
    let state = gait_workflow::state::reconstruct_state(&events);
    let yaml_path_str = read_yaml_path_from_run_log(&gait_dir, &workflow_id);

    if yaml_path_str.is_empty() {
        eprintln!("[gait] cannot find workflow YAML path in run log");
        std::process::exit(1);
    }
    let yaml_content = match std::fs::read_to_string(&yaml_path_str) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[gait] cannot read {}: {}", yaml_path_str, e);
            std::process::exit(1);
        }
    };
    let wf: WorkflowDef = match serde_yaml::from_str(&yaml_content) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("[gait] invalid workflow YAML: {}", e);
            std::process::exit(1);
        }
    };

    eprintln!(
        "[gait workflow] resuming {} ({})",
        state.workflow_name, workflow_id
    );

    let mut sink = gait_core::embed::CliSink::stdio(false);
    match run_workflow(
        WorkflowRunOpts {
            workflow_def: gait_workflow::convert::to_proto(&wf),
            yaml_path: yaml_path_str,
            run_id: workflow_id,
            gait_dir: gait_dir.clone(),
            codebase: gait_dir.join("codebase"),
            args: vec![],
        },
        &mut sink,
        &mut CliObserver,
        None,
    )
    .await
    {
        Ok(WorkflowOutcome::Completed) => {}
        Ok(WorkflowOutcome::GatePaused) => {
            eprintln!("[gait workflow] paused at gate");
            std::process::exit(2);
        }
        Ok(WorkflowOutcome::NodeOpen) => {
            eprintln!("[gait workflow] paused at open node");
            std::process::exit(0);
        }
        Err(e) => {
            eprintln!("[gait workflow] error: {}", e);
            std::process::exit(1);
        }
    }
}

pub fn cmd_workflow_abort(argv: &[String], gait_dir: PathBuf) {
    let workflow_id = match argv.first() {
        Some(id) => id.clone(),
        None => {
            eprintln!("[gait] usage: gait workflow abort <workflow-id>");
            std::process::exit(1);
        }
    };
    let _ = gait_core::git::Repos::production().traces.commit_session(
        &gait_dir,
        &workflow_id,
        "workflow: aborted",
    );
    println!("[gait workflow] aborted {}", workflow_id);
}

pub fn cmd_workflow_log(argv: &[String], gait_dir: PathBuf) {
    let workflow_id = match argv.first() {
        Some(id) => id.clone(),
        None => {
            eprintln!("[gait] usage: gait workflow log <workflow-id>");
            std::process::exit(1);
        }
    };
    let workflow_runs_dir = gait_dir.join("workflow-runs");
    let events = gait_workflow::run_log::WorkflowRunLog::open(&workflow_runs_dir, &workflow_id)
        .and_then(|l| l.read_events())
        .unwrap_or_default();
    for event in &events {
        println!("{}", serde_json::to_string(event).unwrap_or_default());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn read_yaml_path_from_run_log_extracts_yaml_path() {
        let dir = TempDir::new().unwrap();
        let gait_dir = dir.path().to_path_buf();
        let workflow_runs_dir = gait_dir.join("workflow-runs");
        std::fs::create_dir_all(&workflow_runs_dir).unwrap();

        // Create a workflow run log with a WorkflowStart event
        let run_id = "test-run-20260616-120000";
        let log =
            gait_workflow::run_log::WorkflowRunLog::create(&workflow_runs_dir, run_id).unwrap();
        let def = gait_core::proto::gait::workflow::v1::Workflow::default();
        log.workflow_start(run_id, "test-workflow", "workflows/test.yaml", &[], &def)
            .unwrap();

        // Test the helper function
        let yaml_path = read_yaml_path_from_run_log(&gait_dir, run_id);
        assert_eq!(yaml_path, "workflows/test.yaml");
    }

    #[test]
    fn read_yaml_path_from_run_log_returns_empty_when_not_found() {
        let dir = TempDir::new().unwrap();
        let gait_dir = dir.path().to_path_buf();

        // Test with a non-existent run
        let yaml_path = read_yaml_path_from_run_log(&gait_dir, "nonexistent-run");
        assert_eq!(yaml_path, "");
    }
}

#[cfg(test)]
mod list_tests {
    use super::*;
    use gait_core::proto::gait::workflow::v1::Workflow;
    use gait_workflow::run_log::WorkflowRunLog;

    #[test]
    fn summaries_derive_name_and_status_from_run_log() {
        let td = tempfile::TempDir::new().unwrap();
        let runs = td.path().join("workflow-runs");
        let log = WorkflowRunLog::create(&runs, "deploy-1-2-3").unwrap();
        log.workflow_start(
            "deploy-1-2-3",
            "deploy",
            "wf.yaml",
            &[],
            &Workflow::default(),
        )
        .unwrap();
        log.workflow_complete("deploy-1-2-3").unwrap();
        let out = collect_workflow_summaries(&runs);
        assert_eq!(
            out,
            vec![(
                "deploy-1-2-3".to_string(),
                "deploy".to_string(),
                "done".to_string(),
            )]
        );
    }

    #[test]
    fn continue_gate_uses_run_log_name() {
        let td = tempfile::TempDir::new().unwrap();
        let runs = td.path().join("workflow-runs");
        let log = WorkflowRunLog::create(&runs, "deploy-9").unwrap();
        log.workflow_start("deploy-9", "deploy", "wf.yaml", &[], &Workflow::default())
            .unwrap();
        let state = gait_workflow::state::reconstruct_state(
            &WorkflowRunLog::open(&runs, "deploy-9")
                .unwrap()
                .read_events()
                .unwrap(),
        );
        assert_eq!(state.workflow_name, "deploy");
        assert!(WorkflowRunLog::open(&runs, "missing").is_err());
    }
}

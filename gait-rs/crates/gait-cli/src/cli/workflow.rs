//! `gait workflow <run|list|continue|abort|log>` commands.
//! Pulled out of gait-bin so the bin can become a thin dispatcher.

use clap::Parser;
use std::path::PathBuf;

use gait_workflow::observer::CliObserver;
use gait_workflow::progress;
use gait_workflow::runner::{run_workflow, WorkflowOutcome, WorkflowRunOpts};
use gait_workflow::schema::WorkflowDef; // still needed for yaml parsing

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

pub fn cmd_workflow_list(_argv: &[String], gait_dir: PathBuf) {
    let sessions = gait_core::git::Repos::production()
        .traces
        .list_active_sessions(&gait_dir)
        .unwrap_or_default();
    let mut found = false;
    for session_id in &sessions {
        if let Some(progress) = progress::read_progress(&gait_dir, session_id) {
            println!("{} — {} ({})", session_id, progress.name, progress.status);
            found = true;
        }
    }
    if !found {
        println!("(no active workflow sessions)");
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
    let progress = match progress::read_progress(&gait_dir, &workflow_id) {
        Some(p) => p,
        None => {
            eprintln!("[gait] no progress.yaml found for workflow {}", workflow_id);
            std::process::exit(1);
        }
    };
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
        progress.name, workflow_id
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

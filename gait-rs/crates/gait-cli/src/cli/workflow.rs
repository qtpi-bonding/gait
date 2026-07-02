//! `gait workflow <run|list|continue|abort|log>` commands.
//! Pulled out of gait-bin so the bin can become a thin dispatcher.

use clap::Parser;
use std::path::PathBuf;

use gait_workflow::observer::CliObserver;
use gait_workflow::progress;
use gait_workflow::runner::{run_workflow, WorkflowOutcome, WorkflowRunOpts};
use gait_workflow::schema::WorkflowDef; // still needed for yaml parsing

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
    let tx_path = gait_dir
        .join("sessions")
        .join(&workflow_id)
        .join("log.jsonl");
    let events = gait_core::session::log::read_events(&tx_path).unwrap_or_default();
    let yaml_path_str = events
        .iter()
        .find_map(|e| {
            use gait_core::proto::gait::log::v1::{event, payload};
            if let Some(event::Body::Payload(p)) = &e.body {
                if let Some(payload::Kind::Raw(r)) = &p.kind {
                    if r.r#type == "workflow_start" {
                        let v = r
                            .payload
                            .as_ref()
                            .and_then(|s| serde_json::to_value(s).ok())
                            .unwrap_or(serde_json::Value::Null);
                        return v
                            .get("yaml_path")
                            .and_then(|x| x.as_str())
                            .map(|s| s.to_string());
                    }
                }
            }
            None
        })
        .unwrap_or_default();

    if yaml_path_str.is_empty() {
        eprintln!("[gait] cannot find workflow YAML path in transcript");
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
    let tx_path = gait_dir
        .join("sessions")
        .join(&workflow_id)
        .join("log.jsonl");
    let events = gait_core::session::log::read_events(&tx_path).unwrap_or_default();
    for event in &events {
        println!("{}", serde_json::to_string(event).unwrap_or_default());
    }
}

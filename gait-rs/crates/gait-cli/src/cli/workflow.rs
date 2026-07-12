//! `gait workflow <run|list|continue|abort|log>` commands.
//! Pulled out of gait-bin so the bin can become a thin dispatcher.

use clap::Parser;
use std::path::{Path, PathBuf};

use gait_workflow::observer::CliObserver;
use gait_workflow::run_log::WorkflowRunLog;
use gait_workflow::runner::{run_workflow, WorkflowOutcome, WorkflowRunOpts};
use gait_workflow::schema::WorkflowDef; // still needed for yaml parsing
use gait_workflow::state::reconstruct_state;

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
/// Pull a `--gait-dir <path>` or `--gait-dir=<path>` pair out of `rest`,
/// returning the extracted path (if any) and the remaining tokens.
///
/// Why this exists: `WorkflowArgs.rest` is a `#[arg(trailing_var_arg = true)]`
/// catch-all. Once clap starts consuming positionals into it, it swallows
/// every later token verbatim — including a `--gait-dir <path>` typed after
/// the action/name, e.g. `gait workflow run somename --gait-dir /path`
/// (the exact usage README.md and docs/cli-audit.md document). That flag
/// then never reaches `WorkflowArgs.gait_dir`, and `cmd_workflow` silently
/// falls back to `$GAIT_DIR`. This scans `rest` for that swallowed flag and
/// recovers it, so `--gait-dir` works regardless of where it's typed —
/// right after `workflow` (already worked) or trailing after the
/// action/rest (previously broken).
fn extract_gait_dir_from_rest(rest: &[String]) -> (Option<PathBuf>, Vec<String>) {
    let mut found: Option<PathBuf> = None;
    let mut remaining = Vec::with_capacity(rest.len());
    let mut i = 0;
    while i < rest.len() {
        if rest[i] == "--gait-dir" {
            if let Some(value) = rest.get(i + 1) {
                found = Some(PathBuf::from(value));
                i += 2;
                continue;
            }
        } else if let Some(value) = rest[i].strip_prefix("--gait-dir=") {
            found = Some(PathBuf::from(value));
            i += 1;
            continue;
        }
        remaining.push(rest[i].clone());
        i += 1;
    }
    (found, remaining)
}

pub async fn cmd_workflow(args: WorkflowArgs) {
    let (gait_dir_from_rest, rest) = extract_gait_dir_from_rest(&args.rest);
    let gait_dir: PathBuf = match args.gait_dir.clone().or(gait_dir_from_rest).or_else(|| {
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
            ..Default::default()
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

/// `gait workflow continue <run-id> [--force]`
#[derive(clap::Parser, Debug)]
#[command(no_binary_name = true)]
struct ContinueArgs {
    /// Workflow run id to resume.
    run_id: String,
    /// Resume the ORIGINAL logged graph even if the on-disk YAML changed.
    /// Without this, an edited YAML is a hard error (never a silent divergence).
    #[arg(long)]
    force: bool,
}

pub async fn cmd_workflow_continue(argv: &[String], gait_dir: PathBuf) {
    let parsed = match ContinueArgs::try_parse_from(argv) {
        Ok(a) => a,
        Err(e) => {
            eprint!("{e}");
            std::process::exit(1);
        }
    };
    let workflow_id = parsed.run_id;

    // Source of truth: the run log — NOT the session transcript, NOT disk YAML.
    let run_log = match gait_workflow::run_log::WorkflowRunLog::open(
        &gait_dir.join("workflow-runs"),
        &workflow_id,
    ) {
        Ok(l) => l,
        Err(_) => {
            eprintln!("[gait] no run log found for workflow {}", workflow_id);
            std::process::exit(1);
        }
    };
    let events = run_log.read_events().unwrap_or_default();

    // Read on-disk YAML (if present) ONLY to detect drift; the graph that runs
    // always comes from the logged def.
    let logged_yaml_path = events
        .iter()
        .find_map(|e| {
            use gait_core::proto::gait::workflow::v1::workflow_event;
            match &e.kind {
                Some(workflow_event::Kind::WorkflowStart(s)) => Some(s.yaml_path.clone()),
                _ => None,
            }
        })
        .unwrap_or_default();
    let disk_yaml = std::fs::read_to_string(&logged_yaml_path).ok();

    let resolved = match gait_workflow::convert::resolve_continue_def(
        &events,
        disk_yaml.as_deref(),
        parsed.force,
    ) {
        Ok(r) => r,
        Err(gait_workflow::convert::ContinueError::Drift { .. }) => {
            eprintln!(
                "[gait] {} changed since this run started; start a new run, \
                     or pass --force to resume the original graph",
                logged_yaml_path
            );
            std::process::exit(1);
        }
        Err(gait_workflow::convert::ContinueError::NoStart) => {
            eprintln!("[gait] run log for {} has no workflow_start", workflow_id);
            std::process::exit(1);
        }
    };

    if parsed.force && disk_yaml.is_some() {
        eprintln!(
            "[gait workflow] --force: resuming the ORIGINAL logged graph (ignoring on-disk YAML edits)"
        );
    }
    eprintln!(
        "[gait workflow] resuming {} ({})",
        resolved.workflow_name, workflow_id
    );

    let mut sink = gait_core::embed::CliSink::stdio(false);
    match run_workflow(
        WorkflowRunOpts {
            workflow_def: resolved.def,
            yaml_path: resolved.yaml_path,
            run_id: workflow_id,
            gait_dir: gait_dir.clone(),
            codebase: gait_dir.join("codebase"),
            args: resolved.args,
            ..Default::default()
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

    // Abort must change recoverable state: write a terminal WorkflowError into the
    // run log so `reconstruct_state` folds the run to "failed" and `list`/`continue`
    // no longer treat it as resumable. (The run log is the sole source of truth —
    // there is no bookkeeping session to commit.)
    let workflow_runs_dir = gait_dir.join("workflow-runs");
    let run_log = match WorkflowRunLog::open(&workflow_runs_dir, &workflow_id) {
        Ok(l) => l,
        Err(_) => {
            eprintln!("[gait] no run log found for workflow {}", workflow_id);
            std::process::exit(1);
        }
    };
    // Best-effort: record the node that was in flight (if any) for context.
    let failed_node = run_log
        .read_events()
        .ok()
        .map(|evs| reconstruct_state(&evs))
        .and_then(|state| state.open_node().map(|s| s.to_string()))
        .unwrap_or_default();
    if let Err(e) = run_log.workflow_error(&workflow_id, &failed_node, "aborted by user") {
        eprintln!("[gait] failed to record abort for {}: {}", workflow_id, e);
        std::process::exit(1);
    }
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
mod gait_dir_extraction_tests {
    use super::extract_gait_dir_from_rest;
    use std::path::PathBuf;

    #[test]
    fn finds_and_strips_a_space_separated_gait_dir() {
        let rest = vec![
            "somename".to_string(),
            "--gait-dir".to_string(),
            "/tmp/x".to_string(),
        ];
        let (found, remaining) = extract_gait_dir_from_rest(&rest);
        assert_eq!(found, Some(PathBuf::from("/tmp/x")));
        assert_eq!(remaining, vec!["somename".to_string()]);
    }

    #[test]
    fn finds_and_strips_an_equals_form_gait_dir() {
        let rest = vec!["somename".to_string(), "--gait-dir=/tmp/x".to_string()];
        let (found, remaining) = extract_gait_dir_from_rest(&rest);
        assert_eq!(found, Some(PathBuf::from("/tmp/x")));
        assert_eq!(remaining, vec!["somename".to_string()]);
    }

    #[test]
    fn leaves_rest_untouched_when_no_gait_dir_present() {
        let rest = vec!["somename".to_string(), "extra-arg".to_string()];
        let (found, remaining) = extract_gait_dir_from_rest(&rest);
        assert_eq!(found, None);
        assert_eq!(remaining, rest);
    }
}

#[cfg(test)]
mod continue_arg_tests {
    use super::ContinueArgs;
    use clap::Parser;
    #[test]
    fn continue_args_parse_force_flag() {
        let a = ContinueArgs::try_parse_from(["run-1", "--force"]).unwrap();
        assert_eq!(a.run_id, "run-1");
        assert!(a.force);
        let b = ContinueArgs::try_parse_from(["run-1"]).unwrap();
        assert!(!b.force);
    }
}

#[cfg(test)]
mod abort_tests {
    use super::*;
    use gait_core::proto::gait::workflow::v1::Workflow;
    use gait_workflow::run_log::WorkflowRunLog;

    #[test]
    fn abort_folds_run_to_failed() {
        let td = tempfile::TempDir::new().unwrap();
        let gait_dir = td.path().to_path_buf();
        let runs = gait_dir.join("workflow-runs");
        // A run that is mid-flight: started, one node running, no terminal event.
        let log = WorkflowRunLog::create(&runs, "deploy-abort-1").unwrap();
        log.workflow_start(
            "deploy-abort-1",
            "deploy",
            "wf.yaml",
            &[],
            &Workflow::default(),
        )
        .unwrap();
        log.node_start("build", "shell", 1, "").unwrap();
        // Pre-condition: without the abort, the run folds to "running" (resumable).
        let before = reconstruct_state(
            &WorkflowRunLog::open(&runs, "deploy-abort-1")
                .unwrap()
                .read_events()
                .unwrap(),
        );
        assert_eq!(before.workflow_status(), "running");

        cmd_workflow_abort(&["deploy-abort-1".to_string()], gait_dir.clone());

        // Post-condition: abort wrote a terminal WorkflowError → folds to "failed",
        // so list/continue no longer treat it as resumable.
        let after = reconstruct_state(
            &WorkflowRunLog::open(&runs, "deploy-abort-1")
                .unwrap()
                .read_events()
                .unwrap(),
        );
        assert_eq!(after.workflow_status(), "failed");
        assert_eq!(after.failed_node.as_deref(), Some("build"));
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

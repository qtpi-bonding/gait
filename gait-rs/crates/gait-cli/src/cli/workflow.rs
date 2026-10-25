//! `gait workflow <run|list|continue|abort|log>` commands.
//! Pulled out of gait-bin so the bin can become a thin dispatcher.

use clap::Parser;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use crate::cli::error::{CliError, CliResult};
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
    /// Applies to `list`/`abort`/`validate` only — `run`/`continue` stream
    /// turn output through `CliSink` (always compact, no pretty option) and
    /// `log` streams one compact JSON object per event (NDJSON; pretty would
    /// break line-per-event streaming).
    #[arg(long)]
    pub pretty: bool,
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

/// Same problem `extract_gait_dir_from_rest` solves, for the boolean
/// `--pretty` flag: `WorkflowArgs.rest`'s `trailing_var_arg` swallows a
/// `--pretty` typed after the action (`gait workflow list --pretty`) before
/// it can reach `WorkflowArgs.pretty`.
fn extract_pretty_from_rest(rest: &[String]) -> (bool, Vec<String>) {
    let mut found = false;
    let mut remaining = Vec::with_capacity(rest.len());
    for tok in rest {
        if tok == "--pretty" {
            found = true;
        } else {
            remaining.push(tok.clone());
        }
    }
    (found, remaining)
}

/// Priority between `WorkflowArgs.gait_dir` (a leading `--gait-dir`, or
/// clap's `env = "GAIT_DIR"` fallback if no leading flag was given at all)
/// and a `--gait-dir` recovered from `rest` by `extract_gait_dir_from_rest`.
/// The recovered value must win: clap's `env` fallback populates
/// `WorkflowArgs.gait_dir` from `$GAIT_DIR` whenever no *leading* flag is
/// present, which is indistinguishable from "the user really meant the env
/// default" — but a `--gait-dir` typed after the action and swallowed into
/// `rest` is an explicit, deliberate choice that must not be shadowed by an
/// env var the user may not even know is set.
fn resolve_workflow_gait_dir(
    cli_field: Option<PathBuf>,
    from_rest: Option<PathBuf>,
) -> Option<PathBuf> {
    from_rest.or(cli_field)
}

pub async fn cmd_workflow(args: WorkflowArgs) -> anyhow::Result<ExitCode> {
    let (gait_dir_from_rest, rest) = extract_gait_dir_from_rest(&args.rest);
    let (pretty_from_rest, rest) = extract_pretty_from_rest(&rest);
    let pretty = args.pretty || pretty_from_rest;
    let gait_dir = crate::cli::args::resolve_gait_dir_from(resolve_workflow_gait_dir(
        args.gait_dir.clone(),
        gait_dir_from_rest,
    ))
    .map_err(|error| CliError::not_found(format!("[gait] {error}")))?;
    let sub = args.action.as_deref().unwrap_or("");
    match sub {
        "run" => cmd_workflow_run(&rest, gait_dir).await,
        "list" => cmd_workflow_list(&rest, gait_dir, pretty).map(|()| ExitCode::SUCCESS),
        "continue" => cmd_workflow_continue(&rest, gait_dir).await,
        "abort" => cmd_workflow_abort(&rest, gait_dir, pretty).map(|()| ExitCode::SUCCESS),
        "log" => cmd_workflow_log(&rest, gait_dir).map(|()| ExitCode::SUCCESS),
        "validate" => cmd_workflow_validate(&rest, gait_dir, pretty).map(|()| ExitCode::SUCCESS),
        _ => Err(CliError::usage(
            "[gait] usage: gait workflow <run|list|continue|abort|log|validate>",
        )),
    }
}

pub async fn cmd_workflow_run(argv: &[String], gait_dir: PathBuf) -> anyhow::Result<ExitCode> {
    let name = match argv.first() {
        Some(n) => n.clone(),
        None => {
            return Err(CliError::usage(
                "[gait] usage: gait workflow run <name> [args...]",
            ))
        }
    };
    let args: Vec<String> = argv[1..].to_vec();

    let yaml_path = gait_dir.join("workflows").join(format!("{}.yaml", name));
    let yaml_content = std::fs::read_to_string(&yaml_path).map_err(|e| {
        CliError::failed(format!("[gait] cannot read {}: {}", yaml_path.display(), e))
    })?;
    let wf: WorkflowDef = serde_yaml::from_str(&yaml_content).map_err(|e| {
        CliError::invalid_input(format!("[gait] invalid workflow YAML: {}", e))
    })?;

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
        Ok(WorkflowOutcome::Completed) => Ok(ExitCode::SUCCESS),
        Ok(WorkflowOutcome::GatePaused) => {
            eprintln!("[gait workflow] paused at gate");
            Ok(ExitCode::from(2))
        }
        Ok(WorkflowOutcome::NodeOpen) => {
            eprintln!("[gait workflow] paused at open node");
            Ok(ExitCode::SUCCESS)
        }
        Err(e) => Err(CliError::failed(format!("[gait workflow] error: {}", e))),
    }
}

/// Parse and structurally validate a workflow's graph without running it —
/// no worktree, no run-log, no gait-dir session state touched. Prints the
/// JSON verdict on stdout; an invalid graph returns an invalid_input error
/// naming every problem found.
/// Validate raw workflow YAML against the generated `WorkflowDef` JSON
/// Schema, returning every violation found (not just the first). `None`
/// means the YAML is schema-valid (or wasn't parseable YAML at all — the
/// `serde_yaml::from_str::<WorkflowDef>` call right after this one owns
/// reporting that case).
fn schema_shape_errors(yaml_content: &str) -> Option<Vec<String>> {
    let as_json = serde_yaml::from_str::<serde_json::Value>(yaml_content).ok()?;
    let schema_value = serde_json::to_value(gait_workflow::schema::json_schema())
        .expect("generated workflow JSON Schema always serializes");
    let compiled = jsonschema::JSONSchema::compile(&schema_value).ok()?;
    let errors: Vec<String> = compiled
        .validate(&as_json)
        .err()?
        .map(|e| format!("{}: {}", e.instance_path, e))
        .collect();
    Some(errors)
}

pub fn cmd_workflow_validate(argv: &[String], gait_dir: PathBuf, pretty: bool) -> CliResult {
    let name = match argv.first() {
        Some(n) => n.clone(),
        None => {
            return Err(CliError::usage(
                "[gait] usage: gait workflow validate <name>",
            ))
        }
    };
    let yaml_path = gait_dir.join("workflows").join(format!("{}.yaml", name));
    let yaml_content = std::fs::read_to_string(&yaml_path).map_err(|e| {
        CliError::failed(format!("[gait] cannot read {}: {}", yaml_path.display(), e))
    })?;

    // Schema-shape check first: reports every field-level violation (wrong
    // type, missing required field, etc.) in one pass. The `serde_yaml`
    // parse below stops at the first error, so this runs ahead of it.
    if let Some(error_strings) = schema_shape_errors(&yaml_content) {
        println!(
            "{}",
            gait_ops::render_json(
                &serde_json::json!({"name": name, "valid": false, "errors": error_strings}),
                pretty
            )
        );
        return Err(CliError::invalid_input(format!(
            "[gait] workflow {name} is not valid: {}",
            error_strings.join("; ")
        )));
    }

    let wf: WorkflowDef = serde_yaml::from_str(&yaml_content).map_err(|e| {
        CliError::invalid_input(format!("[gait] invalid workflow YAML: {}", e))
    })?;
    let proto = gait_workflow::convert::to_proto(&wf);
    let errors = gait_workflow::validate::validate_graph(&proto);
    let valid = errors.is_empty();
    let error_strings: Vec<String> = errors.iter().map(|e| e.to_string()).collect();
    println!(
        "{}",
        gait_ops::render_json(
            &serde_json::json!({"name": name, "valid": valid, "errors": error_strings}),
            pretty
        )
    );
    if !valid {
        return Err(CliError::invalid_input(format!(
            "[gait] workflow {name} is not valid: {}",
            error_strings.join("; ")
        )));
    }
    Ok(())
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

pub fn cmd_workflow_list(_argv: &[String], gait_dir: PathBuf, pretty: bool) -> CliResult {
    let summaries = collect_workflow_summaries(&gait_dir.join("workflow-runs"));
    let runs: Vec<serde_json::Value> = summaries
        .into_iter()
        .map(|(run_id, name, status)| serde_json::json!({"run_id": run_id, "name": name, "status": status}))
        .collect();
    println!(
        "{}",
        gait_ops::render_json(&serde_json::json!({"runs": runs}), pretty)
    );
    Ok(())
}

/// `gait workflow continue <run-id> [--force] [--force-rerun <node-id>]...`
#[derive(clap::Parser, Debug)]
#[command(no_binary_name = true)]
struct ContinueArgs {
    /// Workflow run id to resume.
    run_id: String,
    /// Resume the ORIGINAL logged graph even if the on-disk YAML changed.
    /// Without this, an edited YAML is a hard error (never a silent divergence).
    #[arg(long)]
    force: bool,
    /// Accept the risk of re-running a shell node that was still executing when
    /// this workflow was interrupted (its prior side effects, if any, are NOT
    /// undone). Must name the exact node id reported in the resume-guard error.
    /// Repeatable — pass once per stuck node.
    #[arg(long)]
    force_rerun: Vec<String>,
}

pub async fn cmd_workflow_continue(argv: &[String], gait_dir: PathBuf) -> anyhow::Result<ExitCode> {
    let parsed = match ContinueArgs::try_parse_from(argv) {
        Ok(a) => a,
        // clap's message already ends in a newline; main adds its own, so trim
        // it to keep the printed bytes identical to the old `eprint!`.
        Err(e) => {
            return Err(CliError::usage(
                format!("{e}").trim_end_matches('\n').to_string(),
            ))
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
            return Err(CliError::not_found(format!(
                "[gait] no run log found for workflow {}",
                workflow_id
            )))
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
            return Err(CliError::invalid_input(format!(
                "[gait] {} changed since this run started; start a new run, \
                     or pass --force to resume the original graph",
                logged_yaml_path
            )));
        }
        Err(gait_workflow::convert::ContinueError::NoStart) => {
            return Err(CliError::failed(format!(
                "[gait] run log for {} has no workflow_start",
                workflow_id
            )));
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
            force_rerun_nodes: parsed.force_rerun.clone(),
            ..Default::default()
        },
        &mut sink,
        &mut CliObserver,
        None,
    )
    .await
    {
        Ok(WorkflowOutcome::Completed) => Ok(ExitCode::SUCCESS),
        Ok(WorkflowOutcome::GatePaused) => {
            eprintln!("[gait workflow] paused at gate");
            Ok(ExitCode::from(2))
        }
        Ok(WorkflowOutcome::NodeOpen) => {
            eprintln!("[gait workflow] paused at open node");
            Ok(ExitCode::SUCCESS)
        }
        Err(e) => Err(CliError::failed(format!("[gait workflow] error: {}", e))),
    }
}

pub fn cmd_workflow_abort(argv: &[String], gait_dir: PathBuf, pretty: bool) -> CliResult {
    let workflow_id = match argv.first() {
        Some(id) => id.clone(),
        None => {
            return Err(CliError::usage(
                "[gait] usage: gait workflow abort <workflow-id>",
            ))
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
            return Err(CliError::not_found(format!(
                "[gait] no run log found for workflow {}",
                workflow_id
            )))
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
        return Err(CliError::failed(format!(
            "[gait] failed to record abort for {}: {}",
            workflow_id, e
        )));
    }
    println!(
        "{}",
        gait_ops::render_json(
            &serde_json::json!({"workflow_id": workflow_id, "action": "aborted"}),
            pretty
        )
    );
    Ok(())
}

pub fn cmd_workflow_log(argv: &[String], gait_dir: PathBuf) -> CliResult {
    let workflow_id = match argv.first() {
        Some(id) => id.clone(),
        None => {
            return Err(CliError::usage(
                "[gait] usage: gait workflow log <workflow-id>",
            ))
        }
    };
    let workflow_runs_dir = gait_dir.join("workflow-runs");
    let events = gait_workflow::run_log::WorkflowRunLog::open(&workflow_runs_dir, &workflow_id)
        .and_then(|l| l.read_events())
        .unwrap_or_default();
    for event in &events {
        println!("{}", serde_json::to_string(event).unwrap_or_default());
    }
    Ok(())
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

    // -- gait_dir priority (bug: env-populated CLI field was winning over an
    // explicit --gait-dir recovered from `rest`) --

    #[test]
    fn rest_extracted_gait_dir_wins_over_env_populated_cli_field() {
        // Simulates `$GAIT_DIR` being set in the environment (so clap's
        // `env = "GAIT_DIR"` populates `WorkflowArgs.gait_dir` even though the
        // user never passed a leading --gait-dir) while the user ALSO typed
        // `--gait-dir <path>` after the action, where trailing_var_arg
        // swallows it into `rest` before `extract_gait_dir_from_rest` pulls
        // it back out. The explicit, recovered value must win.
        let cli_field_from_env = Some(PathBuf::from("/env/default"));
        let from_rest = Some(PathBuf::from("/explicit/from/rest"));
        assert_eq!(
            super::resolve_workflow_gait_dir(cli_field_from_env, from_rest),
            Some(PathBuf::from("/explicit/from/rest"))
        );
    }

    #[test]
    fn env_populated_cli_field_used_when_nothing_recovered_from_rest() {
        let cli_field_from_env = Some(PathBuf::from("/env/default"));
        assert_eq!(
            super::resolve_workflow_gait_dir(cli_field_from_env, None),
            Some(PathBuf::from("/env/default"))
        );
    }

    #[test]
    fn none_when_neither_source_has_a_value() {
        assert_eq!(super::resolve_workflow_gait_dir(None, None), None);
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

    #[test]
    fn continue_args_parse_force_rerun_repeatable() {
        let a = ContinueArgs::try_parse_from(["run-1"]).unwrap();
        assert!(a.force_rerun.is_empty());

        let b = ContinueArgs::try_parse_from(["run-1", "--force-rerun", "a"]).unwrap();
        assert_eq!(b.force_rerun, vec!["a".to_string()]);

        let c = ContinueArgs::try_parse_from(["run-1", "--force-rerun", "a", "--force-rerun", "b"])
            .unwrap();
        assert_eq!(c.force_rerun, vec!["a".to_string(), "b".to_string()]);
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

        cmd_workflow_abort(&["deploy-abort-1".to_string()], gait_dir.clone(), false)
            .expect("abort of a mid-flight run succeeds");

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

#[cfg(test)]
mod validate_tests {
    use super::*;

    /// Happy path: a valid graph returns Ok(()). The invalid-graph path is
    /// covered by `invalid_graph_is_an_invalid_input_error` below.
    #[test]
    fn valid_workflow_yaml_returns_ok() {
        let td = tempfile::TempDir::new().unwrap();
        let gait_dir = td.path().to_path_buf();
        std::fs::create_dir_all(gait_dir.join("workflows")).unwrap();
        std::fs::write(
            gait_dir.join("workflows").join("good.yaml"),
            "name: good\nnodes:\n  - id: a\n    shell: \"echo hi\"\n",
        )
        .unwrap();

        cmd_workflow_validate(&["good".to_string()], gait_dir, false)
            .expect("a valid workflow validates to Ok");
    }

    #[test]
    fn invalid_graph_is_an_invalid_input_error() {
        let td = tempfile::TempDir::new().unwrap();
        let gait_dir = td.path().to_path_buf();
        std::fs::create_dir_all(gait_dir.join("workflows")).unwrap();
        std::fs::write(
            gait_dir.join("workflows").join("bad.yaml"),
            "name: bad\nauto: \"yes\"\nnodes: \"oops\"\n",
        )
        .unwrap();
        let err = cmd_workflow_validate(&["bad".to_string()], gait_dir, false)
            .expect_err("a schema-invalid workflow is an error");
        assert!(matches!(
            coded_error::downcast_ref::<CliError>(&err),
            Some(CliError::InvalidInput(_))
        ));
    }

    #[test]
    fn validate_without_a_name_is_a_usage_error() {
        let td = tempfile::TempDir::new().unwrap();
        let err = cmd_workflow_validate(&[], td.path().to_path_buf(), false)
            .expect_err("missing name is an error");
        assert!(matches!(
            coded_error::downcast_ref::<CliError>(&err),
            Some(CliError::Usage(_))
        ));
    }

    #[test]
    fn validate_of_a_missing_workflow_is_a_failed_read() {
        let td = tempfile::TempDir::new().unwrap();
        let err = cmd_workflow_validate(&["nope".to_string()], td.path().to_path_buf(), false)
            .expect_err("missing workflow file is an error");
        assert!(matches!(
            coded_error::downcast_ref::<CliError>(&err),
            Some(CliError::Failed(_))
        ));
    }

    #[test]
    fn validate_with_unparseable_yaml_is_an_invalid_input_error() {
        // Passes the schema-shape check (a bare scalar is not an object, so
        // schema_shape_errors returns None) and then fails serde_yaml parsing.
        let td = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(td.path().join("workflows")).unwrap();
        std::fs::write(td.path().join("workflows").join("junk.yaml"), "- [unclosed\n").unwrap();
        let err = cmd_workflow_validate(&["junk".to_string()], td.path().to_path_buf(), false)
            .expect_err("unparseable workflow is an error");
        assert!(matches!(
            coded_error::downcast_ref::<CliError>(&err),
            Some(CliError::InvalidInput(_))
        ));
    }

    #[tokio::test]
    async fn run_without_a_name_is_a_usage_error() {
        let td = tempfile::TempDir::new().unwrap();
        let err = cmd_workflow_run(&[], td.path().to_path_buf())
            .await
            .expect_err("missing name is an error");
        assert!(matches!(
            coded_error::downcast_ref::<CliError>(&err),
            Some(CliError::Usage(_))
        ));
    }

    #[tokio::test]
    async fn run_of_a_missing_workflow_is_a_failed_read() {
        let td = tempfile::TempDir::new().unwrap();
        let err = cmd_workflow_run(&["nope".to_string()], td.path().to_path_buf())
            .await
            .expect_err("missing workflow file is an error");
        assert!(matches!(
            coded_error::downcast_ref::<CliError>(&err),
            Some(CliError::Failed(_))
        ));
    }

    #[tokio::test]
    async fn continue_with_an_unknown_flag_is_a_usage_error() {
        let td = tempfile::TempDir::new().unwrap();
        let err = cmd_workflow_continue(&["run-1".to_string(), "--bogus".to_string()], td.path().to_path_buf())
            .await
            .expect_err("unknown flag is an error");
        assert!(matches!(
            coded_error::downcast_ref::<CliError>(&err),
            Some(CliError::Usage(_))
        ));
    }

    #[tokio::test]
    async fn continue_of_a_missing_run_is_not_found() {
        let td = tempfile::TempDir::new().unwrap();
        let err = cmd_workflow_continue(&["no-such-run".to_string()], td.path().to_path_buf())
            .await
            .expect_err("missing run log is an error");
        assert!(matches!(
            coded_error::downcast_ref::<CliError>(&err),
            Some(CliError::NotFound(_))
        ));
    }

    #[test]
    fn abort_of_a_missing_run_is_not_found() {
        let td = tempfile::TempDir::new().unwrap();
        let err = cmd_workflow_abort(&["no-such-run".to_string()], td.path().to_path_buf(), false)
            .expect_err("missing run log is an error");
        assert!(matches!(
            coded_error::downcast_ref::<CliError>(&err),
            Some(CliError::NotFound(_))
        ));
    }

    #[test]
    fn abort_without_an_id_is_a_usage_error() {
        let td = tempfile::TempDir::new().unwrap();
        let err = cmd_workflow_abort(&[], td.path().to_path_buf(), false)
            .expect_err("missing id is an error");
        assert!(matches!(
            coded_error::downcast_ref::<CliError>(&err),
            Some(CliError::Usage(_))
        ));
    }

    #[test]
    fn log_without_an_id_is_a_usage_error() {
        let td = tempfile::TempDir::new().unwrap();
        let err = cmd_workflow_log(&[], td.path().to_path_buf()).expect_err("missing id is an error");
        assert!(matches!(
            coded_error::downcast_ref::<CliError>(&err),
            Some(CliError::Usage(_))
        ));
    }

    #[test]
    fn schema_shape_errors_reports_every_violation_at_once() {
        // Two independent shape errors: `nodes` is a string instead of a
        // list, and `auto` is a string instead of a bool. A bare
        // `serde_yaml::from_str::<WorkflowDef>` would only ever surface the
        // first one it happens to hit.
        let yaml = "name: bad\nauto: \"yes\"\nnodes: \"oops\"\n";
        let errors = super::schema_shape_errors(yaml).expect("schema violations expected");
        assert_eq!(errors.len(), 2, "expected both violations, got: {errors:?}");
        assert!(
            errors.iter().any(|e| e.contains("/nodes")),
            "missing /nodes violation: {errors:?}"
        );
        assert!(
            errors.iter().any(|e| e.contains("/auto")),
            "missing /auto violation: {errors:?}"
        );
    }

    #[test]
    fn schema_shape_errors_is_none_for_valid_workflow_yaml() {
        let yaml = "name: good\nnodes:\n  - id: a\n    shell: \"echo hi\"\n";
        assert_eq!(super::schema_shape_errors(yaml), None);
    }
}

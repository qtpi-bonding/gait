//! `gait workflow <run|list|continue|abort|log>` commands.
//! Pulled out of gait-bin so the bin can become a thin dispatcher.

use clap::Parser;
use std::path::{Path, PathBuf};

use crate::cli::error::{CliError, CliResult};
use gait_workflow::observer::CliObserver;
use gait_workflow::run_log::WorkflowRunLog;
use gait_workflow::state::reconstruct_state;
use gait_workflow::types::{render_dts, DTS_FILE_NAME};
use gait_workflow::WorkflowOutcome;

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
    /// Applies to `list`/`abort` only — `run`/`continue` stream
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

pub async fn cmd_workflow(args: WorkflowArgs) -> anyhow::Result<u8> {
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
        "list" => cmd_workflow_list(&rest, gait_dir, pretty).map(|()| 0u8),
        "continue" => cmd_workflow_continue(&rest, gait_dir).await,
        "abort" => cmd_workflow_abort(&rest, gait_dir, pretty).map(|()| 0u8),
        "log" => cmd_workflow_log(&rest, gait_dir).map(|()| 0u8),
        "types" => cmd_workflow_types(&rest, gait_dir, pretty).map(|()| 0u8),
        _ => Err(CliError::usage(
            "[gait] usage: gait workflow <run|list|continue|abort|log|types>",
        )),
    }
}

/// `gait workflow types`: write the generated TypeScript declarations for
/// workflow scripts (`gait.d.ts`) into `<gait_dir>/workflows/` and print the
/// written path as the JSON result. An unexpected extra argument is a usage
/// error.
pub fn cmd_workflow_types(argv: &[String], gait_dir: PathBuf, pretty: bool) -> CliResult {
    if let Some(extra) = argv.first() {
        return Err(CliError::usage(format!(
            "[gait] usage: gait workflow types (unexpected argument: {extra})"
        )));
    }
    let workflows_dir = gait_dir.join("workflows");
    std::fs::create_dir_all(&workflows_dir).map_err(|e| {
        CliError::failed(format!(
            "[gait] cannot create {}: {}",
            workflows_dir.display(),
            e
        ))
    })?;
    let path = workflows_dir.join(DTS_FILE_NAME);
    std::fs::write(&path, render_dts())
        .map_err(|e| CliError::failed(format!("[gait] cannot write {}: {}", path.display(), e)))?;
    println!(
        "{}",
        gait_ops::render_json(
            &serde_json::json!({"path": path.display().to_string()}),
            pretty
        )
    );
    Ok(())
}

pub async fn cmd_workflow_run(argv: &[String], gait_dir: PathBuf) -> anyhow::Result<u8> {
    let name = match argv.first() {
        Some(n) => n.clone(),
        None => {
            return Err(CliError::usage(
                "[gait] usage: gait workflow run <name> [args...]",
            ))
        }
    };
    let args: Vec<String> = argv[1..].to_vec();

    let mut sink = gait_core::embed::CliSink::stdio(false);
    let env = gait_workflow::launch::LaunchEnv {
        gait_dir: gait_dir.clone(),
        codebase: gait_dir.join("codebase"),
        started: &mut |run_id, wf_name| {
            eprintln!("[gait workflow] starting {wf_name} as session {run_id}")
        },
        sink: &mut sink,
        observer: &mut CliObserver,
    };
    match gait_workflow::launch::run_named(&name, args, env).await {
        Ok((_run_id, WorkflowOutcome::Completed)) => Ok(0u8),
        Ok((_run_id, WorkflowOutcome::GatePaused)) => {
            eprintln!("[gait workflow] paused at gate");
            Ok(2u8)
        }
        Err((_run_id, e)) => Err(launch_error(e)),
    }
}

/// Map a `launch::LaunchError` to today's `CliError` classes.
fn launch_error(e: gait_workflow::launch::LaunchError) -> anyhow::Error {
    match e {
        gait_workflow::launch::LaunchError::Failed(e) => {
            CliError::failed_with("[gait workflow] error", e)
        }
        e => coded_error::into_anyhow(e),
    }
}

/// One per-run summary for `workflow_list_json`.
#[derive(Debug, PartialEq)]
struct RunSummary {
    run_id: String,
    name: String,
    status: String,
}

fn collect_workflow_summaries(workflow_runs_dir: &Path) -> Vec<RunSummary> {
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
        // A run whose log cannot be opened or read is skipped.
        let log = match WorkflowRunLog::open(workflow_runs_dir, &run_id) {
            Ok(l) => l,
            Err(_) => continue,
        };
        let events = match log.read_events() {
            Ok(ev) => ev,
            Err(_) => continue,
        };
        if events.is_empty() {
            continue;
        }
        let state = reconstruct_state(&events);
        out.push(RunSummary {
            run_id,
            name: state.workflow_name.clone(),
            status: state.workflow_status().to_string(),
        });
    }
    out
}

/// Why `open` failed for `workflow_id`: "not found".
fn open_failure(workflow_id: &str) -> anyhow::Error {
    CliError::not_found(format!(
        "[gait] no run log found for workflow {}",
        workflow_id
    ))
}

/// Why `read_events` failed for `workflow_id`: the read error itself (never
/// swallowed, nothing is written).
fn read_failure(workflow_id: &str, err: &anyhow::Error) -> anyhow::Error {
    CliError::failed(format!(
        "[gait] workflow {}: failed to read its run log: {}",
        workflow_id, err
    ))
}

pub fn cmd_workflow_list(_argv: &[String], gait_dir: PathBuf, pretty: bool) -> CliResult {
    let value = workflow_list_json(&gait_dir);
    println!("{}", gait_ops::render_json(&value, pretty));
    Ok(())
}

/// Build the JSON value `cmd_workflow_list` prints: `{"runs": [...]}` with
/// one entry per non-empty run log under `<gait_dir>/workflow-runs`.
pub fn workflow_list_json(gait_dir: &Path) -> serde_json::Value {
    let summaries = collect_workflow_summaries(&gait_dir.join("workflow-runs"));
    let runs: Vec<serde_json::Value> = summaries
        .into_iter()
        .map(|summary| {
            serde_json::json!({
                "run_id": summary.run_id,
                "name": summary.name,
                "status": summary.status
            })
        })
        .collect();
    serde_json::json!({"runs": runs})
}

/// `gait workflow continue <run-id> [--force] [--force-rerun <step-id>]...`
#[derive(clap::Parser, Debug)]
#[command(no_binary_name = true)]
struct ContinueArgs {
    /// Workflow run id to resume.
    run_id: String,
    /// Resume the script saved in the run log even if the file on disk
    /// changed. Without this, an edited script is a hard error.
    #[arg(long)]
    force: bool,
    /// Accept re-running a step that was still running when this workflow was
    /// interrupted (its earlier side effects are not undone). Must name the
    /// exact step id reported by the error. Repeatable, once per step.
    #[arg(long)]
    force_rerun: Vec<String>,
}

pub async fn cmd_workflow_continue(argv: &[String], gait_dir: PathBuf) -> anyhow::Result<u8> {
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

    // The log must be openable BEFORE the engine is chosen; the engine
    // decision lives in launch.rs.
    let run_log = match gait_workflow::run_log::WorkflowRunLog::open(
        &gait_dir.join("workflow-runs"),
        &workflow_id,
    ) {
        Ok(l) => l,
        Err(_) => return Err(open_failure(&workflow_id)),
    };
    if let Err(e) = run_log.read_events() {
        return Err(read_failure(&workflow_id, &e));
    }

    let mut sink = gait_core::embed::CliSink::stdio(false);
    let env = gait_workflow::launch::LaunchEnv {
        gait_dir: gait_dir.clone(),
        codebase: gait_dir.join("codebase"),
        started: &mut |run_id, name| eprintln!("[gait workflow] resuming {name} ({run_id})"),
        sink: &mut sink,
        observer: &mut CliObserver,
    };
    match gait_workflow::launch::continue_run(
        &workflow_id,
        parsed.force,
        parsed.force_rerun.clone(),
        env,
    )
    .await
    {
        Ok(WorkflowOutcome::Completed) => Ok(0u8),
        Ok(WorkflowOutcome::GatePaused) => {
            eprintln!("[gait workflow] paused at gate");
            Ok(2u8)
        }
        Err(e) => Err(launch_error(e)),
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
        Err(_) => return Err(open_failure(&workflow_id)),
    };
    // Best-effort: record the node that was in flight (if any) for context.
    let failed_node = match run_log.read_events() {
        Ok(evs) => reconstruct_state(&evs)
            .open_node()
            .map(|s| s.to_string())
            .unwrap_or_default(),
        Err(e) => return Err(read_failure(&workflow_id, &e)),
    };
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
    let lines = workflow_log_lines(&gait_dir, &workflow_id);
    for line in &lines {
        println!("{line}");
    }
    Ok(())
}

/// Produce the JSON lines `cmd_workflow_log` prints: one compact JSON
/// object per event in `<gait_dir>/workflow-runs/<run_id>/log.jsonl`.
pub fn workflow_log_lines(gait_dir: &Path, run_id: &str) -> Vec<String> {
    let workflow_runs_dir = gait_dir.join("workflow-runs");
    match gait_workflow::run_log::WorkflowRunLog::open(&workflow_runs_dir, run_id) {
        Ok(log) => log
            .read_events()
            .map(|ev| {
                ev.iter()
                    .map(|event| serde_json::to_string(event).unwrap_or_default())
                    .collect()
            })
            .unwrap_or_default(),
        Err(_) => Vec::new(),
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
    use gait_workflow::run_log::WorkflowRunLog;

    #[test]
    fn abort_folds_run_to_failed() {
        let td = tempfile::TempDir::new().unwrap();
        let gait_dir = td.path().to_path_buf();
        let runs = gait_dir.join("workflow-runs");
        // A run that is mid-flight: started, one node running, no terminal event.
        let log = WorkflowRunLog::create(&runs, "deploy-abort-1").unwrap();
        log.script_workflow_start("deploy-abort-1", "deploy", "wf.js", "deadbeef", "", &[])
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
    use gait_workflow::run_log::WorkflowRunLog;

    #[test]
    fn summaries_derive_name_and_status_from_run_log() {
        let td = tempfile::TempDir::new().unwrap();
        let runs = td.path().join("workflow-runs");
        let log = WorkflowRunLog::create(&runs, "deploy-1-2-3").unwrap();
        log.script_workflow_start("deploy-1-2-3", "deploy", "wf.js", "deadbeef", "", &[])
            .unwrap();
        log.workflow_complete("deploy-1-2-3").unwrap();
        let out = collect_workflow_summaries(&runs);
        assert_eq!(
            out,
            vec![RunSummary {
                run_id: "deploy-1-2-3".to_string(),
                name: "deploy".to_string(),
                status: "done".to_string(),
            }]
        );
    }

    #[test]
    fn continue_gate_uses_run_log_name() {
        let td = tempfile::TempDir::new().unwrap();
        let runs = td.path().join("workflow-runs");
        let log = WorkflowRunLog::create(&runs, "deploy-9").unwrap();
        log.script_workflow_start("deploy-9", "deploy", "wf.js", "deadbeef", "", &[])
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
mod cli_error_tests {
    use super::*;

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
    async fn run_of_a_missing_workflow_is_a_not_found() {
        let td = tempfile::TempDir::new().unwrap();
        let err = cmd_workflow_run(&["nope".to_string()], td.path().to_path_buf())
            .await
            .expect_err("missing workflow file is an error");
        assert!(matches!(
            coded_error::downcast_ref::<gait_workflow::launch::LaunchError>(&err),
            Some(gait_workflow::launch::LaunchError::NotFound { .. })
        ));
        assert_eq!(
            coded_error::cli::exit_code_anyhow(&err),
            65,
            "LaunchError::NotFound maps to exit code 65"
        );
    }

    #[tokio::test]
    async fn continue_with_an_unknown_flag_is_a_usage_error() {
        let td = tempfile::TempDir::new().unwrap();
        let err = cmd_workflow_continue(
            &["run-1".to_string(), "--bogus".to_string()],
            td.path().to_path_buf(),
        )
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
        let err =
            cmd_workflow_log(&[], td.path().to_path_buf()).expect_err("missing id is an error");
        assert!(matches!(
            coded_error::downcast_ref::<CliError>(&err),
            Some(CliError::Usage(_))
        ));
    }
}

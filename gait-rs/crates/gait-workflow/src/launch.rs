//! The ONE place that decides which workflow engine runs a named workflow
//! (`<name>.js` → the script engine, else `<name>.yaml` → the YAML DAG
//! engine) and how a run log is resumed (a `WorkflowStart` with a non-empty
//! `script_source` → the script engine, else the YAML engine).
//!
//! Both the CLI (`gait workflow run|continue`) and the REPL
//! (`/workflow` → `run_named_workflow`) go through this module, so the two
//! entry points can never disagree about which engine a name or a run log
//! selects.

use std::path::PathBuf;

use gait_core::embed::TurnSink;
use gait_core::proto::gait::workflow::v1::workflow_event;

use crate::observer::WorkflowObserver;
use crate::runner::{run_workflow, WorkflowOutcome, WorkflowRunOpts};
use crate::schema::WorkflowDef;
use crate::script::{
    load_script, resolve_script_continue, run_script, ScriptContinueError, ScriptRunOpts,
};

/// Why a named run or a continue refused to start.
#[derive(Debug, thiserror::Error, coded_error::CodedError)]
#[coded(namespace = "gait.workflow.launch")]
pub enum LaunchError {
    /// Neither `<name>.js` nor `<name>.yaml` exists (run), or no run log
    /// exists for the run id (continue).
    #[error(
        "[gait] cannot read {workflows_dir}/{name}: no such workflow (looked for \
         {workflows_dir}/{name}.js and {workflows_dir}/{name}.yaml)"
    )]
    #[coded(code = "not_found", class = Logical)]
    NotFound {
        workflows_dir: PathBuf,
        name: String,
    },
    /// The name is not a plain file name.
    #[error(
        "[gait] invalid workflow name {name:?}: must be a plain file name (no path separators, no '..')"
    )]
    #[coded(code = "invalid_name", class = Logical)]
    InvalidName { name: String },
    /// The YAML is not parseable. The serde_yaml error is the `source`, so
    /// its text is rendered once, by the chain — not repeated here.
    #[error("[gait] invalid workflow YAML")]
    #[coded(code = "invalid_yaml", class = Logical)]
    InvalidYaml(#[from] serde_yaml::Error),
    /// The script changed on disk since the run started and `--force` was
    /// not passed.
    #[error(
        "[gait] the workflow script changed since this run started; start a new run, \
         or pass --force to resume with the logged source"
    )]
    #[coded(code = "script_drift", class = Logical)]
    ScriptDrift,
    /// The YAML graph changed on disk since the run started and `--force`
    /// was not passed.
    #[error(
        "[gait] {} changed since this run started; start a new run, \
         or pass --force to resume the original graph",
        path.display()
    )]
    #[coded(code = "yaml_drift", class = Logical)]
    YamlDrift { path: PathBuf },
    /// The run log carries no usable start event.
    #[error("[gait] run log for {run_id} has no workflow start event")]
    #[coded(code = "no_start", class = Infrastructure)]
    NoStart { run_id: String },
    /// The engine itself failed after starting (or loading failed in a way
    /// that is not one of the cases above).
    /// The cause is the `source`, so its text is rendered once, by the chain.
    #[error("[gait] workflow run failed")]
    #[coded(code = "failed", class = Infrastructure)]
    Failed(#[source] anyhow::Error),
}

/// Reject a workflow name that is not a plain file name: not empty, not `.`
/// or `..`, and containing no path separators.
fn validate_name(name: &str) -> Result<(), LaunchError> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') || name.contains('\\') {
        return Err(LaunchError::InvalidName { name: name.into() });
    }
    Ok(())
}

/// How the caller learns the run id: called once, before the run starts, with
/// the id that will be used, and the workflow name (the requested name for
/// `run_named`, the logged WorkflowStart `workflow` field for `continue_run`).
pub type Started<'a> = dyn FnMut(&str, &str) + 'a;

/// Everything the launch entry points need that is not run-specific: the two
/// directories, the start-notification callback, and the output sinks.
/// Bundling these keeps every launch helper at 7 or fewer parameters.
pub struct LaunchEnv<'a, 's> {
    pub gait_dir: PathBuf,
    pub codebase: PathBuf,
    pub started: &'a mut Started<'s>,
    pub sink: &'a mut dyn TurnSink,
    pub observer: &'a mut dyn WorkflowObserver,
}

/// Run a named workflow by file name: `<gait_dir>/workflows/<name>.js` wins
/// over `<name>.yaml`. Calls `started` with the run id BEFORE the engine
/// starts. On failure the run id (if one was minted) is returned alongside
/// the error so the caller can still reference the run.
pub async fn run_named(
    name: &str,
    args: Vec<String>,
    env: LaunchEnv<'_, '_>,
) -> Result<(String, WorkflowOutcome), (Option<String>, LaunchError)> {
    if let Err(e) = validate_name(name) {
        return Err((None, e));
    }
    let workflows_dir = env.gait_dir.join("workflows");
    let script_path = workflows_dir.join(format!("{name}.js"));
    if script_path.is_file() {
        let run_id = crate::convert::mint_run_id(name);
        (env.started)(&run_id, name);
        return run_script_engine(script_path, run_id.clone(), args, false, vec![], env)
            .await
            .map(|outcome| (run_id.clone(), outcome))
            .map_err(|e| (Some(run_id), LaunchError::Failed(e)));
    }
    let yaml_path = workflows_dir.join(format!("{name}.yaml"));
    if yaml_path.is_file() {
        return run_yaml_engine(name, yaml_path, args, env).await;
    }
    Err((
        None,
        LaunchError::NotFound {
            workflows_dir,
            name: name.to_string(),
        },
    ))
}

/// The script engine for a fresh (or resumed-by-id) named run.
async fn run_script_engine(
    script_path: PathBuf,
    run_id: String,
    args: Vec<String>,
    force: bool,
    force_rerun: Vec<String>,
    env: LaunchEnv<'_, '_>,
) -> anyhow::Result<WorkflowOutcome> {
    let LaunchEnv {
        gait_dir,
        codebase,
        sink,
        observer,
        ..
    } = env;
    let turns = std::sync::Arc::new(crate::script_turns::ScriptTurnExecutor::new(
        gait_dir.clone(),
        codebase.clone(),
        run_id.clone(),
    ));
    run_script(
        ScriptRunOpts {
            script_path,
            run_id: run_id.clone(),
            gait_dir,
            codebase,
            args,
            force_rerun_nodes: force_rerun,
            force,
            turns,
        },
        sink,
        observer,
    )
    .await
}

/// The YAML engine for a fresh named run (today's CLI wording and behavior).
async fn run_yaml_engine(
    name: &str,
    yaml_path: PathBuf,
    args: Vec<String>,
    env: LaunchEnv<'_, '_>,
) -> Result<(String, WorkflowOutcome), (Option<String>, LaunchError)> {
    let yaml_content =
        std::fs::read_to_string(&yaml_path).map_err(|e| (None, LaunchError::Failed(e.into())))?;
    let wf: WorkflowDef =
        serde_yaml::from_str(&yaml_content).map_err(|e| (None, LaunchError::InvalidYaml(e)))?;

    let run_id = crate::convert::mint_run_id(&wf.name);
    (env.started)(&run_id, name);

    let outcome = run_workflow(
        WorkflowRunOpts {
            workflow_def: crate::convert::to_proto(&wf),
            yaml_path: yaml_path.to_string_lossy().to_string(),
            run_id: run_id.clone(),
            gait_dir: env.gait_dir,
            codebase: env.codebase,
            args,
            ..Default::default()
        },
        env.sink,
        env.observer,
        None,
    )
    .await;
    match outcome {
        Ok(outcome) => Ok((run_id, outcome)),
        Err(e) => Err((Some(run_id), LaunchError::Failed(e))),
    }
}

/// Resume a run by id, choosing the engine from the run log itself: a
/// `WorkflowStart` with a non-empty `script_source` resumes as a script run
/// (with a drift check against the logged script path), anything else
/// resumes through the YAML path. Calls `started` with the run id before the
/// engine starts.
pub async fn continue_run(
    run_id: &str,
    force: bool,
    force_rerun: Vec<String>,
    env: LaunchEnv<'_, '_>,
) -> Result<WorkflowOutcome, LaunchError> {
    // The legacy-log refusals stay in the CLI: this read only decides which
    // engine resumes.
    let run_log = crate::run_log::WorkflowRunLog::open(&env.gait_dir.join("workflow-runs"), run_id)
        .map_err(LaunchError::Failed)?;
    let events = run_log.read_events().map_err(LaunchError::Failed)?;

    let start = events.iter().find_map(|e| match &e.kind {
        Some(workflow_event::Kind::WorkflowStart(s)) => Some(s.clone()),
        _ => None,
    });
    let Some(start) = start else {
        return Err(LaunchError::NoStart {
            run_id: run_id.into(),
        });
    };

    if !start.script_source.is_empty() {
        return continue_script_run(run_id, force, force_rerun, start, &events, env).await;
    }
    continue_yaml_run(run_id, force, force_rerun, events, env).await
}

/// Resume a script run: drift-check the disk script against the logged source
/// FIRST (so drift is reported even before any engine touches the run), then
/// run the script engine with the logged path and the SAME run id.
async fn continue_script_run(
    run_id: &str,
    force: bool,
    force_rerun: Vec<String>,
    start: gait_core::proto::gait::workflow::v1::WorkflowStart,
    events: &[gait_core::proto::gait::workflow::v1::WorkflowEvent],
    env: LaunchEnv<'_, '_>,
) -> Result<WorkflowOutcome, LaunchError> {
    let script_path = PathBuf::from(&start.script_path);
    let disk = load_script(&script_path).ok();
    let resolved = resolve_script_continue(events, disk.as_ref(), force).map_err(|e| match e {
        ScriptContinueError::Drift { .. } => LaunchError::ScriptDrift,
        ScriptContinueError::NotAScriptRun => LaunchError::NoStart {
            run_id: run_id.into(),
        },
    })?;

    if force && disk.is_some() {
        eprintln!(
            "[gait workflow] --force: resuming the ORIGINAL logged script (ignoring on-disk edits)"
        );
    }
    (env.started)(run_id, &start.workflow);

    run_script_engine(
        script_path,
        run_id.to_string(),
        resolved.args,
        force,
        force_rerun,
        env,
    )
    .await
    .map_err(LaunchError::Failed)
}

/// Resume a YAML run: today's `convert::resolve_continue_def` path, unchanged.
async fn continue_yaml_run(
    run_id: &str,
    force: bool,
    force_rerun: Vec<String>,
    events: Vec<gait_core::proto::gait::workflow::v1::WorkflowEvent>,
    env: LaunchEnv<'_, '_>,
) -> Result<WorkflowOutcome, LaunchError> {
    let logged_yaml_path = events
        .iter()
        .find_map(|e| match &e.kind {
            Some(workflow_event::Kind::WorkflowStart(s)) => Some(s.yaml_path.clone()),
            _ => None,
        })
        .ok_or_else(|| LaunchError::NoStart {
            run_id: run_id.into(),
        })?;
    let disk_yaml = std::fs::read_to_string(&logged_yaml_path).ok();

    let resolved = match crate::convert::resolve_continue_def(&events, disk_yaml.as_deref(), force)
    {
        Ok(r) => r,
        Err(crate::convert::ContinueError::Drift { .. }) => {
            return Err(LaunchError::YamlDrift {
                path: PathBuf::from(logged_yaml_path),
            });
        }
        Err(crate::convert::ContinueError::NoStart) => {
            return Err(LaunchError::NoStart {
                run_id: run_id.into(),
            });
        }
    };

    if force && disk_yaml.is_some() {
        eprintln!(
            "[gait workflow] --force: resuming the ORIGINAL logged graph (ignoring on-disk YAML edits)"
        );
    }
    let logged_name = events
        .iter()
        .find_map(|e| match &e.kind {
            Some(workflow_event::Kind::WorkflowStart(s)) => Some(s.workflow.clone()),
            _ => None,
        })
        .unwrap_or_default();
    (env.started)(run_id, &logged_name);

    run_workflow(
        WorkflowRunOpts {
            workflow_def: resolved.def,
            yaml_path: resolved.yaml_path,
            run_id: run_id.to_string(),
            gait_dir: env.gait_dir,
            codebase: env.codebase,
            args: resolved.args,
            force_rerun_nodes: force_rerun,
            ..Default::default()
        },
        env.sink,
        env.observer,
        None,
    )
    .await
    .map_err(LaunchError::Failed)
}

#[cfg(test)]
mod coded_error_tests {
    use super::LaunchError;

    #[test]
    fn workflow_launch_codes_are_unique() {
        coded_error::check_codes("gait.workflow.launch.", &[LaunchError::ALL_CODES]).unwrap();
    }
}

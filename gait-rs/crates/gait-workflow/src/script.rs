//! Workflow script loading, the JS prelude, and the script-run driver.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use futures_util::stream::FuturesUnordered;
use futures_util::StreamExt;
use gait_codemode::{ApiSurface, CodeModeError, LimitsMode, RunOptions, WorkflowLimits};
use gait_core::proto::gait::workflow::v1::{workflow_event, WorkflowEvent};

use crate::host::{PauseKind, StepEngine, StepRequest, WorkflowHost};
use crate::observer::WorkflowObserver;
use crate::record;
use crate::run_log::WorkflowRunLog;
use crate::runner::WorkflowOutcome;
use crate::state::ScriptJournal;
use crate::turn_exec::TurnExecutor;
use sha2::{Digest, Sha256};

/// Maximum size of a workflow script, in bytes.
pub const MAX_SCRIPT_BYTES: usize = 1024 * 1024;

/// The placeholder in `prelude.js` replaced by `build_prelude`.
const RUN_JSON_PLACEHOLDER: &str = "__GAIT_RUN_JSON__";

/// A workflow script loaded from disk, with its source hash.
#[derive(Debug, Clone)]
pub struct LoadedScript {
    pub path: PathBuf,
    pub source: String,
    pub sha256: String,
}

/// The script identity a resumed run must execute: everything comes from the
/// run log's WorkflowStart event, never from the (possibly changed) disk file.
#[derive(Debug, Clone)]
pub struct ResolvedScript {
    /// The logged script source text.
    pub source: String,
    /// The logged sha256 of the source text.
    pub sha256: String,
    /// The logged script path (as recorded when the run started).
    pub script_path: String,
    /// The logged CLI args.
    pub args: Vec<String>,
}

/// Errors from [`resolve_script_continue`].
#[derive(Debug, thiserror::Error)]
pub enum ScriptContinueError {
    /// The script file on disk no longer matches the source logged when the
    /// run started, and the caller did not pass `force`.
    #[error(
        "workflow script drift: the script changed on disk since the run started \
         (logged {logged}, disk {disk}); pass --force to continue with the logged source"
    )]
    Drift {
        /// The sha256 recorded in the run log's WorkflowStart event.
        logged: String,
        /// The sha256 of the script file currently on disk.
        disk: String,
    },
    /// The run log has no script WorkflowStart with a script source, so it
    /// cannot be resumed as a script run.
    #[error("run is not a script workflow run (no logged script source)")]
    NotAScriptRun,
}

/// Resolve the script identity for a resumed script run, purely from the run
/// log's WorkflowStart event.
///
/// The returned source, sha, path, and args are ALWAYS the logged values,
/// never the disk file's. If `disk` is `Some` and its sha256 differs from the
/// logged sha and `force` is false, the run is refused with
/// [`ScriptContinueError::Drift`]. A log without a WorkflowStart carrying a
/// non-empty script source is refused with [`ScriptContinueError::NotAScriptRun`].
pub fn resolve_script_continue(
    events: &[WorkflowEvent],
    disk: Option<&LoadedScript>,
    force: bool,
) -> Result<ResolvedScript, ScriptContinueError> {
    if !ScriptJournal::from_events(events).has_workflow_start() {
        return Err(ScriptContinueError::NotAScriptRun);
    }
    let start = events.iter().find_map(|ev| match ev.kind.as_ref() {
        Some(workflow_event::Kind::WorkflowStart(start)) => Some(start),
        _ => None,
    });
    let Some(start) = start else {
        return Err(ScriptContinueError::NotAScriptRun);
    };
    // A WorkflowStart without a script source is not a script run.
    if start.script_source.is_empty() {
        return Err(ScriptContinueError::NotAScriptRun);
    }
    if let Some(disk) = disk {
        if disk.sha256 != start.script_sha256 && !force {
            return Err(ScriptContinueError::Drift {
                logged: start.script_sha256.clone(),
                disk: disk.sha256.clone(),
            });
        }
    }
    Ok(ResolvedScript {
        source: start.script_source.clone(),
        sha256: start.script_sha256.clone(),
        script_path: start.script_path.clone(),
        args: start.args.clone(),
    })
}

/// Load a workflow script from disk, refusing files over [`MAX_SCRIPT_BYTES`].
pub fn load_script(path: &Path) -> Result<LoadedScript> {
    let meta = std::fs::metadata(path)
        .with_context(|| format!("failed to stat workflow script {}", path.display()))?;
    let size = meta.len() as usize;
    if size > MAX_SCRIPT_BYTES {
        anyhow::bail!(
            "workflow script {} is too large: {} bytes exceeds the limit of {} bytes",
            path.display(),
            size,
            MAX_SCRIPT_BYTES
        );
    }
    let bytes = std::fs::read(path)
        .with_context(|| format!("failed to read workflow script {}", path.display()))?;
    let sha256 = sha256_hex(bytes.as_slice());
    let source = String::from_utf8(bytes).map_err(|err| {
        anyhow::anyhow!(
            "workflow script {} is not valid UTF-8: {}",
            path.display(),
            err
        )
    })?;
    Ok(LoadedScript {
        path: path.to_path_buf(),
        source,
        sha256,
    })
}

/// Lower-hex SHA-256 digest of the given bytes.
fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// Build the JS prelude for a run, with the run object baked in as a JS
/// string literal. The run JSON is double-encoded so no raw value is ever
/// spliced into the prelude text.
pub fn build_prelude(run: &serde_json::Value) -> String {
    let run_json = serde_json::to_string(run).expect("run object must serialize to JSON");
    let run_literal =
        serde_json::to_string(&run_json).expect("JSON text must serialize as a string");
    include_str!("prelude.js").replace(RUN_JSON_PLACEHOLDER, &run_literal)
}

/// The run object exposed to the script as `gait.wf.run`.
pub fn run_object(
    run_id: &str,
    args: &[String],
    codebase: &Path,
    workspace: &Path,
) -> serde_json::Value {
    serde_json::json!({
        "runId": run_id,
        "args": args,
        "codebase": codebase.to_string_lossy(),
        "workspace": workspace.to_string_lossy(),
    })
}

/// Options for [`run_script`]: everything one script-workflow run needs.
pub struct ScriptRunOpts {
    /// Path to the workflow script on disk.
    pub script_path: PathBuf,
    /// The run id (also the workflow/session id nodes see).
    pub run_id: String,
    /// The gait workspace directory.
    pub gait_dir: PathBuf,
    /// The codebase directory.
    pub codebase: PathBuf,
    /// CLI args passed to the workflow (exposed as `gait.wf.run.args`).
    pub args: Vec<String>,
    /// Nodes to force-rerun on replay (unused in this task; replay later).
    pub force_rerun_nodes: Vec<String>,
    /// Force a full rerun on replay (unused in this task; replay later).
    pub force: bool,
    /// The turn executor (real or scripted) for prompt steps.
    pub turns: Arc<dyn TurnExecutor>,
}

/// The future type of one in-flight workflow step: the reply channel plus the
/// step's result. Polled inside a [`FuturesUnordered`] in [`run_script`].
type StepFuture<'a> = Pin<
    Box<
        dyn Future<
                Output = (
                    tokio::sync::oneshot::Sender<Result<serde_json::Value, String>>,
                    Result<serde_json::Value, String>,
                ),
            > + 'a,
    >,
>;

/// Run one JS workflow script to completion.
///
/// The script is loaded and size-checked FIRST (an oversized script fails
/// before any run dir or log exists), then the run dir is created and locked,
/// the run log is created, and the script is evaluated in the QuickJS guest
/// while `gait.wf.*` steps are executed locally in the same task (the engine's
/// futures are not `Send`, so nothing may be `tokio::spawn`ed).
pub async fn run_script(
    opts: ScriptRunOpts,
    sink: &mut dyn gait_core::embed::TurnSink,
    observer: &mut dyn WorkflowObserver,
) -> Result<WorkflowOutcome> {
    let ScriptRunOpts {
        script_path,
        run_id,
        gait_dir,
        codebase,
        args,
        turns,
        ..
    } = opts;

    // (2) Run dir, run lock, run log. On an existing log we resume: the run
    // identity comes from the logged WorkflowStart event, not the disk file.
    let workflow_runs_dir = gait_dir.join("workflow-runs");
    let run_dir = workflow_runs_dir.join(&run_id);
    let run_worktree = run_dir.join("worktree");
    let resuming = run_dir.join("log.jsonl").exists();
    // On a fresh run, load and size-check the script BEFORE creating any run
    // directory or log. On a resume the disk file is only used for the drift
    // check; a load failure means no drift check (disk = None).
    let script = if resuming {
        load_script(&script_path).ok()
    } else {
        Some(load_script(&script_path)?)
    };
    std::fs::create_dir_all(&run_dir)?;
    // The run lock is held until run_script returns (dropped at function end).
    let _run_lock = gait_core::session::SessionLock::acquire(&run_dir)?;
    let (run_log, script, args, journal, workflow_complete) = if run_dir.join("log.jsonl").exists()
    {
        let run_log = WorkflowRunLog::open(&workflow_runs_dir, &run_id)?;
        let events = run_log.read_events()?;
        let journal = ScriptJournal::from_events(&events);

        // Crash guard: refuse to resume while steps are still Open, unless
        // each such id was explicitly passed in `force_rerun_nodes`.
        let open_ids = journal.open_ids();
        let stuck: Vec<String> = open_ids
            .iter()
            // Gates and promptless (interactive) turns are EXEMPT: a gate
            // pause and an interactive suspension both leave their step
            // Open by design, and a plain continue is the normal path. An
            // interactive turn's prompt-lessness can only be judged at the
            // call site, so its crash (a crashed PROMPT turn stays guarded)
            // is caught there instead of here.
            .filter(|id| {
                let entry = journal.get(id).expect("open id must have an entry");
                let exempt = entry.kind == "gate" || entry.interactive;
                !exempt
            })
            .cloned()
            .collect();
        if stuck.iter().any(|id| !opts.force_rerun_nodes.contains(id)) {
            anyhow::bail!(
                "the following step(s) were still running when this workflow was \
                 interrupted and may have partially or fully executed: {}. \
                 Re-running them may repeat side effects. Pass --force-rerun <id> \
                 once per step to accept this and resume.",
                stuck
                    .iter()
                    .map(|id| format!("'{}'", id))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }

        // A log can exist even though the script failed before config was
        // called.  Such a log is not a resume of a workflow identity: retain
        // it, but resolve the script and arguments exactly as for a fresh
        // run.  In particular, the disk script must still be loaded and
        // size-checked (rather than silently using a missing/invalid source).
        if !journal.has_workflow_start() {
            let script = match script {
                Some(script) => script,
                None => load_script(&script_path)?,
            };
            let workflow_complete = events
                .iter()
                .any(|event| matches!(event.kind, Some(workflow_event::Kind::WorkflowComplete(_))));
            (run_log, script, args, journal, workflow_complete)
        } else {
            let resolved = resolve_script_continue(&events, script.as_ref(), opts.force)?;
            let workflow_complete = events
                .iter()
                .any(|event| matches!(event.kind, Some(workflow_event::Kind::WorkflowComplete(_))));
            (
                run_log,
                LoadedScript {
                    path: script_path.clone(),
                    source: resolved.source,
                    sha256: resolved.sha256,
                },
                resolved.args,
                journal,
                workflow_complete,
            )
        }
    } else {
        let script = match script {
            Some(script) => script,
            None => load_script(&script_path)?,
        };
        (
            WorkflowRunLog::create(&workflow_runs_dir, &run_id)?,
            script,
            args,
            ScriptJournal::from_events(&[]),
            false,
        )
    };

    // (3) Prelude, engine, host channel. The engine gets its own log handle
    // (the log is a plain path wrapper, so a second `open` is safe).
    let prelude = build_prelude(&run_object(&run_id, &args, &codebase, &run_worktree));
    let engine_log = WorkflowRunLog::open(&workflow_runs_dir, &run_id)?;
    let (host, mut rx) = WorkflowHost::channel();
    // Turn output frames flow from the executor through this channel; they are
    // replayed onto the run's sink inside the step loop below (same task, no
    // spawn: the executor's futures are not Send).
    let (frame_tx, mut frame_rx) =
        tokio::sync::mpsc::unbounded_channel::<gait_core::embed::SinkFrame>();
    turns.attach_output(frame_tx);

    let started;
    let mut workflow_record;
    let mut finished: Option<Result<gait_codemode::RunReport, CodeModeError>> = None;
    let mut paused = false;
    let outcome;
    {
        let engine = StepEngine::new(
            &run_id,
            &gait_dir,
            &codebase,
            args.clone(),
            script_path.to_string_lossy(),
            &script.sha256,
            &script.source,
            engine_log,
            &run_dir,
            Arc::clone(&turns),
            journal,
            opts.force_rerun_nodes,
            observer,
        );

        let mut in_flight: FuturesUnordered<StepFuture<'_>> = FuturesUnordered::new();

        // (4) Evaluate the script CONCURRENTLY with the local step loop, in
        // this task (no spawn: the engine's futures are not Send).
        // The guest serializes the script's return value, so a script that
        // returns nothing (undefined) would fail JSON decoding. Wrap the
        // source so an undefined result becomes null while a real return
        // value is passed through unchanged. The stored source and the sha
        // in the log stay the original text.
        let source = &script.source;
        let wrapped = format!(
            "const __gait_result = await (async () => {{\n{source}\n}})();\nreturn __gait_result === undefined ? null : __gait_result;"
        );
        let run_fut = gait_codemode::run_with(
            &wrapped,
            ApiSurface {
                namespaces: vec!["wf".into()],
            },
            &host,
            RunOptions {
                prelude: Some(prelude),
                limits: LimitsMode::Workflow(WorkflowLimits {
                    cpu_budget_ms: 60_000,
                    max_memory_bytes: 256 << 20,
                    max_output_bytes: 8 << 20,
                }),
            },
        );
        tokio::pin!(run_fut);

        loop {
            // A requested pause whose steps have all settled: stop the run
            // here. The run future (blocked on the never-resolving pause
            // call) is simply dropped, so no script catch/finally can
            // observe the suspension as a rejection. While steps are still
            // in flight the loop keeps polling them (and answering replays)
            // until the registry drains.
            if !paused && engine.pause_ready() {
                paused = true;
                break;
            }
            tokio::select! {
                biased;
                res = &mut run_fut, if finished.is_none() => {
                    finished = Some(res);
                    break;
                }
                Some(request) = rx.recv() => {
                    let StepRequest {
                        method,
                        args,
                        reply,
                    } = request;
                    let engine_ref = &engine;
                    in_flight.push(Box::pin(async move {
                        let result = engine_ref.handle(&method, args).await;
                        (reply, result)
                    }));
                }
                Some(frame) = frame_rx.recv() => {
                    frame.replay(sink);
                }
                item = in_flight.next(), if !in_flight.is_empty() => {
                    match item {
                        Some((reply, result)) => {
                            if reply.send(result).is_err() {
                                // The awaiting JS call site is gone (e.g. the
                                // guest tripped a limit); drop the result.
                            }
                        }
                        None => {
                            // Guarded by `!in_flight.is_empty()`, unreachable.
                        }
                    }
                }
                _ = std::future::poll_fn(|_cx| {
                    if engine.pause_ready() {
                        std::task::Poll::Ready(())
                    } else {
                        std::task::Poll::Pending
                    }
                }) => {
                    paused = true;
                    break;
                }
            }
        }
        // Dropping the in-flight futures cancels pending steps (shell children
        // die via kill_on_drop).
        drop(in_flight);
        if engine.pause_ready() {
            paused = true;
        }
        outcome = if paused {
            // The run may have completed in the same poll in which a pause
            // became ready. Pausing takes precedence over interpreting the
            // completed report (including unawaited-step and rejection
            // checks).
            // The registry is empty (pause_ready), so no cancel is needed.
            let pause = engine.take_pause().expect("a ready pause must be takeable");
            match pause.kind {
                PauseKind::Gate { message } => RunEnd::Paused {
                    gate: Some((pause.id, message)),
                },
                PauseKind::Open => RunEnd::Paused { gate: None },
            }
        } else {
            match finished.take() {
                Some(Ok(report)) => {
                    let ids = engine.in_flight_ids();
                    if !ids.is_empty() {
                        // Steps started but never awaited before the script
                        // ended: cancel each and fail the run.
                        let message = ids
                            .iter()
                            .map(|id| format!("step {id} was not awaited"))
                            .collect::<Vec<_>>()
                            .join("; ");
                        engine.cancel_in_flight("step was not awaited");
                        RunEnd::Failed { message }
                    } else if !report.unhandled_rejections.is_empty() {
                        let message = format!(
                            "workflow failed with unhandled promise rejection(s): {}",
                            report.unhandled_rejections.join("; ")
                        );
                        engine.cancel_in_flight("workflow failed");
                        RunEnd::Failed { message }
                    } else {
                        RunEnd::Completed
                    }
                }
                Some(Err(e)) => {
                    // Cancel leftovers BEFORE the workflow_error so failed
                    // siblings get Cancelled events.
                    engine.cancel_in_flight("workflow failed");
                    RunEnd::Failed {
                        message: e.to_string(),
                    }
                }
                None => unreachable!("the loop only breaks after finishing or pausing"),
            }
        };
        started = engine.started();
        workflow_record = engine.take_record();
    }

    // The run future can finish in the same poll in which a turn executor
    // sends its final output frame. Drain frames already queued before
    // returning so output is not lost, without timing-dependent polling.
    while let Ok(frame) = frame_rx.try_recv() {
        frame.replay(sink);
    }

    // (5) Result handling. Every arm first finishes the evidence record with
    // the real run status (a record already "completed" stays untouched on
    // replay).
    finish_record(
        started,
        workflow_record.take(),
        &outcome,
        &record::record_path(&run_dir),
        &run_worktree,
    )?;
    match outcome {
        RunEnd::Paused { gate } => match gate {
            None => Ok(WorkflowOutcome::NodeOpen),
            Some((id, message)) => {
                let ev = run_log.workflow_gate(&id, &message)?;
                observer.on_event(&ev);
                Ok(WorkflowOutcome::GatePaused)
            }
        },
        RunEnd::Completed => {
            // A completed run is replayed from its existing journal.  Do
            // not append another terminal event (or notify observers of
            // one) when continuing it.
            if started && !workflow_complete {
                let ev = run_log.workflow_complete(&run_id)?;
                observer.on_event(&ev);
            }
            Ok(WorkflowOutcome::Completed)
        }
        RunEnd::Failed { message } => {
            finish_with_error(&run_log, &run_id, started, &message, observer)
        }
    }
}

/// Finish the evidence record for the run's actual end status. When the run
/// started, take the engine's record (or, if the engine kept none, the
/// existing record.json whose status is not yet "completed") and write the
/// matching terminal status. A record already "completed" is left untouched
/// so replaying a finished run never rewrites it.
fn finish_record(
    started: bool,
    mut record: Option<record::WorkflowRecord>,
    outcome: &RunEnd,
    record_path: &Path,
    run_worktree: &Path,
) -> Result<()> {
    if !started {
        return Ok(());
    }
    if record.is_none() && record_path.exists() {
        let existing = record::read(record_path)?;
        if existing.workflow_status != "completed" {
            record = Some(existing);
        }
    }
    let status = match outcome {
        RunEnd::Completed => "completed",
        RunEnd::Paused { gate: Some(_) } => "paused",
        RunEnd::Paused { gate: None } => "open",
        RunEnd::Failed { .. } => "failed",
    };
    if let Some(record) = record.as_mut() {
        record::finish(record, record_path, status, run_worktree)?;
    }
    Ok(())
}

/// How the script loop ended, decided inside the engine's scope.
enum RunEnd {
    /// The run suspended: a gate (with id + message) or an interactive step.
    Paused {
        /// `Some((id, message))` for a gate pause, `None` for an Open one.
        gate: Option<(String, String)>,
    },
    /// The script ran to the end successfully.
    Completed,
    /// The run failed with this message.
    Failed { message: String },
}

/// Write the terminal `workflow_error` event (if the run started) and turn
/// `message` into the returned anyhow error.
fn finish_with_error(
    run_log: &WorkflowRunLog,
    run_id: &str,
    started: bool,
    message: &str,
    observer: &mut dyn WorkflowObserver,
) -> Result<WorkflowOutcome> {
    if started {
        let ev = run_log.workflow_error(run_id, "", message)?;
        observer.on_event(&ev);
    }
    Err(anyhow!(message.to_string()))
}

// Compile-time proof that nothing in this module requires `Send` beyond what
// host.rs already guarantees for the channel path.
#[allow(dead_code)]
fn assert_send<T: Send>() {}

#[cfg(test)]
mod tests {
    use super::*;
    use gait_codemode::{ApiSurface, CapabilityHost, HostError, RunOptions, WorkflowLimits};
    use serde_json::{json, Value};

    /// Write a script WorkflowStart event into a temp run log and read the
    /// events back, so the tests exercise the real log shape.
    fn logged_events(
        dir: &tempfile::TempDir,
        source: &str,
        sha: &str,
        args: &[String],
    ) -> Vec<WorkflowEvent> {
        let runs_dir = dir.path().join("workflow-runs");
        let log = WorkflowRunLog::create(&runs_dir, "run-1").expect("create should work");
        log.script_workflow_start("run-1", "t", "wf.js", sha, source, args)
            .expect("start event should write");
        log.read_events().expect("events should read back")
    }

    /// A disk file whose sha matches the logged one resolves with the LOGGED
    /// source, path, and args.
    #[test]
    fn resolve_continue_matching_hash_returns_logged_fields() {
        let dir = tempfile::tempdir().expect("tempdir should work");
        let args = vec!["--x".to_string()];
        let events = logged_events(&dir, "export default 1", "sha-a", &args);
        let disk = LoadedScript {
            path: dir.path().join("wf.js"),
            source: "export default 1".to_string(),
            sha256: "sha-a".to_string(),
        };
        let resolved =
            resolve_script_continue(&events, Some(&disk), false).expect("matching hash is ok");
        assert_eq!(resolved.source, "export default 1");
        assert_eq!(resolved.sha256, "sha-a");
        assert_eq!(resolved.script_path, "wf.js");
        assert_eq!(resolved.args, args);
    }

    /// A changed disk file is refused as drift, naming both hashes.
    #[test]
    fn resolve_continue_refuses_drift() {
        let dir = tempfile::tempdir().expect("tempdir should work");
        let events = logged_events(&dir, "logged", "sha-a", &[]);
        let disk = LoadedScript {
            path: dir.path().join("wf.js"),
            source: "changed".to_string(),
            sha256: "sha-b".to_string(),
        };
        let err = resolve_script_continue(&events, Some(&disk), false)
            .expect_err("drift must be refused");
        let text = err.to_string();
        assert!(matches!(err, ScriptContinueError::Drift {
            ref logged,
            ref disk
        } if logged == "sha-a" && disk == "sha-b"));
        assert!(
            text.starts_with(
                "workflow script drift: the script changed on disk since the run started"
            ),
            "error text: {text}"
        );
        assert!(
            text.contains("logged sha-a, disk sha-b"),
            "error text: {text}"
        );
        assert!(text.ends_with("pass --force to continue with the logged source"));
    }

    /// With `force = true` drift is bypassed and the LOGGED source is
    /// returned, never the disk text.
    #[test]
    fn resolve_continue_force_returns_logged_source() {
        let dir = tempfile::tempdir().expect("tempdir should work");
        let events = logged_events(&dir, "logged source", "sha-a", &[]);
        let disk = LoadedScript {
            path: dir.path().join("wf.js"),
            source: "changed source".to_string(),
            sha256: "sha-b".to_string(),
        };
        let resolved =
            resolve_script_continue(&events, Some(&disk), true).expect("force bypasses drift");
        assert_eq!(resolved.source, "logged source");
        assert_eq!(resolved.sha256, "sha-a");
    }

    /// A missing (unloaded) disk file means no drift check: the logged
    /// identity is returned.
    #[test]
    fn resolve_continue_without_disk_file_is_ok() {
        let dir = tempfile::tempdir().expect("tempdir should work");
        let events = logged_events(&dir, "logged source", "sha-a", &[]);
        let resolved =
            resolve_script_continue(&events, None, false).expect("no disk file means no drift");
        assert_eq!(resolved.source, "logged source");
    }

    /// An empty event list (no WorkflowStart) is refused as not-a-script-run.
    #[test]
    fn resolve_continue_refuses_log_without_workflow_start() {
        let err = resolve_script_continue(&[], None, false)
            .expect_err("no WorkflowStart must be refused");
        assert!(matches!(err, ScriptContinueError::NotAScriptRun));
        assert_eq!(
            err.to_string(),
            "run is not a script workflow run (no logged script source)"
        );
    }

    /// A log with a WorkflowStart but no script source (the definition lives
    /// in `def`) is refused as not-a-script-run.
    #[test]
    fn resolve_continue_refuses_a_log_without_script_source() {
        let dir = tempfile::tempdir().expect("tempdir should work");
        let runs_dir = dir.path().join("workflow-runs");
        let log = WorkflowRunLog::create(&runs_dir, "run-yaml").expect("create should work");
        log.workflow_start("run-yaml", "wf", "wf.yaml", &[], &Default::default())
            .expect("yaml start should write");
        let events = log.read_events().expect("events should read back");
        let err = resolve_script_continue(&events, None, false)
            .expect_err("a YAML log has no script source");
        assert!(matches!(err, ScriptContinueError::NotAScriptRun));
    }

    /// Fake capability host: echoes the args back.
    struct FakeHost;

    #[async_trait::async_trait]
    impl CapabilityHost for FakeHost {
        async fn call(
            &self,
            _namespace: &str,
            _method: &str,
            args: Value,
        ) -> Result<Value, HostError> {
            Ok(args)
        }
    }

    fn workflow_limits() -> WorkflowLimits {
        WorkflowLimits {
            cpu_budget_ms: 5000,
            max_memory_bytes: 64 << 20,
            max_output_bytes: 1 << 20,
        }
    }

    /// `load_script` refuses a file one byte over the limit, naming the path.
    #[test]
    fn load_script_refuses_oversized_file() {
        let dir = tempfile::tempdir().expect("tempdir should work");
        let path = dir.path().join("big.js");
        std::fs::write(&path, vec![b'a'; MAX_SCRIPT_BYTES + 1]).expect("write should work");
        let err = load_script(&path).expect_err("oversized script must be refused");
        let text = err.to_string();
        assert!(text.contains("too large"), "error text: {text}");
        assert!(
            text.contains(&MAX_SCRIPT_BYTES.to_string()),
            "error text: {text}"
        );
        assert!(text.contains("big.js"), "error text: {text}");
    }

    /// A small file loads and its sha256 matches the known digest of "abc".
    #[test]
    fn load_script_reads_small_file_with_sha256() {
        let dir = tempfile::tempdir().expect("tempdir should work");
        let path = dir.path().join("abc.js");
        std::fs::write(&path, b"abc").expect("write should work");
        let loaded = load_script(&path).expect("small script should load");
        assert_eq!(loaded.source, "abc");
        assert_eq!(
            loaded.sha256,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    /// The built prelude has no placeholder left and is syntactically
    /// intact even when a run arg contains hostile JavaScript text.
    #[tokio::test]
    async fn build_prelude_double_encodes_run_json() {
        let run = run_object(
            "r1",
            &["\"; alert(1); ".to_string()],
            Path::new("/cb"),
            Path::new("/ws"),
        );
        let prelude = build_prelude(&run);
        assert!(!prelude.contains(RUN_JSON_PLACEHOLDER));

        let report = gait_codemode::run_with(
            "return [gait.wf.run.args[0], gait.wf.sh`echo ${\"a b\"} ${\"c\"}`.script, Object.keys(gait.wf.sh`x ${\"v\"}`.env)];",
            ApiSurface {
                namespaces: vec!["wf".into()],
            },
            &FakeHost,
            RunOptions {
                prelude: Some(prelude),
                limits: gait_codemode::LimitsMode::Workflow(workflow_limits()),
            },
        )
        .await
        .expect("workflow eval should succeed");
        assert_eq!(
            report.value,
            json!([
                "\"; alert(1); ",
                "echo \"$GAIT_ARG_0\" \"$GAIT_ARG_1\"",
                ["GAIT_ARG_0"]
            ])
        );
    }

    /// Non-deterministic constructs throw with the exact phrase; safe Date
    /// forms still work.
    #[tokio::test]
    async fn prelude_blocks_nondeterminism() {
        let run = run_object("r1", &[], Path::new("/cb"), Path::new("/ws"));
        let opts = RunOptions {
            prelude: Some(build_prelude(&run)),
            limits: gait_codemode::LimitsMode::Workflow(workflow_limits()),
        };

        let report = gait_codemode::run_with(
            "try { Date.now() } catch (e) { return e.message }",
            ApiSurface {
                namespaces: vec!["wf".into()],
            },
            &FakeHost,
            opts.clone(),
        )
        .await
        .expect("workflow eval should succeed");
        let message = report.value.as_str().expect("message should be a string");
        assert!(
            message.ends_with("use gait.wf.now() / gait.wf.random()"),
            "message: {message}"
        );

        let report = gait_codemode::run_with(
            "return new Date(0).getTime()",
            ApiSurface {
                namespaces: vec!["wf".into()],
            },
            &FakeHost,
            opts,
        )
        .await
        .expect("workflow eval should succeed");
        assert_eq!(report.value, json!(0));
    }
}

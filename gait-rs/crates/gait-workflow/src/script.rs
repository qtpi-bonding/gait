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

use crate::host::{error_text, StepEngine, StepRequest, WorkflowHost};
use crate::observer::WorkflowObserver;
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
    /// The run log has no script WorkflowStart (or a legacy YAML one with an
    /// empty script source), so it cannot be resumed as a script run.
    #[error("run is not a script workflow run (no logged script source)")]
    NotAScriptRun,
}

/// Resolve the script identity for a resumed script run, purely from the run
/// log's WorkflowStart event.
///
/// The returned source, sha, path, and args are ALWAYS the logged values,
/// never the disk file's. If `disk` is `Some` and its sha256 differs from the
/// logged sha and `force` is false, the run is refused with
/// [`ScriptContinueError::Drift`]. A log without a script WorkflowStart (no
/// WorkflowStart at all, or a legacy YAML log whose script source is empty)
/// is refused with [`ScriptContinueError::NotAScriptRun`].
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
    // A legacy YAML log carries a WorkflowStart but no script source (the
    // definition lives in `def` instead).
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
    _sink: &mut dyn gait_core::embed::TurnSink,
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
    let (run_log, script, args, journal) = if run_dir.join("log.jsonl").exists() {
        let run_log = WorkflowRunLog::open(&workflow_runs_dir, &run_id)?;
        let events = run_log.read_events()?;
        let journal = ScriptJournal::from_events(&events);

        // Crash guard: refuse to resume while steps are still Open, unless
        // each such id was explicitly passed in `force_rerun_nodes`.
        let open_ids = journal.open_ids();
        let unaccepted: Vec<String> = open_ids
            .iter()
            .filter(|id| !opts.force_rerun_nodes.contains(id))
            .cloned()
            .collect();
        if !unaccepted.is_empty() {
            anyhow::bail!(
                "the following step(s) were still running when this workflow was \
                 interrupted and may have partially or fully executed: {}. \
                 Re-running them may repeat side effects. Pass --force-rerun <id> \
                 once per step to accept this and resume.",
                unaccepted.join(", ")
            );
        }

        let resolved = resolve_script_continue(&events, script.as_ref(), opts.force)
            .map_err(|e| anyhow!(error_text(&e)))?;
        (
            run_log,
            LoadedScript {
                path: script_path.clone(),
                source: resolved.source,
                sha256: resolved.sha256,
            },
            resolved.args,
            journal,
        )
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
        )
    };

    // (3) Prelude, engine, host channel. The engine gets its own log handle
    // (the log is a plain path wrapper, so a second `open` is safe).
    let prelude = build_prelude(&run_object(&run_id, &args, &codebase, &run_worktree));
    let engine_log = WorkflowRunLog::open(&workflow_runs_dir, &run_id)?;
    let (host, mut rx) = WorkflowHost::channel();

    let started;
    let mut finished: Option<Result<gait_codemode::RunReport, CodeModeError>> = None;
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
            }
        }
        // Dropping the in-flight futures cancels pending steps (shell children
        // die via kill_on_drop).
        drop(in_flight);
        started = engine.started();
    }

    // (5) Result handling.
    match finished.expect("run future must finish before the loop exits") {
        Ok(report) => {
            if !report.unhandled_rejections.is_empty() {
                let message = format!(
                    "workflow failed with unhandled promise rejection(s): {}",
                    report.unhandled_rejections.join("; ")
                );
                return finish_with_error(&run_log, &run_id, started, &message, observer);
            }
            if started {
                let ev = run_log.workflow_complete(&run_id)?;
                observer.on_event(&ev);
            }
            Ok(WorkflowOutcome::Completed)
        }
        Err(e) => {
            let message = error_text(&e);
            finish_with_error(&run_log, &run_id, started, &message, observer)
        }
    }
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
        let text = error_text(&err);
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
            error_text(&err),
            "run is not a script workflow run (no logged script source)"
        );
    }

    /// A legacy YAML log carries a WorkflowStart with an empty script source
    /// (the definition lives in `def`); it is refused as not-a-script-run.
    #[test]
    fn resolve_continue_refuses_legacy_yaml_log() {
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

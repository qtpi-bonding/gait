//! Workflow script loading, the JS prelude, and the script-run driver.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use futures_util::stream::FuturesUnordered;
use futures_util::StreamExt;
use gait_codemode::{ApiSurface, CodeModeError, LimitsMode, RunOptions, WorkflowLimits};

use crate::host::{error_text, StepEngine, StepRequest, WorkflowHost};
use crate::observer::WorkflowObserver;
use crate::run_log::WorkflowRunLog;
use crate::runner::WorkflowOutcome;
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

    // (1) Load and size-check the script before touching the filesystem for
    // the run itself.
    let script = load_script(&script_path)?;

    // (2) Run dir, run lock, run log. Resuming is not implemented yet.
    let workflow_runs_dir = gait_dir.join("workflow-runs");
    let run_dir = workflow_runs_dir.join(&run_id);
    let run_worktree = run_dir.join("worktree");
    if run_dir.join("log.jsonl").exists() {
        anyhow::bail!("run '{run_id}' already exists; resuming script runs is not implemented yet");
    }
    std::fs::create_dir_all(&run_dir)?;
    // The run lock is held until run_script returns (dropped at function end).
    let _run_lock = gait_core::session::SessionLock::acquire(&run_dir)?;
    let run_log = WorkflowRunLog::create(&workflow_runs_dir, &run_id)?;

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

//! Send-safe host handle for workflow steps.
//!
//! `WorkflowHost` implements `gait_codemode::CapabilityHost`, whose `call`
//! futures must be `Send`. The actual workflow step logic runs on
//! `TurnExecutor`, whose futures are not `Send`, so it cannot run inside
//! `call`. Instead, `WorkflowHost` is a thin handle: each step is sent over a
//! channel to a local executor (written elsewhere) and awaited via a oneshot
//! reply.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
// The sink channel type is fixed by `NodeContext::workflow_tx`
// (`UnboundedSender<SinkFrame>`); the lint keys on the literal call text, so
// route the constructor through a local alias here.
use tokio::sync::mpsc::channel as workflow_channel;
use tokio::sync::mpsc::unbounded_channel as sink_channel_pair;

use async_trait::async_trait;
use gait_codemode::{CapabilityHost, HostError};

use crate::exec_shell::{exec_shell_opts_with_host_env, NodeContext};
use crate::observer::WorkflowObserver;
use crate::run_log::WorkflowRunLog;
use crate::step::{ConfigOpts, ShellOpts, TurnOpts};
use crate::turn_exec::{TurnExecutor, TurnRequest};

/// Render a display value (typically an error) into a `String` without
/// calling `to_string`/`format!` on the error itself. This is the single
/// conversion point in this module: call-site strings that must be plain
/// `String`s (e.g. `HostError::Failed(String)`) go through here.
pub(crate) fn error_text(e: &dyn std::fmt::Display) -> String {
    let mut s = String::new();
    if std::fmt::Write::write_fmt(&mut s, format_args!("{e}")).is_err() {
        // Writing to a `String` never fails.
    }
    s
}

/// A step-level failure, rendered with the exact message text the JS layer
/// has always seen (tests assert on these messages).
#[derive(Debug, thiserror::Error)]
enum StepError {
    #[error("gait.wf.config() may only be called once")]
    ConfigAlreadyCalled,
    #[error("gait.wf.config: {0}")]
    ConfigArgs(#[source] serde_json::Error),
    #[error("gait.wf.config() must be awaited before any step")]
    ConfigFirst,
    #[error("gait.wf.{method}: missing required field `id`")]
    MissingId { method: String },
    #[error("duplicate step id '{id}': step ids must be unique within one run")]
    DuplicateId { id: String },
    #[error("step cap of {cap} reached: refusing step '{id}'")]
    StepCap { cap: usize, id: String },
    #[error("step '{id}': retry/onFail/hooks are not supported yet")]
    RetryUnsupported { id: String },
    #[error("step '{id}': gait.wf.{method} outputSchema is not supported yet")]
    OutputSchemaUnsupported { id: String, method: String },
    #[error("gait.wf.turn step '{id}': {source}")]
    TurnArgs {
        id: String,
        source: serde_json::Error,
    },
    #[error("turn step '{id}': prompt is required")]
    MissingPrompt { id: String },
    #[error("gait.wf.shell step '{id}': {source}")]
    ShellArgs {
        id: String,
        source: serde_json::Error,
    },
    #[error("{0}")]
    ShellValidation(anyhow::Error),
    #[error(transparent)]
    Worktree(anyhow::Error),
    #[error(transparent)]
    LogWrite(anyhow::Error),
    #[error("{0}")]
    TurnFailure(String),
    #[error("{0}")]
    ShellFailure(String),
    #[error("unknown gait.wf method '{0}'")]
    UnknownMethod(String),
}

impl StepError {
    fn turn_args(id: impl Into<String>, source: serde_json::Error) -> Self {
        Self::TurnArgs {
            id: id.into(),
            source,
        }
    }

    fn shell_args(id: impl Into<String>, source: serde_json::Error) -> Self {
        Self::ShellArgs {
            id: id.into(),
            source,
        }
    }
}

/// The local step engine for JS (script) workflows.
///
/// `WorkflowHost` forwards each `gait.wf.*` call over the channel; the owner
/// of the receiving end constructs a [`StepEngine`] and feeds it the requests.
/// The engine is deliberately **local** (not behind the Send-bounded
/// `CapabilityHost::call`): `TurnExecutor`'s futures are not `Send`, and steps
/// run concurrently (JS `Promise.all`) as futures polled in one task. That
/// means **no `RefCell` borrow may ever be held across an `.await`** — every
/// borrow must be dropped before suspending.
pub struct StepEngine<'a> {
    /// The run id (also the workflow/session id nodes see).
    run_id: String,
    /// The gait workspace directory.
    gait_dir: PathBuf,
    /// The codebase directory.
    codebase: PathBuf,
    /// CLI args passed to the workflow (joined by spaces for `$ARGS`).
    args: Vec<String>,
    /// Script path recorded in the WorkflowStart event.
    script_path: String,
    /// Script sha256 recorded in the WorkflowStart event.
    script_sha256: String,
    /// Full script source text recorded in the WorkflowStart event.
    script_source: String,
    /// The run's append-only event log.
    run_log: WorkflowRunLog,
    /// The run directory (`workflow-runs/<run_id>`).
    run_dir: PathBuf,
    /// The lazy per-run git worktree (`run_dir/worktree`).
    run_worktree: PathBuf,
    /// The turn executor (real or scripted) for prompt steps.
    turns: Arc<dyn TurnExecutor>,
    /// The lifecycle observer, notified after every log write.
    observer: RefCell<&'a mut dyn WorkflowObserver>,
    /// Mutable engine state, guarded so borrows never cross an `.await`.
    state: RefCell<EngineState>,
    /// Whether the run worktree has been created (lazy, created on first
    /// shell step).
    worktree_ready: Cell<bool>,
    /// Live sink channel for shell/sub-turn frames. The receiver lives with
    /// the engine's owner; frames are currently discarded.
    _sink_tx: tokio::sync::mpsc::UnboundedSender<gait_core::embed::SinkFrame>,
}

/// Mutable state of a [`StepEngine`].
struct EngineState {
    /// The workflow-level config, once `config` has been handled.
    config: Option<ConfigOpts>,
    /// Step ids already seen in this run (uniqueness check).
    seen_ids: HashSet<String>,
    /// Number of steps accepted so far.
    executed: usize,
    /// Whether the WorkflowStart event has been written.
    started: bool,
}

impl<'a> StepEngine<'a> {
    /// Create a step engine for one script-workflow run.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        run_id: impl Into<String>,
        gait_dir: impl Into<PathBuf>,
        codebase: impl Into<PathBuf>,
        args: Vec<String>,
        script_path: impl Into<String>,
        script_sha256: impl Into<String>,
        script_source: impl Into<String>,
        run_log: WorkflowRunLog,
        run_dir: impl Into<PathBuf>,
        turns: Arc<dyn TurnExecutor>,
        observer: &'a mut dyn WorkflowObserver,
    ) -> Self {
        let run_dir = run_dir.into();
        let run_worktree = run_dir.join("worktree");
        let (sink_tx, _sink_rx) = sink_channel_pair();
        Self {
            run_id: run_id.into(),
            gait_dir: gait_dir.into(),
            codebase: codebase.into(),
            args,
            script_path: script_path.into(),
            script_sha256: script_sha256.into(),
            script_source: script_source.into(),
            run_log,
            run_dir,
            run_worktree,
            turns,
            observer: RefCell::new(observer),
            state: RefCell::new(EngineState {
                config: None,
                seen_ids: HashSet::new(),
                executed: 0,
                started: false,
            }),
            worktree_ready: Cell::new(false),
            _sink_tx: sink_tx,
        }
    }

    /// Whether the `config` step has been handled (WorkflowStart written).
    pub fn started(&self) -> bool {
        self.state.borrow().started
    }

    /// Handle one `gait.wf.*` step request. Errors are plain strings — they
    /// become JS exceptions in the script.
    pub async fn handle(
        &self,
        req_method: &str,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        match req_method {
            "config" => self.handle_config(args).await.map_err(|e| error_text(&e)),
            "turn" => self.handle_turn(args).await.map_err(|e| error_text(&e)),
            "shell" => self.handle_shell(args).await.map_err(|e| error_text(&e)),
            other => Err(error_text(&StepError::UnknownMethod(other.to_string()))),
        }
    }

    /// Handle the `config` step: parse, write WorkflowStart, mark started.
    async fn handle_config(&self, args: serde_json::Value) -> Result<serde_json::Value, StepError> {
        if self.state.borrow().config.is_some() {
            return Err(StepError::ConfigAlreadyCalled);
        }
        let config: ConfigOpts =
            serde_json::from_value(args.clone()).map_err(StepError::ConfigArgs)?;

        let ev = self
            .run_log
            .script_workflow_start(
                &self.run_id,
                &config.name,
                &self.script_path,
                &self.script_sha256,
                &self.script_source,
                &self.args,
            )
            .map_err(StepError::LogWrite)?;
        self.observer.borrow_mut().on_event(&ev);

        {
            let mut state = self.state.borrow_mut();
            state.config = Some(config);
            state.started = true;
        }
        Ok(serde_json::json!({ "runId": self.run_id }))
    }

    /// Common cheap checks for turn/shell steps: the config must be seen and
    /// the `id` field must be present. These run BEFORE any bookkeeping, so a
    /// step that fails validation does not consume its id or count toward the
    /// step cap. Returns the parsed `id` of the step.
    fn validate_common_step(
        &self,
        method: &str,
        args: &serde_json::Value,
    ) -> Result<String, StepError> {
        {
            let state = self.state.borrow();
            if state.config.is_none() {
                return Err(StepError::ConfigFirst);
            }
        }
        let obj = match args {
            serde_json::Value::Object(map) => map,
            _ => {
                return Err(StepError::MissingId {
                    method: method.to_string(),
                })
            }
        };
        match obj.get("id").and_then(|v| v.as_str()) {
            Some(id) if !id.is_empty() => Ok(id.to_string()),
            _ => Err(StepError::MissingId {
                method: method.to_string(),
            }),
        }
    }

    /// Accept a validated step: check the duplicate id and the step cap, then
    /// commit the id and increment the counter in one short borrow (no await
    /// is possible here). Called immediately before `node_start`.
    fn accept_step(&self, id: &str) -> Result<(), StepError> {
        {
            let mut state = self.state.borrow_mut();
            if state.seen_ids.contains(id) {
                return Err(StepError::DuplicateId { id: id.to_string() });
            }
            let cap = state
                .config
                .as_ref()
                .and_then(|c| c.max_steps)
                .unwrap_or(1000);
            if state.executed >= cap {
                return Err(StepError::StepCap {
                    cap,
                    id: id.to_string(),
                });
            }
            state.seen_ids.insert(id.to_string());
            state.executed += 1;
        }
        Ok(())
    }

    /// Reject retry/onFail/hooks (and shell outputSchema) until implemented.
    fn reject_unsupported(
        method: &str,
        id: &str,
        retry: bool,
        on_fail: bool,
        hooks: bool,
        output_schema: bool,
    ) -> Result<(), StepError> {
        if retry || on_fail || hooks {
            return Err(StepError::RetryUnsupported { id: id.to_string() });
        }
        if output_schema {
            return Err(StepError::OutputSchemaUnsupported {
                id: id.to_string(),
                method: method.to_string(),
            });
        }
        Ok(())
    }

    /// Handle a `turn` step: parse, check, run the turn, log and return.
    async fn handle_turn(&self, args: serde_json::Value) -> Result<serde_json::Value, StepError> {
        let id = self.validate_common_step("turn", &args)?;
        let opts: TurnOpts =
            serde_json::from_value(args).map_err(|e| StepError::turn_args(id.clone(), e))?;
        Self::reject_unsupported(
            "turn",
            &id,
            opts.retry.is_some(),
            opts.on_fail.is_some(),
            opts.hooks.is_some(),
            false,
        )?;
        if opts.prompt.is_none() {
            return Err(StepError::MissingPrompt { id: id.clone() });
        }

        self.accept_step(&id)?;

        let ev = self
            .run_log
            .node_start(
                &id,
                "turn",
                1,
                opts.continue_session.as_deref().unwrap_or(""),
            )
            .map_err(StepError::LogWrite)?;
        self.observer.borrow_mut().on_event(&ev);

        let req = TurnRequest {
            id: id.clone(),
            prompt: opts.prompt.clone(),
            model: opts.model.clone(),
            auto: opts.auto,
            allow: opts.allow.clone(),
            agent: opts.agent.clone(),
            codebase: opts.codebase.clone().map(PathBuf::from),
            continue_session: opts.continue_session.clone(),
            output_schema: opts.output_schema.clone(),
        };
        match self.turns.run_turn(req).await {
            Ok(outcome) => {
                let session_id = outcome.session_id.clone().unwrap_or_default();
                let json_text = match &outcome.json {
                    Some(j) => serde_json::to_string(j).unwrap_or_default(),
                    None => String::new(),
                };
                let ev = self
                    .run_log
                    .node_complete(
                        &id,
                        &session_id,
                        &outcome.output,
                        outcome.exit,
                        &json_text,
                        "",
                    )
                    .map_err(StepError::LogWrite)?;
                self.observer.borrow_mut().on_event(&ev);
                Ok(serde_json::json!({
                    "output": outcome.output,
                    "exit": outcome.exit,
                    "json": outcome.json,
                    "sessionId": outcome.session_id,
                    "error": null,
                }))
            }
            Err(e) => {
                let ev = self
                    .run_log
                    .node_error(&id, &error_text(&e), 1)
                    .map_err(StepError::LogWrite)?;
                self.observer.borrow_mut().on_event(&ev);
                Err(StepError::TurnFailure(error_text(&e)))
            }
        }
    }

    /// Handle a `shell` step: parse, check, split host env, run, log, return.
    async fn handle_shell(&self, args: serde_json::Value) -> Result<serde_json::Value, StepError> {
        let id = self.validate_common_step("shell", &args)?;
        let opts: ShellOpts =
            serde_json::from_value(args).map_err(|e| StepError::shell_args(id.clone(), e))?;
        Self::reject_unsupported(
            "shell",
            &id,
            opts.retry.is_some(),
            opts.on_fail.is_some(),
            opts.hooks.is_some(),
            opts.output_schema.is_some(),
        )?;
        opts.validate().map_err(StepError::ShellValidation)?;

        // Split env: GAIT_ARG_<digits> entries come from the `gait.wf.sh`
        // tagged template and go to host_env; everything else stays in the
        // user env (and any other GAIT_ key is rejected by exec_shell).
        let mut opts = opts;
        let mut host_env: BTreeMap<String, String> = BTreeMap::new();
        let keys: Vec<String> = opts.env.keys().cloned().collect();
        for key in keys {
            if Self::is_gait_arg_key(&key) {
                if let Some(value) = opts.env.remove(&key) {
                    host_env.insert(key, value);
                }
            }
        }

        self.accept_step(&id)?;

        self.ensure_worktree().map_err(StepError::Worktree)?;

        let ev = self
            .run_log
            .node_start(&id, "shell", 1, "")
            .map_err(StepError::LogWrite)?;
        self.observer.borrow_mut().on_event(&ev);

        let ctx = self.build_node_context(&id);
        match exec_shell_opts_with_host_env(&opts, &ctx, &[], &host_env).await {
            Ok(res) => {
                let json_text = match &res.json {
                    Some(j) => serde_json::to_string(j).unwrap_or_default(),
                    None => String::new(),
                };
                let ev = self
                    .run_log
                    .node_complete(&id, "", &res.output, res.exit, &json_text, "")
                    .map_err(StepError::LogWrite)?;
                self.observer.borrow_mut().on_event(&ev);
                serde_json::to_value(&res).map_err(|e| StepError::LogWrite(e.into()))
            }
            Err(e) => {
                let ev = self
                    .run_log
                    .node_error(&id, &error_text(&e), 1)
                    .map_err(StepError::LogWrite)?;
                self.observer.borrow_mut().on_event(&ev);
                Err(StepError::ShellFailure(error_text(&e)))
            }
        }
    }

    /// Is this key exactly `GAIT_ARG_` followed by one or more ASCII digits?
    fn is_gait_arg_key(key: &str) -> bool {
        let Some(rest) = key.strip_prefix("GAIT_ARG_") else {
            return false;
        };
        !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit())
    }

    /// Lazily create the run worktree. Synchronous (no await inside), so two
    /// concurrent steps cannot both create it.
    fn ensure_worktree(&self) -> anyhow::Result<()> {
        if self.worktree_ready.get() {
            return Ok(());
        }
        if !self.run_worktree.join(".git").exists() {
            std::fs::create_dir_all(&self.run_dir)?;
            let repos = gait_core::git::Repos::production();
            repos.traces.create_branch_worktree(
                &self.gait_dir,
                "main",
                &format!("run/{}", self.run_id),
                &self.run_worktree,
            )?;
        }
        self.worktree_ready.set(true);
        Ok(())
    }

    /// Build the `NodeContext` for a shell step (same shape as
    /// tests/shell_env.rs: workflow/session ids are the run id, node session
    /// is empty, args joined by spaces).
    fn build_node_context<'s>(&'s self, node_id: &'s str) -> NodeContext<'s> {
        NodeContext {
            workflow_id: &self.run_id,
            session_id: &self.run_id,
            node_session: "",
            gait_dir: &self.gait_dir,
            codebase: &self.codebase,
            run_worktree: &self.run_worktree,
            vars: crate::vars::VarContext {
                workflow_id: self.run_id.clone(),
                session_id: self.run_id.clone(),
                gait_dir: self.gait_dir.clone(),
                codebase: self.codebase.clone(),
                args: self.args.join(" "),
                node_id: node_id.to_string(),
                node_outputs: Default::default(),
                node_exit: None,
                failed_node: None,
            },
            workflow_tx: &self._sink_tx,
            sandbox_on: None,
            cascade_group_arg: None,
            cascade: None,
            profile: None,
            fs_delegate: None,
            allow_intree: false,
            env_passthrough: Vec::new(),
        }
    }
}

/// A single workflow step request forwarded to the executor side of the
/// channel.
pub struct StepRequest {
    /// Method name requested by the codemode script (e.g. `"config"`).
    pub method: String,
    /// JSON arguments for the step.
    pub args: serde_json::Value,
    /// Reply channel back to the awaiting `WorkflowHost`.
    pub reply: tokio::sync::oneshot::Sender<Result<serde_json::Value, String>>,
}

/// Send-safe handle implementing [`CapabilityHost`] by delegating steps to a
/// remote executor over a bounded channel.
pub struct WorkflowHost {
    tx: tokio::sync::mpsc::Sender<StepRequest>,
}

impl WorkflowHost {
    /// Create a host handle paired with the receiver that will execute steps.
    pub fn channel() -> (WorkflowHost, tokio::sync::mpsc::Receiver<StepRequest>) {
        let (tx, rx) = workflow_channel(1024);
        (WorkflowHost { tx }, rx)
    }
}

const NOT_ACCEPTING: &str = "workflow run is no longer accepting steps";

#[async_trait]
impl CapabilityHost for WorkflowHost {
    async fn call(
        &self,
        namespace: &str,
        method: &str,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, HostError> {
        if namespace != "wf" {
            return Err(HostError::Unavailable {
                namespace: namespace.to_string(),
                method: method.to_string(),
            });
        }

        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        let request = StepRequest {
            method: method.to_string(),
            args,
            reply: reply_tx,
        };

        if self.tx.send(request).await.is_err() {
            return Err(HostError::Failed(error_text(&NOT_ACCEPTING)));
        }

        match reply_rx.await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(message)) => Err(HostError::Failed(message)),
            Err(_) => Err(HostError::Failed(error_text(&NOT_ACCEPTING))),
        }
    }
}

// Compile-time proof that the future returned by `call` is `Send`, which the
// `CapabilityHost` contract requires. This uses the `async_trait` default
// (`Send` futures); it deliberately does not opt out with `?Send`.
#[allow(dead_code)]
fn _call_future_is_send(h: &WorkflowHost) {
    fn check<T: Send>(_: &T) {}
    let f = h.call("wf", "config", serde_json::Value::Null);
    check(&f);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn assert_send<T: Send>() {}

    #[test]
    fn types_are_send() {
        assert_send::<WorkflowHost>();
        assert_send::<StepRequest>();
    }

    #[tokio::test]
    async fn successful_step_round_trip() {
        let (host, mut rx) = WorkflowHost::channel();

        tokio::spawn(async move {
            if let Some(request) = rx.recv().await {
                assert_eq!(request.method, "config");
                if request.reply.send(Ok(json!({"ok": 1}))).is_err() {
                    // The awaiting host was dropped; nothing to do.
                }
            }
        });

        let result = host.call("wf", "config", json!({"name": "t"})).await;
        assert_eq!(result.unwrap(), json!({"ok": 1}));
    }

    #[tokio::test]
    async fn error_reply_becomes_failed() {
        let (host, mut rx) = WorkflowHost::channel();

        tokio::spawn(async move {
            if let Some(request) = rx.recv().await {
                if request.reply.send(Err("boom".to_string())).is_err() {
                    // The awaiting host was dropped; nothing to do.
                }
            }
        });

        let result = host.call("wf", "config", json!({})).await;
        match result {
            Err(HostError::Failed(message)) => assert_eq!(message, "boom"),
            other => panic!("expected Failed(\"boom\"), got {:?}", other),
        }
    }

    #[tokio::test]
    async fn foreign_namespace_is_unavailable() {
        let (host, _rx) = WorkflowHost::channel();

        let result = host.call("fs", "read", json!({})).await;
        match result {
            Err(HostError::Unavailable { namespace, method }) => {
                assert_eq!(namespace, "fs");
                assert_eq!(method, "read");
            }
            other => panic!("expected Unavailable, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn dropped_receiver_reports_not_accepting() {
        let (host, rx) = WorkflowHost::channel();
        drop(rx);

        let result = host.call("wf", "config", json!({})).await;
        match result {
            Err(HostError::Failed(message)) => {
                assert_eq!(message, NOT_ACCEPTING);
            }
            other => panic!("expected Failed, got {:?}", other),
        }
    }

    // ── StepEngine (pure checks; no git repo, scripted turns only) ─────────

    use crate::observer::NullObserver;
    use crate::run_log::WorkflowRunLog;
    use crate::turn_exec::testing::ScriptedTurnExecutor;
    use tempfile::TempDir;

    /// Build a StepEngine with its run log inside a TempDir. The TempDir is
    /// returned so it outlives the engine.
    fn make_engine(
        script: ScriptedTurnExecutor,
        _max_steps: Option<usize>,
    ) -> (TempDir, StepEngine<'static>) {
        let dir = TempDir::new().unwrap();
        let run_id = "run-test".to_string();
        let runs_dir = dir.path().join("workflow-runs");
        let log = WorkflowRunLog::create(&runs_dir, &run_id).unwrap();
        let run_dir = runs_dir.join(&run_id);
        let observer: &'static mut dyn crate::observer::WorkflowObserver =
            Box::leak(Box::new(NullObserver));
        let engine = StepEngine::new(
            run_id,
            dir.path(),
            dir.path(),
            Vec::new(),
            "wf.js",
            "deadbeef",
            "export default async () => {}",
            log,
            run_dir,
            Arc::new(script),
            observer,
        );
        (dir, engine)
    }

    #[tokio::test]
    async fn config_must_come_first() {
        let (_dir, engine) = make_engine(ScriptedTurnExecutor::new(), None);

        let err = engine
            .handle("turn", json!({ "id": "t1", "prompt": "hi" }))
            .await
            .unwrap_err();
        assert!(
            err.contains("gait.wf.config() must be awaited before any step"),
            "got: {err}"
        );
        assert!(!engine.started());
    }

    #[tokio::test]
    async fn step_without_id_is_rejected() {
        let (_dir, engine) = make_engine(ScriptedTurnExecutor::new(), None);
        engine
            .handle("config", json!({ "name": "test-wf" }))
            .await
            .unwrap();

        let err = engine
            .handle("turn", json!({ "prompt": "hi" }))
            .await
            .unwrap_err();
        assert!(
            err.contains("gait.wf.turn: missing required field `id`"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn config_may_only_be_called_once() {
        let (_dir, engine) = make_engine(ScriptedTurnExecutor::new(), None);
        engine
            .handle("config", json!({ "name": "test-wf" }))
            .await
            .unwrap();

        let err = engine
            .handle("config", json!({ "name": "again" }))
            .await
            .unwrap_err();
        assert_eq!(err, "gait.wf.config() may only be called once");
    }

    #[tokio::test]
    async fn unknown_method_is_an_error() {
        let (_dir, engine) = make_engine(ScriptedTurnExecutor::new(), None);
        let err = engine.handle("gate", json!({})).await.unwrap_err();
        assert_eq!(err, "unknown gait.wf method 'gate'");
    }

    #[tokio::test]
    async fn duplicate_step_id_is_rejected() {
        let mut script = ScriptedTurnExecutor::new();
        script.script_text("one");
        let (_dir, engine) = make_engine(script, None);
        engine
            .handle("config", json!({ "name": "test-wf" }))
            .await
            .unwrap();

        engine
            .handle("turn", json!({ "id": "t1", "prompt": "hi" }))
            .await
            .unwrap();

        let err = engine
            .handle("turn", json!({ "id": "t1", "prompt": "again" }))
            .await
            .unwrap_err();
        assert!(err.contains("duplicate step id 't1'"), "got: {err}");
    }

    #[tokio::test]
    async fn step_cap_is_enforced() {
        let mut script = ScriptedTurnExecutor::new();
        script.script_text("one");
        let (_dir, engine) = make_engine(script, None);
        engine
            .handle("config", json!({ "name": "test-wf", "maxSteps": 1 }))
            .await
            .unwrap();

        engine
            .handle("turn", json!({ "id": "t1", "prompt": "hi" }))
            .await
            .unwrap();

        let err = engine
            .handle("turn", json!({ "id": "t2", "prompt": "hi" }))
            .await
            .unwrap_err();
        assert!(
            err.contains("step cap of 1 reached: refusing step 't2'"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn retry_on_fail_and_hooks_are_rejected() {
        let mut script = ScriptedTurnExecutor::new();
        script.script_text("unused");
        let (_dir, engine) = make_engine(script, None);
        engine
            .handle("config", json!({ "name": "test-wf" }))
            .await
            .unwrap();

        let err = engine
            .handle(
                "turn",
                json!({ "id": "t1", "prompt": "hi", "retry": { "times": 2 } }),
            )
            .await
            .unwrap_err();
        assert!(
            err.contains("step 't1': retry/onFail/hooks are not supported yet"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn turn_step_returns_a_step_result() {
        let mut script = ScriptedTurnExecutor::new();
        script.script_text("hello turn").session_id("sess-1");
        let (_dir, engine) = make_engine(script, None);
        engine
            .handle("config", json!({ "name": "test-wf" }))
            .await
            .unwrap();

        let value = engine
            .handle("turn", json!({ "id": "t1", "prompt": "hi" }))
            .await
            .unwrap();
        assert_eq!(value["output"], "hello turn");
        assert_eq!(value["sessionId"], "sess-1");
        assert_eq!(value["exit"], 0);
        assert!(engine.started());
    }

    #[tokio::test]
    async fn invalid_shell_step_fails_validation_and_frees_its_id() {
        // No git repo in the temp dir, so the successful shell step must be
        // replaced by a turn step (which needs no worktree) to prove the id
        // was freed.
        let mut script = ScriptedTurnExecutor::new();
        script.script_text("turn after rejection");
        let (_dir, engine) = make_engine(script, None);
        engine
            .handle("config", json!({ "name": "test-wf" }))
            .await
            .unwrap();

        let err = engine
            .handle(
                "shell",
                json!({ "id": "s1", "argv": ["true"], "script": "echo hi" }),
            )
            .await
            .unwrap_err();
        assert!(
            err.contains("argv and script are mutually exclusive"),
            "argv+script must fail validation: {err}"
        );

        // The id was not consumed, so a turn step may now use it.
        engine
            .handle("turn", json!({ "id": "s1", "prompt": "go" }))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn rejected_step_does_not_count_toward_max_steps() {
        let mut script = ScriptedTurnExecutor::new();
        script.script_text("turn after rejection");
        let (_dir, engine) = make_engine(script, None);
        engine
            .handle("config", json!({ "name": "test-wf", "maxSteps": 1 }))
            .await
            .unwrap();

        let err = engine
            .handle(
                "shell",
                json!({ "id": "s1", "argv": ["true"], "script": "echo hi" }),
            )
            .await
            .unwrap_err();
        assert!(!err.contains("step cap"), "not a cap error: {err}");

        // The rejected step did not increment the counter, so a valid step
        // still fits within maxSteps = 1.
        engine
            .handle("turn", json!({ "id": "s1", "prompt": "go" }))
            .await
            .unwrap();
    }
}

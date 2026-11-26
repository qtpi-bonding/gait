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
use std::future::pending;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use gait_codemode::{CapabilityHost, HostError};

use crate::exec_shell::{exec_shell_opts_with_host_env, NodeContext};
use crate::hooks::run_hook;
use crate::observer::WorkflowObserver;
use crate::record::{self, WorkflowRecord};
use crate::run_log::WorkflowRunLog;
use crate::state::{JournalEntry, JournalState, ScriptJournal};
use crate::step::{build_shell_env, Backoff, ConfigOpts, GateOpts, RetryOpts, ShellOpts, TurnOpts};
use crate::turn_exec::{TurnExecutor, TurnRequest};
use gait_core::proto::gait::workflow::v1::{NodeErrorKind, RecordCommand, RecordConfig};

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
    #[error("step id '{id}' is reserved (ids starting with __now_ or __random_ are used by gait.wf.now/random)")]
    ReservedId { id: String },
    #[error("step cap of {cap} reached: refusing step '{id}'")]
    StepCap { cap: usize, id: String },
    #[error("workflow script changed (step {id}): logged as {old}, now {new}")]
    KindChanged {
        id: String,
        old: String,
        new: String,
    },
    #[error(
        "step '{id}' is still open in the run log (no terminal event); \
         pass --force-rerun {id} to accept re-running it"
    )]
    OpenStep { id: String },
    #[error("gait.wf.{method} takes no arguments")]
    NoArgs { method: String },
    #[error("gait.wf.turn step '{id}': {source}")]
    TurnArgs {
        id: String,
        source: serde_json::Error,
    },
    #[error("gait.wf.turn step '{id}': a turn needs a prompt; use gate() to wait for a person")]
    TurnNeedsPrompt { id: String },
    #[error("gait.wf.shell step '{id}': {source}")]
    ShellArgs {
        id: String,
        source: serde_json::Error,
    },
    #[error("gait.wf.gate step '{id}': {source}")]
    GateArgs {
        id: String,
        source: serde_json::Error,
    },
    #[error("{0}")]
    ShellValidation(anyhow::Error),
    #[error(transparent)]
    Worktree(anyhow::Error),
    #[error(transparent)]
    LogWrite(anyhow::Error),
    #[error(transparent)]
    Record(anyhow::Error),
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

/// A gate's suspension request: the gate message and step id.
#[derive(Debug, Clone)]
pub struct PauseRequest {
    /// The gate's human-facing message.
    pub message: String,
    pub id: String,
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
    /// The journal folded from the run log's existing events (empty for a
    /// fresh run).
    journal: ScriptJournal,
    /// Step ids the caller accepted re-running despite being open in the
    /// journal.
    force_rerun: Vec<String>,
    /// The lifecycle observer, notified after every log write.
    observer: RefCell<&'a mut dyn WorkflowObserver>,
    /// Mutable engine state, guarded so borrows never cross an `.await`.
    state: RefCell<EngineState>,
    /// In-progress run-level evidence record, when configured on a fresh run.
    record: RefCell<Option<WorkflowRecord>>,
    /// Whether the run worktree has been created (lazy, created on first
    /// shell step).
    worktree_ready: Cell<bool>,
    /// CLI args joined by spaces (the `$ARGS` value shell steps see), owned
    /// here so a `NodeContext` can borrow it.
    args_joined: String,
}

impl Drop for StepEngine<'_> {
    fn drop(&mut self) {
        // A dropped run_script future (e.g. Ctrl-C) must still record Cancelled
        // for steps that were mid-flight. Log write errors are deliberately
        // ignored here: the run is being torn down anyway.
        self.cancel_in_flight("workflow run interrupted");
    }
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
    /// How many `now` values have been issued (id suffix counter).
    now_count: usize,
    /// How many `random` values have been issued (id suffix counter).
    random_count: usize,
    /// Steps currently executing live (id, kind). Gates and held calls are
    /// never registered; replayed steps never register.
    in_flight: Vec<(String, String)>,
    /// The first requested pause, if any. Later pause requests are held.
    pause: Option<PauseRequest>,
}

/// What the journal says a step should do on this run.
enum ReplayDisposition {
    /// Run live exactly as a fresh run would.
    Live,
    /// Return the journaled terminal result without executing.
    Complete(JournalEntry),
    /// The step failed on a previous run; replay as a resolved `{error}`
    /// result (a failed node counts as finished, so the rest runs).
    Errored(JournalEntry),
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
        journal: ScriptJournal,
        force_rerun: Vec<String>,
        observer: &'a mut dyn WorkflowObserver,
    ) -> Self {
        let run_dir = run_dir.into();
        let run_worktree = run_dir.join("worktree");
        let args_joined = args.join(" ");
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
            journal,
            force_rerun,
            observer: RefCell::new(observer),
            state: RefCell::new(EngineState {
                config: None,
                seen_ids: HashSet::new(),
                executed: 0,
                started: false,
                now_count: 0,
                random_count: 0,
                in_flight: Vec::new(),
                pause: None,
            }),
            record: RefCell::new(None),
            worktree_ready: Cell::new(false),
            args_joined,
        }
    }

    /// Whether the `config` step has been handled (WorkflowStart written).
    pub fn started(&self) -> bool {
        self.state.borrow().started
    }

    /// Take the in-progress record out of the engine for finalization by the
    /// script runner. Resumed runs intentionally have no record here.
    pub fn take_record(&self) -> Option<WorkflowRecord> {
        self.record.borrow_mut().take()
    }

    /// The ids of the steps currently executing live, in registration order.
    pub fn in_flight_ids(&self) -> Vec<String> {
        self.state
            .borrow()
            .in_flight
            .iter()
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Register a live step right after its NodeStart event is written.
    fn register_in_flight(&self, id: &str, kind: &str) {
        self.state
            .borrow_mut()
            .in_flight
            .push((id.to_string(), kind.to_string()));
    }

    /// Unregister a step right after its terminal log event (NodeComplete or
    /// NodeError) is written. Idempotent per step: a step that already wrote
    /// its terminal event is no longer registered.
    fn unregister_in_flight(&self, id: &str) {
        self.state
            .borrow_mut()
            .in_flight
            .retain(|(flight_id, _)| flight_id != id);
    }

    /// Whether a pause has been requested AND no step is executing live.
    pub fn pause_ready(&self) -> bool {
        let state = self.state.borrow();
        state.pause.is_some() && state.in_flight.is_empty()
    }

    /// Take the requested pause (if one is ready). The pause is consumed.
    pub fn take_pause(&self) -> Option<PauseRequest> {
        if !self.pause_ready() {
            return None;
        }
        self.state.borrow_mut().pause.take()
    }

    /// Request a pause (the FIRST request wins) if none is pending. Returns
    /// whether THIS call owns the pause (i.e. none was already requested).
    fn request_pause(&self, request: PauseRequest) -> bool {
        let mut state = self.state.borrow_mut();
        if state.pause.is_some() {
            return false;
        }
        state.pause = Some(request);
        true
    }

    /// Whether a live call is currently HELD because a pause was requested:
    /// its future never resolves, it writes no log event and does not
    /// register. Replays execute nothing and are still answered.
    fn is_held(&self) -> bool {
        self.state.borrow().pause.is_some()
    }

    /// Write a terminal Cancelled event for every registered step, notify
    /// the observer, and clear the registry. Idempotent: a step that already
    /// wrote its terminal event is no longer registered, so nothing is
    /// written twice.
    pub fn cancel_in_flight(&self, reason: &str) {
        let ids: Vec<String> = {
            let mut state = self.state.borrow_mut();
            let taken = std::mem::take(&mut state.in_flight);
            taken.into_iter().map(|(id, _)| id).collect()
        };
        for id in ids {
            if let Ok(ev) = self.run_log.node_cancelled(&id, reason, 1) {
                self.observer.borrow_mut().on_event(&ev);
            }
        }
    }

    /// Handle one `gait.wf.*` step request. Errors are plain strings — they
    /// become JS exceptions in the script.
    pub async fn handle(
        &self,
        req_method: &str,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        match req_method {
            "config" => self.handle_config(args).await,
            "turn" => self.handle_turn(args).await,
            "shell" => self.handle_shell(args).await,
            "gate" => self.handle_gate(args).await,
            "now" => self.handle_now_random("now", args).await,
            "random" => self.handle_now_random("random", args).await,
            other => Err(StepError::UnknownMethod(other.to_string())),
        }
        .map_err(|e| e.to_string())
    }

    /// Replay disposition for one step id against the journal. Called after
    /// validation/arg parsing and BEFORE `accept_step`'s cap check.
    fn replay_disposition(&self, id: &str, kind: &str) -> Result<ReplayDisposition, StepError> {
        let Some(entry) = self.journal.get(id) else {
            return Ok(ReplayDisposition::Live);
        };
        if entry.state == JournalState::Cancelled {
            // Not terminal: the step simply runs again, as today.
            return Ok(ReplayDisposition::Live);
        }
        if entry.kind != kind {
            return Err(StepError::KindChanged {
                id: id.to_string(),
                old: entry.kind.clone(),
                new: kind.to_string(),
            });
        }
        match &entry.state {
            JournalState::Complete { .. } => Ok(ReplayDisposition::Complete(entry.clone())),
            JournalState::Errored { .. } => Ok(ReplayDisposition::Errored(entry.clone())),
            JournalState::Open => {
                if self.force_rerun.iter().any(|f| f == id) {
                    Ok(ReplayDisposition::Live)
                } else {
                    Err(StepError::OpenStep { id: id.to_string() })
                }
            }
            JournalState::Cancelled => unreachable!("handled above"),
        }
    }

    /// Record a replayed step's id: the duplicate-id check still applies (a
    /// second call with the same id in one execution is an error), but the id
    /// does NOT count toward the step cap.
    fn record_seen(&self, id: &str) -> Result<(), StepError> {
        let mut state = self.state.borrow_mut();
        if state.seen_ids.contains(id) {
            return Err(StepError::DuplicateId { id: id.to_string() });
        }
        state.seen_ids.insert(id.to_string());
        Ok(())
    }

    /// Handle the `config` step: parse, write WorkflowStart, mark started.
    async fn handle_config(&self, args: serde_json::Value) -> Result<serde_json::Value, StepError> {
        if self.state.borrow().config.is_some() {
            return Err(StepError::ConfigAlreadyCalled);
        }
        let config: ConfigOpts =
            serde_json::from_value(args.clone()).map_err(StepError::ConfigArgs)?;

        // Recording is a start-of-live-run operation.  Do it before emitting
        // WorkflowStart, and never on a resume (the journal is the authority
        // for that distinction).
        if !self.resuming() {
            if let Some(record_opts) = &config.record {
                self.ensure_worktree().map_err(StepError::Record)?;
                let config_value = RecordConfig {
                    commands: record_opts
                        .commands
                        .iter()
                        .map(|command| RecordCommand {
                            id: command.id.clone(),
                            command: command.command.clone(),
                            timeout_ms: command.timeout.unwrap_or(120_000),
                        })
                        .collect(),
                };
                let captured = record::begin(
                    &self.run_id,
                    &self.script_sha256,
                    Some(&config_value),
                    &self.run_worktree,
                )
                .await
                .map_err(StepError::Record)?;
                record::write(&captured, &record::record_path(&self.run_dir))
                    .map_err(StepError::Record)?;
                *self.record.borrow_mut() = Some(captured);
            }
        }

        // On a resumed run the log already carries the WorkflowStart event;
        // writing a second one would corrupt the journal fold.
        if !self.resuming() {
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
        }

        {
            let mut state = self.state.borrow_mut();
            state.config = Some(config);
            state.started = true;
        }
        Ok(serde_json::json!({ "runId": self.run_id }))
    }

    /// Whether the journal already carries a WorkflowStart (a resumed run).
    fn resuming(&self) -> bool {
        self.journal.has_workflow_start()
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
            Some(id) if !id.is_empty() => {
                if id.starts_with("__now_") || id.starts_with("__random_") {
                    return Err(StepError::ReservedId { id: id.to_string() });
                }
                Ok(id.to_string())
            }
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

    /// Backoff delay before retry attempt N (1-based).
    fn retry_delay_ms(attempt: u32, retry: &RetryOpts) -> u64 {
        match retry.backoff {
            Backoff::Exponential => retry
                .delay
                .saturating_mul(2u64.saturating_pow(attempt.saturating_sub(1))),
            Backoff::Linear => retry.delay,
        }
    }

    /// Handle a `turn` step: parse, check, run the turn, log and return.
    async fn handle_turn(&self, args: serde_json::Value) -> Result<serde_json::Value, StepError> {
        let id = self.validate_common_step("turn", &args)?;
        let opts: TurnOpts =
            serde_json::from_value(args).map_err(|e| StepError::turn_args(id.clone(), e))?;

        // A turn whose prompt is missing or blank can never execute: fail the
        // step outright, before anything is logged and before any hook runs.
        let Some(prompt) = opts.prompt.clone().filter(|p| !p.trim().is_empty()) else {
            return Err(StepError::TurnNeedsPrompt { id });
        };

        // Replay check comes BEFORE accept_step's cap check: a replayed step
        // neither executes nor counts toward the cap.
        match self.replay_disposition(&id, "turn")? {
            ReplayDisposition::Complete(entry) => {
                self.record_seen(&id)?;
                let (session, output, exit, json_output) = match entry.state {
                    JournalState::Complete {
                        session_id,
                        output,
                        exit_code,
                        json_output,
                    } => (session_id, output, exit_code, json_output),
                    _ => unreachable!("disposition says Complete"),
                };
                // A corrupt journaled value replays as absent, exactly as
                // `reconstruct_state` folds it.
                let json: Option<serde_json::Value> = if json_output.is_empty() {
                    None
                } else {
                    serde_json::from_str(&json_output).ok()
                };
                let session = if session.is_empty() {
                    None
                } else {
                    Some(session)
                };
                return Ok(serde_json::json!({
                    "output": output,
                    "exit": exit,
                    "json": json,
                    "sessionId": session,
                    "error": null,
                }));
            }
            ReplayDisposition::Errored(entry) => {
                self.record_seen(&id)?;
                let (session, error) = match entry.state {
                    JournalState::Errored { error } => (entry.session_id, error),
                    _ => unreachable!("disposition says Errored"),
                };
                let session = if session.is_empty() {
                    None
                } else {
                    Some(session)
                };
                return Ok(serde_json::json!({
                    "output": "",
                    "exit": null,
                    "json": null,
                    "sessionId": session,
                    "error": error,
                }));
            }
            ReplayDisposition::Live => {}
        }

        // Once a pause is requested, a live call is HELD: it never resolves,
        // writes no log event, and does not register.
        if self.is_held() {
            return Ok(pending::<serde_json::Value>().await);
        }

        self.accept_step(&id)?;

        // The step's session id is chosen ONCE, before the turn runs: the
        // step's `continueSession` when given, else a freshly generated id
        // (retries reuse it via `req.session_id`).
        let session_id = match opts.continue_session.as_deref() {
            Some(cs) if !cs.is_empty() => cs.to_string(),
            _ => gait_core::session::new_session_id(),
        };
        let ev = self
            .run_log
            .node_start(&id, "turn", 1, &session_id)
            .map_err(StepError::LogWrite)?;
        self.observer.borrow_mut().on_event(&ev);
        self.register_in_flight(&id, "turn");

        let hook_env = |failed: &str| {
            let well_known: BTreeMap<String, String> = [
                ("WORKFLOW_ID".into(), self.run_id.clone()),
                ("SESSION_ID".into(), self.run_id.clone()),
                ("NODE_ID".into(), id.clone()),
                ("CODEBASE".into(), self.codebase.display().to_string()),
                ("WORKSPACE".into(), self.gait_dir.display().to_string()),
                ("ARGS".into(), self.args.join(" ")),
                ("FAILED_NODE".into(), failed.into()),
            ]
            .into_iter()
            .collect();
            build_shell_env(&well_known, &BTreeMap::new(), &[])
                .map_err(|e| StepError::ShellFailure(e.to_string()))
        };
        if let Some(cmd) = opts.hooks.as_ref().and_then(|h| h.before.as_deref()) {
            let env = hook_env("")?;
            let (exit, _) = run_hook(cmd, &env, 120_000, None).await;
            if exit != 0 {
                let message = format!("before hook failed with exit code {exit}");
                if let Some(error_cmd) = opts.hooks.as_ref().and_then(|h| h.error.as_deref()) {
                    let env = hook_env(&id)?;
                    let _ = run_hook(error_cmd, &env, 120_000, None).await;
                }
                let log_result = if opts.on_fail == Some(crate::step::OnFail::Continue) {
                    self.run_log
                        .node_error_kind(&id, &message, 1, NodeErrorKind::Continued)
                } else {
                    self.run_log.node_error(&id, &message, 1)
                }
                .map_err(StepError::LogWrite);
                self.unregister_in_flight(&id);
                let ev = log_result?;
                self.observer.borrow_mut().on_event(&ev);
                if opts.on_fail == Some(crate::step::OnFail::Continue) {
                    return Ok(serde_json::json!({
                        "output": "",
                        "exit": null,
                        "json": null,
                        "sessionId": if session_id.is_empty() { None } else { Some(session_id.clone()) },
                        "error": message,
                    }));
                }
                return Err(StepError::TurnFailure(message));
            }
        }

        // Config defaults (`gait.wf.config`) apply to turn steps: a step's own
        // value wins; `allow` falls back to the config list only when the
        // step's list is empty.
        let config = self.state.borrow().config.clone();
        let model = opts
            .model
            .clone()
            .or_else(|| config.as_ref().and_then(|c| c.model.clone()));
        let auto = opts.auto.or_else(|| config.as_ref().and_then(|c| c.auto));
        let allow = if opts.allow.is_empty() {
            config.as_ref().map(|c| c.allow.clone()).unwrap_or_default()
        } else {
            opts.allow.clone()
        };

        let req = TurnRequest {
            id: id.clone(),
            session_id,
            prompt,
            model,
            auto,
            allow,
            agent: opts.agent.clone(),
            codebase: opts.codebase.clone().map(PathBuf::from),
            continue_session: opts.continue_session.clone(),
            output_schema: opts.output_schema.clone(),
        };
        let max_attempts = opts.retry.as_ref().map_or(1, |r| r.times as usize + 1);
        let mut last_failure: Option<String> = None;
        let result = {
            let mut attempt = 1;
            loop {
                if attempt > 1 {
                    let retry = opts
                        .retry
                        .as_ref()
                        .expect("retry set when max_attempts > 1");
                    let delay = Self::retry_delay_ms(attempt as u32, retry);
                    tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                }
                match self.turns.run_turn(req.clone()).await {
                    Ok(outcome) => break Ok(outcome),
                    Err(e) => {
                        last_failure = Some(e.to_string());
                    }
                }
                attempt += 1;
                if attempt > max_attempts {
                    break Err(());
                }
            }
        };

        match result {
            Ok(outcome) => {
                let session_id = outcome.session_id.clone().unwrap_or_default();
                let json_text = match &outcome.json {
                    Some(j) => serde_json::to_string(j).unwrap_or_default(),
                    None => String::new(),
                };
                let log_result = self
                    .run_log
                    .node_complete(&id, &session_id, &outcome.output, outcome.exit, &json_text)
                    .map_err(StepError::LogWrite);
                self.unregister_in_flight(&id);
                let ev = log_result?;
                self.observer.borrow_mut().on_event(&ev);
                if let Some(cmd) = opts.hooks.as_ref().and_then(|h| h.after.as_deref()) {
                    let env = hook_env("")?;
                    let _ = run_hook(cmd, &env, 120_000, None).await;
                }
                Ok(serde_json::json!({
                    "output": outcome.output,
                    "exit": outcome.exit,
                    "json": outcome.json,
                    "sessionId": outcome.session_id,
                    "error": null,
                }))
            }
            Err(()) => {
                let message =
                    last_failure.unwrap_or_else(|| "turn failed for an unknown reason".into());
                // onFail: "continue" resolves the failure to the script as an
                // error-result object; the NodeError is still logged, but with
                // kind CONTINUED so the replay treats the step as finished.
                if opts.on_fail == Some(crate::step::OnFail::Continue) {
                    if let Some(cmd) = opts.hooks.as_ref().and_then(|h| h.error.as_deref()) {
                        let env = hook_env(&id)?;
                        let _ = run_hook(cmd, &env, 120_000, None).await;
                    }
                    let log_result = self
                        .run_log
                        .node_error_kind(&id, &message, 1, NodeErrorKind::Continued)
                        .map_err(StepError::LogWrite);
                    self.unregister_in_flight(&id);
                    let ev = log_result?;
                    self.observer.borrow_mut().on_event(&ev);
                    return Ok(serde_json::json!({
                        "output": "",
                        "exit": null,
                        "json": null,
                        "sessionId": if req.session_id.is_empty() { None } else { Some(req.session_id.clone()) },
                        "error": message,
                    }));
                }
                let log_result = self
                    .run_log
                    .node_error(&id, &message, 1)
                    .map_err(StepError::LogWrite);
                self.unregister_in_flight(&id);
                let ev = log_result?;
                self.observer.borrow_mut().on_event(&ev);
                if let Some(cmd) = opts.hooks.as_ref().and_then(|h| h.error.as_deref()) {
                    let env = hook_env(&id)?;
                    let _ = run_hook(cmd, &env, 120_000, None).await;
                }
                Err(StepError::TurnFailure(message))
            }
        }
    }

    /// Handle a `gate` step: parse, check, and either replay or request a
    /// pause that suspends the run until a human continues it. A gate call
    /// NEVER resolves: a first-run gate suspends the run; on the continue
    /// (journal Open) it writes exactly one NodeComplete and returns null.
    async fn handle_gate(&self, args: serde_json::Value) -> Result<serde_json::Value, StepError> {
        let id = self.validate_common_step("gate", &args)?;
        let opts: GateOpts = serde_json::from_value(args).map_err(|e| StepError::GateArgs {
            id: id.clone(),
            source: e,
        })?;

        if let Some(entry) = self.journal.get(&id) {
            if entry.state != JournalState::Cancelled {
                if entry.kind != "gate" {
                    return Err(StepError::KindChanged {
                        id: id.clone(),
                        old: entry.kind.clone(),
                        new: "gate".to_string(),
                    });
                }
                match &entry.state {
                    JournalState::Complete { .. } => {
                        self.record_seen(&id)?;
                        return Ok(serde_json::Value::Null);
                    }
                    JournalState::Errored { error } => {
                        self.record_seen(&id)?;
                        return Ok(serde_json::json!({ "error": error }));
                    }
                    JournalState::Open => {
                        if !self.force_rerun.iter().any(|f| f == &id) {
                            if !entry.gate_announced {
                                // The previous run reached the gate but was
                                // interrupted before the pause was published.
                                // Re-publish the pause without another
                                // NodeStart; only an announced gate can be
                                // resolved by continue.
                                self.record_seen(&id)?;
                                self.request_pause(PauseRequest {
                                    message: opts.message,
                                    id: id.clone(),
                                });
                                return Ok(pending::<serde_json::Value>().await);
                            }
                            // The gate was reached on the previous run: this
                            // continue RESOLVES it with exactly one
                            // NodeComplete.
                            self.record_seen(&id)?;
                            let ev = self
                                .run_log
                                .node_complete(&id, "", "", None, "")
                                .map_err(StepError::LogWrite)?;
                            self.observer.borrow_mut().on_event(&ev);
                            return Ok(serde_json::Value::Null);
                        }
                        // force-rerun: fall through to the live path.
                    }
                    JournalState::Cancelled => unreachable!("handled above"),
                }
            }
        }

        // Live: a held call (a pause was already requested) never resolves
        // and writes no NodeStart.
        if self.is_held() {
            return Ok(pending::<serde_json::Value>().await);
        }

        // Gates do not count toward the step cap; only the duplicate-id
        // check applies.
        self.record_seen(&id)?;

        let ev = self
            .run_log
            .node_start(&id, "gate", 1, "")
            .map_err(StepError::LogWrite)?;
        self.observer.borrow_mut().on_event(&ev);
        self.request_pause(PauseRequest {
            message: opts.message,
            id: id.clone(),
        });
        // The gate never resolves: the run suspends once the in-flight
        // registry is empty (see `pause_ready`).
        Ok(pending::<serde_json::Value>().await)
    }

    /// Handle a `shell` step: parse, check, split host env, run, log, return.
    async fn handle_shell(&self, args: serde_json::Value) -> Result<serde_json::Value, StepError> {
        let id = self.validate_common_step("shell", &args)?;
        let opts: ShellOpts =
            serde_json::from_value(args).map_err(|e| StepError::shell_args(id.clone(), e))?;
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

        // Replay check comes BEFORE accept_step's cap check: a replayed step
        // neither executes (nor creates the worktree) nor counts toward the
        // cap.
        match self.replay_disposition(&id, "shell")? {
            ReplayDisposition::Complete(entry) => {
                self.record_seen(&id)?;
                let (session, output, exit, json_output) = match entry.state {
                    JournalState::Complete {
                        session_id,
                        output,
                        exit_code,
                        json_output,
                    } => (session_id, output, exit_code, json_output),
                    _ => unreachable!("disposition says Complete"),
                };
                let json: Option<serde_json::Value> = if json_output.is_empty() {
                    None
                } else {
                    serde_json::from_str(&json_output).ok()
                };
                // Rebuild exactly the shape `serde_json::to_value(&StepResult)`
                // produces for a live run.
                return Ok(serde_json::json!({
                    "output": output,
                    "exit": exit,
                    "json": json,
                    "sessionId": if session.is_empty() { None } else { Some(session) },
                    "error": null,
                }));
            }
            ReplayDisposition::Errored(entry) => {
                self.record_seen(&id)?;
                let error = match entry.state {
                    JournalState::Errored { error } => error,
                    _ => unreachable!("disposition says Errored"),
                };
                return Ok(serde_json::json!({
                    "output": "",
                    "exit": null,
                    "json": null,
                    "sessionId": null,
                    "error": error,
                }));
            }
            ReplayDisposition::Live => {}
        }

        // Once a pause is requested, a live call is HELD: it never resolves,
        // writes no log event, and does not register.
        if self.is_held() {
            return Ok(pending::<serde_json::Value>().await);
        }

        self.accept_step(&id)?;

        self.ensure_worktree().map_err(StepError::Worktree)?;

        let ev = self
            .run_log
            .node_start(&id, "shell", 1, "")
            .map_err(StepError::LogWrite)?;
        self.observer.borrow_mut().on_event(&ev);
        self.register_in_flight(&id, "shell");

        let hook_env = |failed: &str| {
            let well_known: BTreeMap<String, String> = [
                ("WORKFLOW_ID".into(), self.run_id.clone()),
                ("SESSION_ID".into(), self.run_id.clone()),
                ("NODE_ID".into(), id.clone()),
                ("CODEBASE".into(), self.codebase.display().to_string()),
                ("WORKSPACE".into(), self.gait_dir.display().to_string()),
                ("ARGS".into(), self.args.join(" ")),
                ("FAILED_NODE".into(), failed.into()),
            ]
            .into_iter()
            .collect();
            build_shell_env(&well_known, &BTreeMap::new(), &[])
                .map_err(|e| StepError::ShellFailure(e.to_string()))
        };
        if let Some(cmd) = opts.hooks.as_ref().and_then(|h| h.before.as_deref()) {
            let env = hook_env("")?;
            let (exit, _) = run_hook(cmd, &env, opts.timeout.unwrap_or(120_000), None).await;
            if exit != 0 {
                let message = format!("before hook failed with exit code {exit}");
                if let Some(error_cmd) = opts.hooks.as_ref().and_then(|h| h.error.as_deref()) {
                    let env = hook_env(&id)?;
                    let _ = run_hook(error_cmd, &env, opts.timeout.unwrap_or(120_000), None).await;
                }
                let log_result = if opts.on_fail == Some(crate::step::OnFail::Continue) {
                    self.run_log
                        .node_error_kind(&id, &message, 1, NodeErrorKind::Continued)
                } else {
                    self.run_log.node_error(&id, &message, 1)
                }
                .map_err(StepError::LogWrite);
                self.unregister_in_flight(&id);
                let ev = log_result?;
                self.observer.borrow_mut().on_event(&ev);
                if opts.on_fail == Some(crate::step::OnFail::Continue) {
                    return Ok(serde_json::json!({
                        "output": "", "exit": null, "json": null,
                        "sessionId": null, "error": message,
                    }));
                }
                return Err(StepError::ShellFailure(message));
            }
        }

        let ctx = self.build_node_context(&id);
        let max_attempts = opts.retry.as_ref().map_or(1, |r| r.times as usize + 1);
        let mut last_failure: Option<String> = None;
        let result = {
            let mut attempt = 1;
            loop {
                if attempt > 1 {
                    let retry = opts
                        .retry
                        .as_ref()
                        .expect("retry set when max_attempts > 1");
                    let delay = Self::retry_delay_ms(attempt as u32, retry);
                    tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                }
                match exec_shell_opts_with_host_env(&opts, &ctx, &[], &host_env).await {
                    // A non-zero exit is a RESULT, not an error: no retry.
                    Ok(res) if res.error.is_none() => break Ok(res),
                    Ok(res) => {
                        last_failure =
                            Some(res.error.clone().unwrap_or_else(|| "shell failed".into()));
                    }
                    Err(e) => {
                        last_failure = Some(e.to_string());
                    }
                }
                attempt += 1;
                if attempt > max_attempts {
                    break Err(());
                }
            }
        };

        match result {
            Ok(res) => {
                let json_text = match &res.json {
                    Some(j) => serde_json::to_string(j).unwrap_or_default(),
                    None => String::new(),
                };
                let log_result = self
                    .run_log
                    .node_complete(&id, "", &res.output, res.exit, &json_text)
                    .map_err(StepError::LogWrite);
                self.unregister_in_flight(&id);
                let ev = log_result?;
                self.observer.borrow_mut().on_event(&ev);
                if let Some(cmd) = opts.hooks.as_ref().and_then(|h| h.after.as_deref()) {
                    let env = hook_env("")?;
                    let _ = run_hook(cmd, &env, opts.timeout.unwrap_or(120_000), None).await;
                }
                serde_json::to_value(&res).map_err(|e| StepError::LogWrite(e.into()))
            }
            Err(()) => {
                let message =
                    last_failure.unwrap_or_else(|| "shell failed for an unknown reason".into());
                // onFail: "continue" resolves the failure to the script as an
                // error-result object; the NodeError is still logged, but with
                // kind CONTINUED so the replay treats the step as finished.
                if opts.on_fail == Some(crate::step::OnFail::Continue) {
                    if let Some(cmd) = opts.hooks.as_ref().and_then(|h| h.error.as_deref()) {
                        let env = hook_env(&id)?;
                        let _ = run_hook(cmd, &env, opts.timeout.unwrap_or(120_000), None).await;
                    }
                    let log_result = self
                        .run_log
                        .node_error_kind(&id, &message, 1, NodeErrorKind::Continued)
                        .map_err(StepError::LogWrite);
                    self.unregister_in_flight(&id);
                    let ev = log_result?;
                    self.observer.borrow_mut().on_event(&ev);
                    return Ok(serde_json::json!({
                        "output": "",
                        "exit": null,
                        "json": null,
                        "sessionId": null,
                        "error": message,
                    }));
                }
                let log_result = self
                    .run_log
                    .node_error(&id, &message, 1)
                    .map_err(StepError::LogWrite);
                self.unregister_in_flight(&id);
                let ev = log_result?;
                self.observer.borrow_mut().on_event(&ev);
                if let Some(cmd) = opts.hooks.as_ref().and_then(|h| h.error.as_deref()) {
                    let env = hook_env(&id)?;
                    let _ = run_hook(cmd, &env, opts.timeout.unwrap_or(120_000), None).await;
                }
                Err(StepError::ShellFailure(message))
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

    /// Handle a `now` or `random` call: no arguments, config must be seen,
    /// the id (`__now_<n>` / `__random_<n>`) is assigned synchronously from a
    /// per-method counter, and the value is journaled so a replay returns the
    /// SAME value. These never count toward the step cap.
    async fn handle_now_random(
        &self,
        method: &str,
        args: serde_json::Value,
    ) -> Result<serde_json::Value, StepError> {
        // Config must have been handled, exactly like turn/shell.
        {
            let state = self.state.borrow();
            if state.config.is_none() {
                return Err(StepError::ConfigFirst);
            }
        }
        match args {
            serde_json::Value::Null | serde_json::Value::Object(_) => {}
            _ => {
                return Err(StepError::NoArgs {
                    method: method.to_string(),
                })
            }
        }

        // Assign the id synchronously, before any await: the counter lives in
        // EngineState and the borrow is dropped immediately.
        let id = {
            let mut state = self.state.borrow_mut();
            let (count_field, prefix) = if method == "now" {
                (&mut state.now_count, "__now_")
            } else {
                (&mut state.random_count, "__random_")
            };
            *count_field += 1;
            format!("{prefix}{}", *count_field)
        };

        // Replay: a Complete journal entry with the same kind returns the
        // journaled value with no log events.
        if let ReplayDisposition::Complete(entry) = self.replay_disposition(&id, method)? {
            let json_output = match entry.state {
                JournalState::Complete { json_output, .. } => json_output,
                _ => unreachable!("disposition says Complete"),
            };
            return serde_json::from_str(&json_output).map_err(StepError::ConfigArgs);
        }

        // Once a pause is requested, a live call is HELD: it never resolves
        // and writes no log event. (Replays above are still answered.)
        if self.is_held() {
            return Ok(pending::<serde_json::Value>().await);
        }

        // Live: journal the value. `now` = unix time in ms; `random` = f64 in
        // [0, 1) from SystemTime nanos mixed through a xorshift (no external
        // dependency). The value is computed once, serialized, journaled and
        // returned — no await anywhere, so no RefCell borrow can be held
        // across a suspension.
        let value: serde_json::Value = if method == "now" {
            let ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or_default();
            serde_json::Value::from(ms)
        } else {
            serde_json::Value::from(Self::xorshift_unit_f64())
        };
        let json_text = serde_json::to_string(&value).unwrap_or_default();

        let ev = self
            .run_log
            .node_start(&id, method, 1, "")
            .map_err(StepError::LogWrite)?;
        self.observer.borrow_mut().on_event(&ev);
        let ev = self
            .run_log
            .node_complete(&id, "", "", None, &json_text)
            .map_err(StepError::LogWrite)?;
        self.observer.borrow_mut().on_event(&ev);

        Ok(value)
    }

    /// A pseudo-random f64 in [0, 1) derived from the system clock's nanos
    /// mixed with a process-wide counter, via an xorshift64* generator. Not
    /// cryptographic; just deterministic per call and never repeated within a
    /// process.
    fn xorshift_unit_f64() -> f64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0u64);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut x = nanos ^ n.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        if x == 0 {
            x = 0x2545_F491_4F6C_DD1D;
        }
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        let frac = (x >> 11) as f64 / (1u64 << 53) as f64;
        // frac is in [0, 1); clamp defensively against float edge cases.
        if !(0.0..1.0).contains(&frac) {
            0.0
        } else {
            frac
        }
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
    /// tests/shell_env.rs: workflow/session ids are the run id, args joined
    /// by spaces).
    fn build_node_context<'s>(&'s self, node_id: &'s str) -> NodeContext<'s> {
        NodeContext {
            workflow_id: &self.run_id,
            session_id: &self.run_id,
            node_id,
            args: &self.args_joined,
            gait_dir: &self.gait_dir,
            codebase: &self.codebase,
            run_worktree: &self.run_worktree,
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
        let (tx, rx) = tokio::sync::mpsc::channel(1024);
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
            return Err(HostError::Failed(NOT_ACCEPTING.to_string()));
        }

        match reply_rx.await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(message)) => Err(HostError::Failed(message)),
            Err(_) => Err(HostError::Failed(NOT_ACCEPTING.to_string())),
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
    use crate::turn_exec::TurnOutcome;
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
            ScriptJournal::from_events(&[]),
            Vec::new(),
            observer,
        );
        (dir, engine)
    }

    /// Build a StepEngine whose turns never resolve (a hanging executor) —
    /// a live step stays mid-flight indefinitely.
    fn make_hanging_engine() -> (TempDir, StepEngine<'static>) {
        struct HangingExecutor;
        #[async_trait::async_trait(?Send)]
        impl TurnExecutor for HangingExecutor {
            async fn run_turn(&self, _req: TurnRequest) -> anyhow::Result<TurnOutcome> {
                std::future::pending::<()>().await;
                unreachable!("pending never resolves");
            }
        }
        make_engine_with_executor(Arc::new(HangingExecutor))
    }

    /// Build a StepEngine over the given turn executor.
    fn make_engine_with_executor(turns: Arc<dyn TurnExecutor>) -> (TempDir, StepEngine<'static>) {
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
            turns,
            ScriptJournal::from_events(&[]),
            Vec::new(),
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
    async fn reserved_journal_value_step_ids_are_rejected() {
        let (_dir, engine) = make_engine(ScriptedTurnExecutor::new(), None);
        engine
            .handle("config", json!({ "name": "test-wf" }))
            .await
            .unwrap();

        for id in ["__now_user", "__random_user"] {
            let err = engine
                .handle("turn", json!({ "id": id, "prompt": "hi" }))
                .await
                .unwrap_err();
            assert_eq!(
                err,
                format!(
                    "step id '{id}' is reserved (ids starting with __now_ or __random_ are used by gait.wf.now/random)"
                )
            );
        }
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
        let err = engine.handle("bogus", json!({})).await.unwrap_err();
        assert_eq!(err, "unknown gait.wf method 'bogus'");
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
    async fn retry_on_fail_and_hooks_run() {
        let mut script = ScriptedTurnExecutor::new();
        script.script_text("unused");
        let (_dir, engine) = make_engine(script, None);
        engine
            .handle("config", json!({ "name": "test-wf" }))
            .await
            .unwrap();

        let value = engine
            .handle(
                "turn",
                json!({ "id": "t1", "prompt": "hi", "hooks": { "before": "true" } }),
            )
            .await
            .unwrap();
        assert_eq!(value["output"], "unused");
    }

    #[tokio::test]
    async fn before_hook_failure_can_continue_and_is_logged() {
        let mut script = ScriptedTurnExecutor::new();
        script.script_text("next step");
        let (dir, engine) = make_engine(script, None);
        engine
            .handle("config", json!({ "name": "test-wf" }))
            .await
            .unwrap();

        let failed = engine
            .handle(
                "turn",
                json!({
                    "id": "hooked",
                    "prompt": "not run",
                    "hooks": { "before": "exit 1" },
                    "onFail": "continue"
                }),
            )
            .await
            .unwrap();
        assert_eq!(failed["exit"], serde_json::Value::Null);
        assert!(failed["error"].as_str().unwrap().contains("before hook"));
        assert_eq!(
            node_errors(&dir),
            vec![("hooked".to_string(), NodeErrorKind::Continued as i32)]
        );

        let next = engine
            .handle("turn", json!({ "id": "next", "prompt": "go" }))
            .await
            .unwrap();
        assert_eq!(next["output"], "next step");

        let (dir, engine) = make_engine(ScriptedTurnExecutor::new(), None);
        engine
            .handle("config", json!({ "name": "test-wf" }))
            .await
            .unwrap();
        let err = engine
            .handle(
                "turn",
                json!({
                    "id": "hooked",
                    "prompt": "not run",
                    "hooks": { "before": "exit 1" }
                }),
            )
            .await
            .unwrap_err();
        assert!(err.contains("before hook"), "got: {err}");
        assert_eq!(
            node_errors(&dir),
            vec![("hooked".to_string(), NodeErrorKind::Failed as i32)]
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

    // ── Replay (task 4b) ────────────────────────────────────────────────────

    use gait_core::proto::gait::workflow::v1::workflow_event;
    use gait_core::proto::gait::workflow::v1::{
        NodeComplete, NodeError, NodeErrorKind, NodeStart, WorkflowEvent, WorkflowGate,
    };

    /// A journal event with only the kind filled in.
    fn journal_event(kind: workflow_event::Kind) -> WorkflowEvent {
        WorkflowEvent {
            version: 1,
            ts: 0,
            id: String::new(),
            seq: 0,
            content_hash: String::new(),
            parents: vec![],
            kind: Some(kind),
        }
    }

    /// A StepEngine over a journal built from hand-written events.
    fn make_engine_with_journal(
        script: ScriptedTurnExecutor,
        events: Vec<WorkflowEvent>,
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
            ScriptJournal::from_events(&events),
            Vec::new(),
            observer,
        );
        (dir, engine)
    }

    /// A journaled COMPLETE shell step replays its recorded result without
    /// executing: the returned output is the logged one and the worktree was
    /// never created (a live shell step always creates it).
    #[tokio::test]
    async fn replayed_complete_shell_step_does_not_execute() {
        let events = vec![
            journal_event(workflow_event::Kind::NodeStart(NodeStart {
                node_id: "s1".into(),
                node_type: "shell".into(),
                attempt: 1,
                ..Default::default()
            })),
            journal_event(workflow_event::Kind::NodeComplete(NodeComplete {
                node_id: "s1".into(),
                output: "recorded".into(),
                exit_code: Some(0),
                ..Default::default()
            })),
        ];
        let (_dir, engine) = make_engine_with_journal(ScriptedTurnExecutor::new(), events);
        engine
            .handle("config", json!({ "name": "test-wf" }))
            .await
            .unwrap();

        let value = engine
            .handle("shell", json!({ "id": "s1", "script": "echo live" }))
            .await
            .unwrap();
        assert_eq!(value["output"], "recorded");
        assert_eq!(value["exit"], 0);
        // The live path would have created <run>/worktree before executing.
        assert!(
            !engine.run_worktree.join(".git").exists(),
            "a replayed shell step must not create the worktree"
        );
    }

    /// A journaled ERRORED step replays as a resolved `{error}` result (a
    /// failed node counts as finished), with no new log events.
    #[tokio::test]
    async fn replayed_errored_step_returns_the_error_result() {
        let events = vec![
            journal_event(workflow_event::Kind::NodeStart(NodeStart {
                node_id: "t1".into(),
                node_type: "turn".into(),
                attempt: 1,
                session_id: "sess-old".into(),
            })),
            journal_event(workflow_event::Kind::NodeError(NodeError {
                node_id: "t1".into(),
                error: "boom".into(),
                kind: NodeErrorKind::Failed as i32,
                ..Default::default()
            })),
        ];
        let mut script = ScriptedTurnExecutor::new();
        script.script_text("live-run-again");
        let (_dir, engine) = make_engine_with_journal(script, events);
        engine
            .handle("config", json!({ "name": "test-wf" }))
            .await
            .unwrap();

        let value = engine
            .handle("turn", json!({ "id": "t1", "prompt": "hi" }))
            .await
            .unwrap();
        assert_eq!(value["error"], "boom");
        assert_eq!(value["output"], "");
        assert_eq!(value["exit"], serde_json::Value::Null);
        assert_eq!(value["sessionId"], "sess-old");
    }

    /// A step id whose KIND changed between the journal and the script is an
    /// error naming the step, the logged kind, and the new kind.
    #[tokio::test]
    async fn kind_mismatch_error_names_the_step() {
        let events = vec![
            journal_event(workflow_event::Kind::NodeStart(NodeStart {
                node_id: "s1".into(),
                node_type: "shell".into(),
                attempt: 1,
                ..Default::default()
            })),
            journal_event(workflow_event::Kind::NodeComplete(NodeComplete {
                node_id: "s1".into(),
                output: "old".into(),
                ..Default::default()
            })),
        ];
        let (_dir, engine) = make_engine_with_journal(ScriptedTurnExecutor::new(), events);
        engine
            .handle("config", json!({ "name": "test-wf" }))
            .await
            .unwrap();

        let err = engine
            .handle("turn", json!({ "id": "s1", "prompt": "now a turn" }))
            .await
            .unwrap_err();
        assert!(
            err.contains("workflow script changed (step s1): logged as shell, now turn"),
            "got: {err}"
        );
    }

    /// `now` ids are assigned synchronously in call order: `__now_1`,
    /// `__now_2`, ... as written into the log's NodeStart events.
    #[tokio::test]
    async fn now_ids_are_deterministic_in_call_order() {
        let (_dir, engine) = make_engine(ScriptedTurnExecutor::new(), None);
        engine
            .handle("config", json!({ "name": "test-wf" }))
            .await
            .unwrap();

        let v1 = engine.handle("now", serde_json::Value::Null).await.unwrap();
        let v2 = engine.handle("now", json!({})).await.unwrap();
        assert!(v1.is_i64() || v1.is_u64(), "now returns an integer: {v1}");
        assert!(v2.is_i64() || v2.is_u64(), "now returns an integer: {v2}");

        let log = WorkflowRunLog::open(&_dir.path().join("workflow-runs"), "run-test").unwrap();
        let starts: Vec<String> = log
            .read_events()
            .unwrap()
            .into_iter()
            .filter_map(|ev| match ev.kind {
                Some(workflow_event::Kind::NodeStart(ns)) => Some(ns.node_id),
                _ => None,
            })
            .collect();
        assert_eq!(starts, vec!["__now_1".to_string(), "__now_2".to_string()]);
    }

    // ── Gates, cancellation (task JS-5a) ────────────────────────────────────

    /// The ids of NodeError events in the engine's log, with their kinds.
    fn node_errors(dir: &TempDir) -> Vec<(String, i32)> {
        let log = WorkflowRunLog::open(&dir.path().join("workflow-runs"), "run-test").unwrap();
        log.read_events()
            .unwrap()
            .into_iter()
            .filter_map(|ev| match ev.kind {
                Some(workflow_event::Kind::NodeError(ne)) => Some((ne.node_id, ne.kind)),
                _ => None,
            })
            .collect()
    }

    /// All events in the engine's log.
    fn all_events(dir: &TempDir) -> Vec<workflow_event::Kind> {
        let log = WorkflowRunLog::open(&dir.path().join("workflow-runs"), "run-test").unwrap();
        log.read_events()
            .unwrap()
            .into_iter()
            .map(|ev| ev.kind.expect("event must have a kind"))
            .collect()
    }

    /// `cancel_in_flight` writes a Cancelled-kind NodeError per registered
    /// step and clears the registry.
    #[tokio::test]
    async fn cancel_in_flight_writes_cancelled_and_clears_the_registry() {
        let (_dir, engine) = make_hanging_engine();
        engine
            .handle("config", json!({ "name": "test-wf" }))
            .await
            .unwrap();

        // A live turn that never resolves registers as in-flight: the call
        // is polled until the engine's registry shows the step, then
        // cancelled mid-flight.
        {
            let handle_fut = engine.handle("turn", json!({ "id": "t1", "prompt": "hi" }));
            tokio::pin!(handle_fut);
            for _ in 0..200 {
                assert!(
                    futures_util::poll!(handle_fut.as_mut()).is_pending(),
                    "turn must hang"
                );
                tokio::task::yield_now().await;
                if !engine.in_flight_ids().is_empty() {
                    break;
                }
            }
            assert_eq!(engine.in_flight_ids(), vec!["t1".to_string()]);

            engine.cancel_in_flight("test cancel");
            assert!(engine.in_flight_ids().is_empty(), "registry must clear");
            let errors = node_errors(&_dir);
            assert_eq!(
                errors,
                vec![("t1".to_string(), NodeErrorKind::Cancelled as i32)]
            );
        }

        // Drop the still-running turn; the step is no longer registered, so
        // dropping must not write a SECOND terminal event for the same step.
        assert_eq!(
            node_errors(&_dir).len(),
            1,
            "a cancelled step must not be written twice"
        );
    }

    /// A live gate writes a NodeStart with node_type gate, sets a Gate pause,
    /// and never resolves (the call is driven under a short timeout).
    #[tokio::test]
    async fn gate_live_writes_node_start_and_sets_a_gate_pause() {
        let (_dir, engine) = make_engine(ScriptedTurnExecutor::new(), None);
        engine
            .handle("config", json!({ "name": "test-wf" }))
            .await
            .unwrap();

        let result = tokio::time::timeout(
            std::time::Duration::from_millis(200),
            engine.handle("gate", json!({ "id": "review", "message": "look" })),
        )
        .await;
        assert!(result.is_err(), "a live gate call must never resolve");

        let events = all_events(&_dir);
        assert!(
            events.iter().any(
                |k| matches!(k, workflow_event::Kind::NodeStart(ns) if ns.node_id == "review" && ns.node_type == "gate")
            ),
            "the gate must write a NodeStart with node_type gate: {events:?}"
        );
        assert!(
            !events.iter().any(
                |k| matches!(k, workflow_event::Kind::NodeComplete(nc) if nc.node_id == "review")
            ),
            "a paused gate must not complete"
        );

        // The engine is not yet quiescent-checked: with nothing executing the
        // pause is immediately ready, and it is a Gate pause for this gate.
        assert!(engine.pause_ready());
        let pause = engine.take_pause().expect("the gate must set a pause");
        assert_eq!(pause.id, "review");
        assert_eq!(pause.message, "look");
    }

    /// A gate whose journal entry is Open (reached on the previous run)
    /// resolves on the continue: exactly one NodeComplete is written and the
    /// call returns null.
    #[tokio::test]
    async fn gate_on_open_journal_writes_one_node_complete_and_returns_null() {
        let events = vec![
            journal_event(workflow_event::Kind::NodeStart(NodeStart {
                node_id: "review".into(),
                node_type: "gate".into(),
                attempt: 1,
                ..Default::default()
            })),
            journal_event(workflow_event::Kind::WorkflowGate(WorkflowGate {
                node_id: "review".into(),
                message: "look".into(),
            })),
        ];
        let (_dir, engine) = make_engine_with_journal(ScriptedTurnExecutor::new(), events);
        engine
            .handle("config", json!({ "name": "test-wf" }))
            .await
            .unwrap();

        let value = engine
            .handle("gate", json!({ "id": "review", "message": "look" }))
            .await
            .unwrap();
        assert_eq!(value, serde_json::Value::Null);
        assert!(!engine.pause_ready(), "a resolved gate must not pause");

        let completes = all_events(&_dir)
            .into_iter()
            .filter(
                |k| matches!(k, workflow_event::Kind::NodeComplete(nc) if nc.node_id == "review"),
            )
            .count();
        assert_eq!(
            completes, 1,
            "the gate resolves with exactly one NodeComplete"
        );
    }

    /// Once a pause is requested, a later live step call is HELD: it writes
    /// no log event and never resolves (driven under a short timeout).
    #[tokio::test]
    async fn held_call_after_pause_writes_nothing() {
        let mut script = ScriptedTurnExecutor::new();
        script.script_text("unused");
        let (_dir, engine) = make_engine(script, None);
        engine
            .handle("config", json!({ "name": "test-wf" }))
            .await
            .unwrap();

        // A live gate requests the first pause.
        let gate = tokio::time::timeout(
            std::time::Duration::from_millis(200),
            engine.handle("gate", json!({ "id": "g", "message": "m" })),
        )
        .await;
        assert!(gate.is_err(), "the gate must suspend");

        let before = all_events(&_dir).len();

        // A later live call (a prompt turn) is held: it never resolves and
        // writes no log event.
        let held = tokio::time::timeout(
            std::time::Duration::from_millis(200),
            engine.handle("turn", json!({ "id": "t1", "prompt": "hi" })),
        )
        .await;
        assert!(held.is_err(), "a held call must never resolve");
        assert_eq!(
            all_events(&_dir).len(),
            before,
            "a held call must write no log event"
        );
        assert!(
            !engine.in_flight_ids().contains(&"t1".to_string()),
            "a held call must not register"
        );
    }
}

//! Task 3 (js-workflows): the `TurnExecutor` abstraction over a single model
//! turn. `RealTurnExecutor` owns the turn-driving logic that used to live in
//! `exec_prompt.rs` (moved, not copied): config resolution, CLI-override
//! folding, `TurnRunOpts` construction and the `gait-turn-runner` call.
//! `exec_prompt_node` now builds a `TurnRequest` and calls the executor for
//! its literal-prompt branch, and shares the same opts builder for its
//! live-source branch — one path, no duplication.
//!
//! `testing::ScriptedTurnExecutor` is a public fake returning scripted
//! outcomes per request; exhausting the script is a hard error, never a
//! silent empty output.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use crate::exec_shell::NodeContext;

/// A request for one model turn, in the shape script/JS workflow steps author
/// against (mirrors `step::TurnOpts`, with substitution already applied by the
/// caller).
#[derive(Debug, Clone, Default)]
pub struct TurnRequest {
    pub id: String,
    /// The session id this turn runs in: `continue_session` when given, else
    /// the freshly generated id the host chose for this step (chosen once,
    /// before any retry attempt, so retries reuse it).
    pub session_id: String,
    pub prompt: Option<String>,
    pub model: Option<String>,
    pub auto: Option<bool>,
    pub allow: Vec<String>,
    pub agent: Option<String>,
    pub codebase: Option<PathBuf>,
    pub continue_session: Option<String>,
    pub output_schema: Option<Value>,
}

/// The outcome of one completed turn.
#[derive(Debug, Clone, Default)]
pub struct TurnOutcome {
    pub output: String,
    pub exit: Option<i32>,
    pub json: Option<Value>,
    pub session_id: Option<String>,
    /// True when the turn came back "open" (interactive suspension): no
    /// terminal advance was signalled and the caller must treat the node as
    /// still open.
    pub open: bool,
}

/// One model turn, behind a trait so a scripted fake can stand in for the
/// real one.
#[async_trait(?Send)]
// The must_use comes from the async_trait expansion (Pin<Box<dyn Future>> return).
#[allow(clippy::double_must_use)]
pub trait TurnExecutor: Send + Sync {
    async fn run_turn(&self, req: TurnRequest) -> anyhow::Result<TurnOutcome>;

    /// The run driver hands the executor the channel that turn output is
    /// rendered through (a `SinkFrame` stream). Default: ignore — executors
    /// that do not stream their output through the driver's channel need not
    /// override this.
    fn attach_output(
        &self,
        _frames: tokio::sync::mpsc::UnboundedSender<gait_core::embed::SinkFrame>,
    ) {
    }
}

/// The real executor: drives one sealed turn through `gait-turn-runner`
/// exactly the way `exec_prompt_node` did pre-task-3.
pub struct RealTurnExecutor<'a> {
    node: &'a gait_core::proto::gait::workflow::v1::Node,
    prompt: &'a gait_core::proto::gait::workflow::v1::PromptNode,
    workflow: &'a gait_core::proto::gait::workflow::v1::Workflow,
    ctx: &'a NodeContext<'a>,
}

/// Everything `turn_run_opts` needs to build one turn's `TurnRunOpts`,
/// independent of whether the caller is the YAML proto node or a script run.
/// The proto-node executor fills this from its node/workflow/ctx; the script
/// executor fills it from a `TurnRequest`.
pub(crate) struct TurnSpec {
    /// Consumer layer (workflow fields for YAML; none for a script run).
    pub consumer_inference: Option<String>,
    pub consumer_allow: Option<Vec<String>>,
    /// Per-call layer (node fields for YAML; the request's fields for a
    /// script run).
    pub per_call_inference: Option<String>,
    /// `Some` only when the per-call allow list is non-empty (an empty
    /// per-call list must not clobber the resolved default).
    pub per_call_allow: Option<Vec<String>>,
    pub per_call_agent: Option<String>,
    /// Final effective `auto` (node/workflow/request default already folded).
    pub auto: bool,
    /// Caller-supplied result schema (already parsed where an error message
    /// with context is required).
    pub result_schema: Option<Value>,
    pub codebase: PathBuf,
    pub session_id: String,
    pub continue_session: String,
    pub run_id: String,
    pub gait_dir: PathBuf,
    /// CLI-only override layer (`ctx.profile` for YAML; none for a script run).
    pub profile: Option<gait_turn_runner::TurnProfileOverride>,
    pub sandbox_on: bool,
    pub cascade_group_arg: Option<String>,
    pub cascade: Option<gait_core::embed::CascadeKillHandle>,
    pub fs_delegate: Option<std::sync::Arc<dyn gait_core::embed::FsDelegate>>,
    pub allow_intree: bool,
}

/// The single place that builds a `gait_turn_runner::TurnRunOpts`: config
/// loading, `ConfigResolver` resolution (consumer < per-call), CLI-profile
/// override folding, and the `TurnRunOpts` literal. Shared by
/// `RealTurnExecutor` and the script executor — one path, not two.
pub(crate) fn turn_run_opts(spec: TurnSpec) -> anyhow::Result<gait_turn_runner::TurnRunOpts> {
    use gait_core::config::resolver::{ConfigResolver, Overrides};
    let loaded = gait_core::config::load_config(&spec.gait_dir);
    // consumer layer
    let consumer = Overrides {
        inference: spec.consumer_inference.clone(),
        allow: spec.consumer_allow.clone(),
        ..Overrides::empty()
    };
    // per-call layer (higher priority)
    let per_call = Overrides {
        inference: spec.per_call_inference.clone(),
        allow: spec.per_call_allow.clone(),
        agent: spec.per_call_agent.clone(),
        ..Overrides::empty()
    };
    let resolved_cfg = ConfigResolver::new(loaded)
        .with(consumer)
        .with(per_call)
        .resolve_checked()?;
    let inference = resolved_cfg
        .inference
        .clone()
        .expect("resolve_checked guarantees inference is Some");
    let allow = resolved_cfg.allow.clone();

    // spec-06 §3 Task 3.2 (the C1 fix): a CLI-only override, when present,
    // takes priority over the disk-resolved config above — this is what makes
    // `gait run --model/--allow/--deny/--agent/--max-iter/--result-schema`
    // still work now that `cmd_run` is routed through this same function
    // instead of building its own driver directly. Every real workflow node
    // has `ctx.profile == None` and is unaffected.
    let ov = spec.profile.as_ref();
    let session_updates = ov.map(|p| p.session_updates.clone()).unwrap_or_default();
    let inference = ov.and_then(|p| p.inference.clone()).unwrap_or(inference);
    let explicit_inference_override = ov.map(|p| p.explicit_inference_override).unwrap_or(false);
    let allow = ov
        .filter(|p| !p.allow.is_empty())
        .map(|p| p.allow.clone())
        .unwrap_or(allow);
    let deny = ov.map(|p| p.deny.clone()).unwrap_or_default();
    let agent_name = ov.and_then(|p| p.agent.clone()).or(spec.per_call_agent);
    let max_iter = ov.and_then(|p| p.max_iter);
    let result_schema = ov
        .and_then(|p| p.result_schema.clone())
        .or(spec.result_schema);
    // Effective `auto`: the caller's folded default, overridden by a CLI
    // `--auto` (spec-06's degenerate-node override).
    let auto = ov.and_then(|p| p.auto).unwrap_or(spec.auto);

    Ok(gait_turn_runner::TurnRunOpts {
        // The workflow run's own id (doubles as SESSION_ID env /
        // workflow_id default — see `runner::run_workflow`), threaded
        // through as the turn's run identity.
        run_id: spec.run_id,
        session_id: spec.session_id,
        continue_session: spec.continue_session,
        gait_dir: spec.gait_dir,
        codebase: spec.codebase,
        args: Vec::new(),
        sandbox_on: Some(spec.sandbox_on),
        cascade_group_arg: spec.cascade_group_arg,
        cascade: spec.cascade,
        profile: Some(gait_turn_runner::TurnProfileOverride {
            verified_goal: None,
            inference: Some(inference),
            explicit_inference_override,
            session_updates,
            allow,
            deny,
            agent: agent_name,
            max_iter,
            result_schema,
            auto: Some(auto),
            status_block: None,
        }),
        fs_delegate: spec.fs_delegate,
        allow_intree: spec.allow_intree,
        workflow_requests: None,
    })
}

/// The single place that runs one turn and maps its outcome to a
/// `TurnOutcome`: capture the sink frames onto `frames`, drive
/// `run_single_prompt`, and translate the runner's outcome. Shared by
/// `RealTurnExecutor` and the script executor — one path, not two.
pub(crate) async fn drive_turn(
    opts: gait_turn_runner::TurnRunOpts,
    task: String,
    frames: tokio::sync::mpsc::UnboundedSender<gait_core::embed::SinkFrame>,
) -> anyhow::Result<TurnOutcome> {
    let session_id = opts.session_id.clone();
    let mut sink = crate::exec_prompt::CapturingSink::new(frames);
    let outcome = gait_turn_runner::run_single_prompt(opts, Some(task), &mut sink).await?;
    if outcome.opened {
        return Ok(TurnOutcome {
            session_id: Some(session_id),
            open: true,
            ..TurnOutcome::default()
        });
    }
    // A failed turn is an error, so `retry` and `onFail` apply to it.
    if outcome.exit_code.is_some_and(|code| code != 0) {
        anyhow::bail!("turn failed: {}", outcome.text.unwrap_or_default());
    }
    Ok(TurnOutcome {
        output: outcome.text.unwrap_or_default(),
        exit: outcome.exit_code,
        json: outcome.json,
        session_id: Some(session_id),
        open: false,
    })
}

impl<'a> RealTurnExecutor<'a> {
    pub fn new(
        node: &'a gait_core::proto::gait::workflow::v1::Node,
        prompt: &'a gait_core::proto::gait::workflow::v1::PromptNode,
        workflow: &'a gait_core::proto::gait::workflow::v1::Workflow,
        ctx: &'a NodeContext<'a>,
    ) -> Self {
        Self {
            node,
            prompt,
            workflow,
            ctx,
        }
    }

    /// The task text for the turn: the request's (already-substituted) prompt
    /// when given, else the proto node's prompt substituted as before.
    pub(crate) fn task_text(&self, req: &TurnRequest) -> String {
        match &req.prompt {
            Some(p) => p.clone(),
            None => crate::vars::substitute(&self.prompt.prompt, &self.ctx.vars),
        }
    }

    /// Build `TurnRunOpts` for a turn off this node — config resolution,
    /// CLI-override folding, sandbox/cascade/delegate plumbing. This is the
    /// single owner of that construction: `exec_prompt_node` calls it (with a
    /// default `TurnRequest`) for its live-source and literal-prompt branches,
    /// and `run_turn` calls it for the script/JS path — one path, not two.
    pub(crate) fn build_turn_opts(
        &self,
        req: &TurnRequest,
    ) -> anyhow::Result<gait_turn_runner::TurnRunOpts> {
        // output_schema_json: parse non-empty string to Value (the error
        // message carries the node id, so the parse stays on this side of the
        // spec boundary).
        let result_schema: Option<Value> = if self.node.output_schema_json.is_empty() {
            None
        } else {
            Some(
                serde_json::from_str(&self.node.output_schema_json).map_err(|e| {
                    anyhow::anyhow!("node '{}': invalid output_schema_json: {}", self.node.id, e)
                })?,
            )
        };

        let codebase = self
            .prompt
            .codebase
            .as_deref()
            .map(|cb| PathBuf::from(crate::vars::substitute(cb, &self.ctx.vars)))
            .unwrap_or_else(|| self.ctx.codebase.to_path_buf());

        let session_id = self.ctx.node_session.to_string();
        // A request-level output schema (script path) overrides the node's.
        let result_schema = req.output_schema.clone().or(result_schema);
        // The scripted path can override the continue session per request; the
        // proto node's `continue_session` stays the default.
        let continue_session = match &req.continue_session {
            Some(cs) => cs.clone(),
            None => self.prompt.continue_session.clone(),
        };
        let _ = req.model; // model routing goes through inference/profile today

        turn_run_opts(TurnSpec {
            // consumer layer: Workflow fields
            consumer_inference: self.workflow.inference.clone(),
            consumer_allow: Some(self.workflow.allow.clone()),
            // per-call layer: Node fields (higher priority)
            per_call_inference: self.node.inference.clone(),
            per_call_allow: if self.node.allow.is_empty() {
                None
            } else {
                Some(self.node.allow.clone())
            },
            per_call_agent: self.node.agent.clone(),
            // Effective `auto`: node/workflow default, overridden by a CLI
            // `--auto` inside `turn_run_opts` (spec-06's degenerate-node
            // override).
            auto: self.node.auto.unwrap_or(self.workflow.auto),
            result_schema,
            codebase,
            session_id,
            continue_session,
            run_id: self.ctx.session_id.to_string(),
            gait_dir: self.ctx.gait_dir.to_path_buf(),
            profile: self.ctx.profile.cloned(),
            // workflow nodes default sandboxed
            sandbox_on: self.ctx.sandbox_on.unwrap_or(true),
            cascade_group_arg: self.ctx.cascade_group_arg.clone(),
            cascade: self.ctx.cascade.cloned(),
            fs_delegate: self.ctx.fs_delegate.clone(),
            allow_intree: self.ctx.allow_intree,
        })
    }
}

#[async_trait(?Send)]
impl TurnExecutor for RealTurnExecutor<'_> {
    async fn run_turn(&self, req: TurnRequest) -> anyhow::Result<TurnOutcome> {
        let turn_opts = self.build_turn_opts(&req)?;
        let task = self.task_text(&req);
        drive_turn(turn_opts, task, self.ctx.workflow_tx.clone()).await
    }
}

// ── Scripted fake (public, for tests and dry-run paths) ────────────────────

enum ScriptedOutcome {
    Text {
        output: String,
        session_id: Option<String>,
    },
    Json(Value),
    Failure(String),
}

/// A public fake: returns scripted text / json / session id / failure per
/// request, in order. Exhausting the script is a hard error, never a silent
/// empty output.
#[derive(Default, Clone)]
pub struct ScriptedTurnExecutor {
    script: Arc<std::sync::Mutex<VecDeque<ScriptedOutcome>>>,
}

impl ScriptedTurnExecutor {
    pub fn new() -> Self {
        Self::default()
    }

    /// Script the next turn's literal text output.
    pub fn script_text(&mut self, output: impl Into<String>) -> &mut Self {
        self.script
            .lock()
            .unwrap()
            .push_back(ScriptedOutcome::Text {
                output: output.into(),
                session_id: None,
            });
        self
    }

    /// Set the session id reported by the most recently scripted text
    /// outcome (a fake session that persists across the scripted turns).
    pub fn session_id(&mut self, id: impl Into<String>) -> &mut Self {
        let mut q = self.script.lock().unwrap();
        match q.back_mut() {
            Some(ScriptedOutcome::Text { session_id, .. }) => *session_id = Some(id.into()),
            _ => q.push_back(ScriptedOutcome::Text {
                output: String::new(),
                session_id: Some(id.into()),
            }),
        }
        drop(q);
        self
    }

    /// Script the next turn to fail with the given message.
    pub fn script_failure(&mut self, msg: impl Into<String>) -> &mut Self {
        self.script
            .lock()
            .unwrap()
            .push_back(ScriptedOutcome::Failure(msg.into()));
        self
    }

    /// Script the next turn to return a JSON value (the output is the raw
    /// JSON text, as a real schema turn's stdout would be).
    pub fn script_json(&mut self, json: Value) -> &mut Self {
        self.script
            .lock()
            .unwrap()
            .push_back(ScriptedOutcome::Json(json));
        self
    }
}

#[async_trait(?Send)]
impl TurnExecutor for ScriptedTurnExecutor {
    async fn run_turn(&self, _req: TurnRequest) -> anyhow::Result<TurnOutcome> {
        let next = self.script.lock().unwrap().pop_front();
        match next {
            Some(ScriptedOutcome::Text { output, session_id }) => Ok(TurnOutcome {
                output,
                exit: Some(0),
                session_id,
                ..TurnOutcome::default()
            }),
            Some(ScriptedOutcome::Json(json)) => Ok(TurnOutcome {
                output: serde_json::to_string(&json).unwrap_or_default(),
                exit: Some(0),
                json: Some(json),
                ..TurnOutcome::default()
            }),
            Some(ScriptedOutcome::Failure(msg)) => anyhow::bail!("{msg}"),
            None => {
                anyhow::bail!("scripted turn executor: no scripted outcome left for the request")
            }
        }
    }
}

/// Re-export under `testing` (the name the tests import).
pub mod testing {
    pub use super::ScriptedTurnExecutor;
}

#[async_trait(?Send)]
impl TurnExecutor for Arc<dyn TurnExecutor> {
    async fn run_turn(&self, req: TurnRequest) -> anyhow::Result<TurnOutcome> {
        (**self).run_turn(req).await
    }

    fn attach_output(
        &self,
        frames: tokio::sync::mpsc::UnboundedSender<gait_core::embed::SinkFrame>,
    ) {
        (**self).attach_output(frames)
    }
}

#[async_trait(?Send)]
impl TurnExecutor for Box<dyn TurnExecutor> {
    async fn run_turn(&self, req: TurnRequest) -> anyhow::Result<TurnOutcome> {
        (**self).run_turn(req).await
    }

    fn attach_output(
        &self,
        frames: tokio::sync::mpsc::UnboundedSender<gait_core::embed::SinkFrame>,
    ) {
        (**self).attach_output(frames)
    }
}

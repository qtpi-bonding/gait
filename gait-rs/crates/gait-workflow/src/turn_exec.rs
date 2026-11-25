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
pub trait TurnExecutor: Send + Sync {
    async fn run_turn(&self, req: TurnRequest) -> anyhow::Result<TurnOutcome>;
}

/// The real executor: drives one sealed turn through `gait-turn-runner`
/// exactly the way `exec_prompt_node` did pre-task-3.
pub struct RealTurnExecutor<'a> {
    node: &'a gait_core::proto::gait::workflow::v1::Node,
    prompt: &'a gait_core::proto::gait::workflow::v1::PromptNode,
    workflow: &'a gait_core::proto::gait::workflow::v1::Workflow,
    ctx: &'a NodeContext<'a>,
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
        use gait_core::config::resolver::{ConfigResolver, Overrides};
        let loaded = gait_core::config::load_config(self.ctx.gait_dir);
        // consumer layer: Workflow fields
        let consumer = Overrides {
            inference: self.workflow.inference.clone(),
            allow: Some(self.workflow.allow.clone()),
            ..Overrides::empty()
        };
        // per-call layer: Node fields (higher priority)
        let per_call = Overrides {
            inference: self.node.inference.clone(),
            allow: if self.node.allow.is_empty() {
                None
            } else {
                Some(self.node.allow.clone())
            },
            agent: self.node.agent.clone(),
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
        let auto = self.node.auto.unwrap_or(self.workflow.auto);

        // output_schema_json: parse non-empty string to Value
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

        // spec-06 §3 Task 3.2 (the C1 fix): a CLI-only override, when present,
        // takes priority over the disk-resolved config above — this is what makes
        // `gait run --model/--allow/--deny/--agent/--max-iter/--result-schema`
        // still work now that `cmd_run` is routed through this same function
        // instead of building its own driver directly. Every real workflow node
        // has `ctx.profile == None` and is unaffected.
        let ov = self.ctx.profile;
        let session_updates = ov.map(|p| p.session_updates.clone()).unwrap_or_default();
        let inference = ov.and_then(|p| p.inference.clone()).unwrap_or(inference);
        let explicit_inference_override =
            ov.map(|p| p.explicit_inference_override).unwrap_or(false);
        let allow = ov
            .filter(|p| !p.allow.is_empty())
            .map(|p| p.allow.clone())
            .unwrap_or(allow);
        let deny = ov.map(|p| p.deny.clone()).unwrap_or_default();
        let agent_name = ov
            .and_then(|p| p.agent.clone())
            .or_else(|| self.node.agent.clone());
        let max_iter = ov.and_then(|p| p.max_iter);
        let result_schema = ov.and_then(|p| p.result_schema.clone()).or(result_schema);
        // Effective `auto`: node/workflow default, overridden by a CLI `--auto`
        // (spec-06's degenerate-node override).
        let auto = ov.and_then(|p| p.auto).unwrap_or(auto);
        let sandbox_on = self.ctx.sandbox_on.unwrap_or(true); // workflow nodes default sandboxed

        // The scripted path can override the continue session per request; the
        // proto node's `continue_session` stays the default.
        let continue_session = match &req.continue_session {
            Some(cs) => cs.clone(),
            None => self.prompt.continue_session.clone(),
        };
        // A request-level output schema (script path) overrides the node's.
        let result_schema = req.output_schema.clone().or(result_schema);
        let _ = req.model; // model routing goes through inference/profile today

        Ok(gait_turn_runner::TurnRunOpts {
            // The workflow run's own id (doubles as SESSION_ID env /
            // workflow_id default — see `runner::run_workflow`), threaded
            // through as the turn's run identity.
            run_id: self.ctx.session_id.to_string(),
            session_id,
            continue_session,
            gait_dir: self.ctx.gait_dir.to_path_buf(),
            codebase,
            args: Vec::new(),
            sandbox_on: Some(sandbox_on),
            cascade_group_arg: self.ctx.cascade_group_arg.clone(),
            cascade: self.ctx.cascade.cloned(),
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
            }),
            fs_delegate: self.ctx.fs_delegate.clone(),
            allow_intree: self.ctx.allow_intree,
            workflow_requests: None,
        })
    }
}

#[async_trait(?Send)]
impl TurnExecutor for RealTurnExecutor<'_> {
    async fn run_turn(&self, req: TurnRequest) -> anyhow::Result<TurnOutcome> {
        let turn_opts = self.build_turn_opts(&req)?;
        let session_id = turn_opts.session_id.clone();
        let task = self.task_text(&req);
        let mut sink = crate::exec_prompt::CapturingSink::new(self.ctx.workflow_tx.clone());
        let outcome = gait_turn_runner::run_single_prompt(turn_opts, Some(task), &mut sink).await?;
        if outcome.opened {
            return Ok(TurnOutcome {
                session_id: Some(session_id),
                open: true,
                ..TurnOutcome::default()
            });
        }
        Ok(TurnOutcome {
            output: outcome.text.unwrap_or_default(),
            exit: outcome.exit_code,
            json: outcome.json,
            session_id: Some(session_id),
            open: false,
        })
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
}

#[async_trait(?Send)]
impl TurnExecutor for Box<dyn TurnExecutor> {
    async fn run_turn(&self, req: TurnRequest) -> anyhow::Result<TurnOutcome> {
        (**self).run_turn(req).await
    }
}

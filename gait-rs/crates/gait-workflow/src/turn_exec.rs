//! The `TurnExecutor` abstraction over a single model turn, the shared
//! `TurnSpec`/`turn_run_opts` builder, and the shared `drive_turn` runner.
//! `turn_run_opts` and `drive_turn` are each the single path used by the
//! script turn executor (`script_turns::ScriptTurnExecutor`) — one path, no
//! duplication.
//!
//! `testing::ScriptedTurnExecutor` is a public fake returning scripted
//! outcomes per request; exhausting the script is a hard error, never a
//! silent empty output.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

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
    pub prompt: String,
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

/// Everything `turn_run_opts` needs to build one turn's `TurnRunOpts`.
/// The script turn executor fills this from a `TurnRequest`.
pub(crate) struct TurnSpec {
    /// The script request's fields.
    pub per_call_inference: Option<String>,
    /// `Some` only when the per-call allow list is non-empty (an empty
    /// allow list must not clobber the resolved default).
    pub per_call_allow: Option<Vec<String>>,
    pub per_call_agent: Option<String>,
    /// The request's effective `auto` (request default already folded).
    pub auto: bool,
    /// Caller-supplied result schema (already parsed where an error message
    /// with context is required).
    pub result_schema: Option<Value>,
    pub codebase: PathBuf,
    pub session_id: String,
    pub continue_session: String,
    pub run_id: String,
    pub gait_dir: PathBuf,
}

/// The single place that builds a `gait_turn_runner::TurnRunOpts`: config
/// loading, `ConfigResolver` resolution, and the `TurnRunOpts` literal.
/// Used by the script turn executor — one path, not two.
pub(crate) fn turn_run_opts(spec: TurnSpec) -> anyhow::Result<gait_turn_runner::TurnRunOpts> {
    use gait_core::config::resolver::{ConfigResolver, Overrides};
    let loaded = gait_core::config::load_config(&spec.gait_dir);
    // per-call layer
    let per_call = Overrides {
        inference: spec.per_call_inference.clone(),
        allow: spec.per_call_allow.clone(),
        agent: spec.per_call_agent.clone(),
        ..Overrides::empty()
    };
    let resolved_cfg = ConfigResolver::new(loaded)
        .with(per_call)
        .resolve_checked()?;
    let inference = resolved_cfg
        .inference
        .clone()
        .expect("resolve_checked guarantees inference is Some");
    let allow = resolved_cfg.allow.clone();

    Ok(gait_turn_runner::TurnRunOpts {
        // The workflow run's own id (doubles as SESSION_ID env /
        // workflow_id default), threaded through as the turn's run identity.
        run_id: spec.run_id,
        session_id: spec.session_id,
        continue_session: spec.continue_session,
        gait_dir: spec.gait_dir,
        codebase: spec.codebase,
        args: Vec::new(),
        sandbox_on: Some(true),
        cascade_group_arg: None,
        cascade: None,
        profile: Some(gait_turn_runner::TurnProfileOverride {
            verified_goal: None,
            inference: Some(inference),
            explicit_inference_override: false,
            session_updates: Default::default(),
            allow,
            deny: Vec::new(),
            agent: spec.per_call_agent,
            max_iter: None,
            result_schema: spec.result_schema,
            auto: Some(spec.auto),
            status_block: None,
        }),
        fs_delegate: None,
        allow_intree: false,
    })
}

/// The single place that runs one turn and maps its outcome to a
/// `TurnOutcome`: capture the sink frames onto `frames`, drive
/// `run_single_prompt`, and translate the runner's outcome. Shared by all
/// turn executors — one path, not two.
pub(crate) async fn drive_turn(
    opts: gait_turn_runner::TurnRunOpts,
    task: String,
    frames: tokio::sync::mpsc::UnboundedSender<gait_core::embed::SinkFrame>,
) -> anyhow::Result<TurnOutcome> {
    let session_id = opts.session_id.clone();
    let mut sink = crate::capturing_sink::CapturingSink::new(frames);
    let outcome = gait_turn_runner::run_single_prompt(opts, Some(task), &mut sink).await?;
    // A failed turn is an error, so `retry` and `onFail` apply to it.
    if outcome.exit_code.is_some_and(|code| code != 0) {
        anyhow::bail!("turn failed: {}", outcome.text.unwrap_or_default());
    }
    Ok(TurnOutcome {
        output: outcome.text.unwrap_or_default(),
        exit: outcome.exit_code,
        json: outcome.json,
        session_id: Some(session_id),
    })
}

// ── Scripted fake (public, for tests and dry-run paths) ────────────────────

enum ScriptedOutcome {
    Text {
        output: String,
        session_id: Option<String>,
    },
    Json {
        json: Value,
        session_id: Option<String>,
    },
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

    /// Set the session id reported by the most recently scripted text or
    /// json outcome (a fake session that persists across the scripted
    /// turns).
    pub fn session_id(&mut self, id: impl Into<String>) -> &mut Self {
        let mut q = self.script.lock().unwrap();
        match q.back_mut() {
            Some(ScriptedOutcome::Text { session_id, .. }) => *session_id = Some(id.into()),
            Some(ScriptedOutcome::Json { session_id, .. }) => *session_id = Some(id.into()),
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
            .push_back(ScriptedOutcome::Json {
                json,
                session_id: None,
            });
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
            Some(ScriptedOutcome::Json { json, session_id }) => Ok(TurnOutcome {
                output: serde_json::to_string(&json).unwrap_or_default(),
                exit: Some(0),
                json: Some(json),
                session_id,
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

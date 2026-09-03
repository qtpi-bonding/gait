//! Shared sealed-turn execution runner (gait-min-composition Task 2).
//!
//! Owns the sealed-turn execution logic that used to be entangled with
//! `gait-workflow`'s single-node "workflow" wrapper
//! (`gait_workflow::single_prompt_run`). CLI, REPL, ACP, and `gait-workflow`
//! itself all drive an ordinary (non-DAG) turn through this crate's
//! [`run_turn_source`]/[`run_single_prompt`], independent of any workflow
//! concept — this crate never references `SinkFrame` or `NodeOutput`, both of
//! which remain `gait-workflow`-internal.

use std::path::PathBuf;
use std::sync::Arc;

use gait_core::embed::{
    self, BrainLaunch, Diag, LoopConfig, TurnDriver, TurnSink, TurnSource, TurnStart,
};
use gait_core::proto::gait::log::v1::{RunResult, StreamItem};
use gait_core::Session;

/// Per-turn driver: supplies `brain_launch()` so `run_loop` can spawn `gait
/// __brain` for each turn. Moved unchanged (field-for-field) from
/// `gait-workflow::exec_prompt::PromptNodeDriver` — sealed turns are no
/// longer workflow-specific, so this driver no longer lives under
/// `gait-workflow`.
struct SealedTurnDriver {
    codebase: PathBuf,
    /// Named inference selected for this turn.
    inference: String,
    explicit_inference_override: bool,
    /// Allow list (replaces config.allow).
    allow: Vec<String>,
    /// Optional named agent.
    agent_name: Option<String>,
    /// JSON Schema for `structured_done.result`.
    result_schema: Option<serde_json::Value>,
    /// auto permissions gate.
    auto: bool,
    /// Sandbox config (loaded once from gait.jsonc).
    sandbox_cfg: gait_core::config::SandboxConfig,
    /// Whether the sandbox is active for this turn.
    sandbox_on: bool,
    /// Optional cascade group handle (pgid or cgroup path) from a parent
    /// spine.
    cascade_group_arg: Option<String>,
    /// Deny list.
    deny: Vec<String>,
    /// Per-turn iteration cap (`--max-iter`).
    max_iter: Option<usize>,
    /// Live per-turn cancellation token, for parity with
    /// `turn/brain.rs::BrainDriver`. This driver only ever runs via
    /// `brain_launch()`'s spawned `gait __brain` subprocess, so today this
    /// token has no external owner to cancel it — kept `Some(..)` so a future
    /// in-process caller of this driver doesn't silently lose cancellation.
    cancel: tokio_util::sync::CancellationToken,
    /// Optional editor-buffer delegate injected by a workflow-level consumer
    /// (e.g. a future ACP adapter). `None` for REPL/CLI/workflow nodes.
    fs_delegate: Option<Arc<dyn gait_core::embed::FsDelegate>>,
    /// Launcher-set override for the in-tree safety guard (design §7).
    allow_intree: bool,
    /// The real error from the most recent `run_loop` turn failure, captured
    /// by `on_turn_error` (which used to discard it). The literal-prompt
    /// branch prefers this over the generic "no TurnResult" fallback when
    /// present, so the actual failure reaches the caller instead of a
    /// contentless placeholder.
    last_error: Option<anyhow::Error>,
}

#[async_trait::async_trait(?Send)]
impl TurnDriver for SealedTurnDriver {
    fn brain_launch(&self, _session_id: &str) -> BrainLaunch {
        // `GAIT_BRAIN_EXE` overrides the brain binary — the seam that lets an
        // integration test point the spine at `mock_brain` instead of a real
        // `gait __brain`. Unset in production: both supported production
        // binaries — `gait` and `gait-acp` — answer to the sealed role
        // tokens `is_role_argv` checks for, so `current_exe() __brain ...` is
        // a valid self-spawn from either one. Any other embedding binary must
        // supply `GAIT_BRAIN_EXE` unless it also implements the same
        // sealed-role dispatch.
        let gait_bin = std::env::var_os("GAIT_BRAIN_EXE")
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::current_exe().unwrap_or_else(|_| PathBuf::from("gait")));
        let mut bl = BrainLaunch::new(
            self.codebase.clone(),
            gait_bin,
            self.sandbox_cfg.clone(),
            self.sandbox_on,
            self.auto,
            self.max_iter,
            self.cascade_group_arg.clone(),
            self.agent_name.clone(),
            Some(self.inference.clone()),
            self.explicit_inference_override,
            self.allow.clone(),
            self.deny.clone(),
            self.result_schema.clone(),
        );
        bl.fs_delegate = self.fs_delegate.clone();
        bl.allow_intree = self.allow_intree;
        bl
    }

    fn turn_config(&mut self) -> LoopConfig {
        LoopConfig {
            auto: self.auto,
            max_iter: self.max_iter,
            preview_policy: gait_core::config::PreviewPolicy::default(),
            cancel: Some(self.cancel.clone()),
            sandbox_on: self.sandbox_on,
            sandbox_cfg: self.sandbox_cfg.clone(),
            relay: None,
            initial_git: None,
            daemons: vec![],
        }
    }

    /// A turn error surfaces to the caller through `last_error`, not as a
    /// loop break — `Continue` lets `run_loop`'s one-shot source exhaust
    /// normally so the loop returns `Ok(())`. That means the error does NOT
    /// reach the caller via `run_loop`'s `anyhow::Result` (there's nowhere
    /// left for `?` to propagate it through once the loop itself returns
    /// `Ok`) — it used to just vanish here. We stash it on `self` and
    /// surface it on the sink as a diagnostic so it's visible in the stream
    /// immediately, and the caller prefers it over the generic "no
    /// TurnResult" fallback once `run_loop` returns.
    fn on_turn_error(
        &mut self,
        sink: &mut dyn TurnSink,
        e: anyhow::Error,
    ) -> std::ops::ControlFlow<()> {
        sink.diagnostic(Diag::Error, &format!("turn failed: {e}"));
        self.last_error = Some(e);
        std::ops::ControlFlow::Continue(())
    }
}

/// Forwards every `TurnSink` call straight through to a caller-owned sink,
/// capturing the final `RunResult` so `run_turn_source`/`run_single_prompt`
/// can build a [`TurnRunOutcome`]. Unlike `gait-workflow`'s `CapturingSink`
/// (which always forwards over a `SinkFrame` channel to a workflow drain
/// loop), this never touches a channel — CLI/REPL/ACP callers hand their own
/// sink directly, with no workflow drain loop to feed.
struct DirectSink<'a> {
    inner: &'a mut dyn TurnSink,
    result: Option<RunResult>,
}

impl<'a> DirectSink<'a> {
    fn new(inner: &'a mut dyn TurnSink) -> Self {
        Self {
            inner,
            result: None,
        }
    }
}

impl TurnSink for DirectSink<'_> {
    fn begin_turn(&mut self) {
        self.inner.begin_turn();
    }
    fn item(&mut self, item: &StreamItem) {
        self.inner.item(item);
    }
    fn end_turn(&mut self) {
        self.inner.end_turn();
    }
    fn result(&mut self, r: &RunResult) {
        self.result = Some(r.clone());
        self.inner.result(r);
    }
    fn session_id(&mut self, id: &str) {
        self.inner.session_id(id);
    }
    fn diagnostic(&mut self, level: Diag, msg: &str) {
        self.inner.diagnostic(level, msg);
    }
}

/// Single-turn source: yields exactly one `Input([user_turn, context_status])` then `None`.
/// Moved unchanged from `gait-workflow::exec_prompt::OneTurnSource`.
pub struct OneTurnSource {
    pub session_updates: Option<Vec<gait_core::proto::gait::log::v1::event::Body>>,
    pub bodies: Option<Vec<gait_core::proto::gait::log::v1::event::Body>>,
}

#[async_trait::async_trait(?Send)]
impl TurnSource for OneTurnSource {
    async fn next(&mut self) -> Option<TurnStart> {
        if let Some(updates) = self.session_updates.take().filter(|v| !v.is_empty()) {
            return Some(TurnStart::SessionUpdate(updates));
        }
        self.bodies.take().map(|bodies| TurnStart::Input {
            bodies: bodies.into_iter().map(|body| (body, None)).collect(),
        })
    }
}

/// Wrapper enum so the orchestrator can pass a by-value `impl TurnSource` to
/// `run_loop`. `Box<dyn TurnSource>` does NOT itself satisfy `impl
/// TurnSource` (async_trait adds no blanket impl), so this enum bridges the
/// gap: calling `.next()` on a `Box<dyn TurnSource>` works as a method call
/// through the trait object. Moved from `gait-workflow::exec_prompt::NodeSource`
/// and renamed — it is no longer workflow-node-specific.
pub enum EitherSource {
    Literal(OneTurnSource),
    Live(Box<dyn TurnSource>),
}

#[async_trait::async_trait(?Send)]
impl TurnSource for EitherSource {
    async fn next(&mut self) -> Option<TurnStart> {
        match self {
            EitherSource::Literal(s) => s.next().await,
            EitherSource::Live(b) => b.next().await,
        }
    }
}

/// CLI/REPL/ACP/workflow-supplied overrides layered on top of this crate's
/// own disk-config resolution (`gait.jsonc`). `None`/empty fields fall back
/// to the disk-resolved default; `gait-workflow`'s own caller (Task 2 Step
/// 4) instead resolves every field itself first (its own
/// `ConfigResolver`/`Overrides` workflow+node layering) and passes the fully
/// resolved values through here, so for a real DAG node every field below is
/// effectively already final.
pub struct TurnProfileOverride {
    pub inference: Option<String>,
    /// Whether `inference` is an explicit caller override rather than a
    /// resolved workflow/config model passed across the driver boundary.
    pub explicit_inference_override: bool,
    /// Durable session-control events queued before the turn's first model
    /// turn. Core persists these through `SessionUpdate`; the caller never
    /// appends them directly.
    pub session_updates: Vec<gait_core::proto::gait::log::v1::event::Body>,
    pub allow: Vec<String>,
    pub deny: Vec<String>,
    pub agent: Option<String>,
    pub max_iter: Option<usize>,
    pub result_schema: Option<serde_json::Value>,
    /// `Some(b)` forces `auto` to `b`; `None` falls back to the runner's own
    /// default (`false`).
    pub auto: Option<bool>,
}

impl TurnProfileOverride {
    /// Resolve the effective `auto` flag. `base` is the caller's own default
    /// (always `false` for a bare `TurnRunOpts` caller; `gait-workflow`
    /// folds its own node/workflow default into `auto` before constructing
    /// the override, so `base` here only matters when `ov` is absent or its
    /// `auto` field is `None`).
    fn resolve_auto(base: bool, ov: Option<&TurnProfileOverride>) -> bool {
        ov.and_then(|p| p.auto).unwrap_or(base)
    }
}

/// Options for one sealed turn, independent of any workflow concept.
pub struct TurnRunOpts {
    /// Run identity. REPL passes `chat-<session>`; CLI mints via
    /// `convert::mint_run_id`.
    pub run_id: String,
    pub session_id: String,
    /// `""` = fresh session; else attach (spec-04 semantics) — carried
    /// through unchanged from `SinglePromptOpts::continue_session`.
    pub continue_session: String,
    pub gait_dir: PathBuf,
    pub codebase: PathBuf,
    pub args: Vec<String>,
    pub sandbox_on: Option<bool>,
    pub cascade_group_arg: Option<String>,
    pub cascade: Option<gait_core::embed::CascadeKillHandle>,
    pub profile: Option<TurnProfileOverride>,
    pub fs_delegate: Option<Arc<dyn gait_core::embed::FsDelegate>>,
    pub allow_intree: bool,
}

/// The outcome of one sealed turn — the runner's own result type, never a
/// `WorkflowOutcome` or `NodeOutput`.
pub struct TurnRunOutcome {
    pub completed: bool,
    pub opened: bool,
    pub text: Option<String>,
    pub json: Option<serde_json::Value>,
    pub exit_code: Option<i32>,
    pub run_result: Option<RunResult>,
}

/// Map a raw `RunResult` to the runner's own `TurnRunOutcome`. Same
/// `RunResult` → text/json mapping `gait-workflow`'s `map_turn_result` has
/// today, but returning `TurnRunOutcome` instead of `NodeOutput`.
fn run_result_to_outcome(run_result: RunResult) -> TurnRunOutcome {
    use gait_core::proto::gait::log::v1::{turn_done, turn_result, PauseReason};
    let outcome = run_result
        .turns
        .last()
        .and_then(|turn| turn.outcome.as_ref());
    let (text, json, exit_code, opened): (Option<String>, Option<serde_json::Value>, Option<i32>, bool) =
        match outcome {
            Some(turn_result::Outcome::Done(done)) => match done.output.as_ref() {
                Some(turn_done::Output::Text(text)) => (Some(text.message.clone()), None, None, false),
                Some(turn_done::Output::Structured(output)) => (
                    Some(output.message.clone()),
                    output
                        .result
                        .as_ref()
                        .and_then(|value| serde_json::to_value(value).ok()),
                    None,
                    false,
                ),
                None => (None, None, None, true),
            },
            Some(turn_result::Outcome::Pause(pause))
                if pause.reason == PauseReason::Permissions as i32 =>
            {
                (Some("(permissions required)".into()), None, None, false)
            }
            Some(turn_result::Outcome::Pause(pause))
                if pause.reason == PauseReason::Clarify as i32 =>
            {
                (Some("(paused for clarification)".into()), None, None, false)
            }
            Some(turn_result::Outcome::Pause(pause))
                if pause.reason == PauseReason::Inform as i32 =>
            {
                (Some("(paused to inform user)".into()), None, None, false)
            }
            Some(turn_result::Outcome::Pause(_)) => {
                (Some("(paused for sandbox unlock)".into()), None, None, false)
            }
            Some(turn_result::Outcome::Cancelled(_)) => {
                (Some("(turn cancelled by user)".into()), None, None, false)
            }
            Some(turn_result::Outcome::Failed(failed)) => {
                (Some(failed.message.clone()), None, Some(1), false)
            }
            None => (None, None, None, true),
        };
    TurnRunOutcome {
        completed: !opened,
        opened,
        text,
        json,
        exit_code,
        run_result: Some(run_result),
    }
}

/// Shared body for `run_turn_source`/`run_single_prompt`: resolve the
/// effective profile (disk config + `opts.profile` override, same layering
/// `gait-workflow::exec_prompt_node` applies today), build the
/// `SealedTurnDriver`, open the session, and drive `source` through
/// `embed::run_loop`.
async fn run_sealed_turn(
    opts: TurnRunOpts,
    source: EitherSource,
    sink: &mut dyn TurnSink,
) -> anyhow::Result<TurnRunOutcome> {
    let TurnRunOpts {
        run_id: _run_id,
        session_id,
        continue_session: _continue_session,
        gait_dir,
        codebase,
        args: _args,
        sandbox_on,
        cascade_group_arg,
        cascade,
        profile,
        fs_delegate,
        allow_intree,
    } = opts;

    let loaded = gait_core::config::load_config(&gait_dir);
    let resolved_cfg = gait_core::config::ConfigResolver::new(loaded.clone()).resolve_checked()?;
    let base_inference = resolved_cfg
        .inference
        .clone()
        .expect("resolve_checked guarantees inference is Some");
    let base_allow = resolved_cfg.allow.clone();
    let sandbox_cfg = loaded.sandbox;

    let ov = profile.as_ref();
    let inference = ov.and_then(|p| p.inference.clone()).unwrap_or(base_inference);
    let explicit_inference_override = ov.map(|p| p.explicit_inference_override).unwrap_or(false);
    let allow = ov
        .filter(|p| !p.allow.is_empty())
        .map(|p| p.allow.clone())
        .unwrap_or(base_allow);
    let deny = ov.map(|p| p.deny.clone()).unwrap_or_default();
    let agent_name = ov.and_then(|p| p.agent.clone());
    let max_iter = ov.and_then(|p| p.max_iter);
    let result_schema = ov.and_then(|p| p.result_schema.clone());
    let auto = TurnProfileOverride::resolve_auto(false, ov);
    let sandbox_on = sandbox_on.unwrap_or(true);

    let session = Arc::new(Session::open(
        gait_dir.clone(),
        codebase.clone(),
        session_id.clone(),
    )?);

    let mut driver = SealedTurnDriver {
        codebase,
        inference,
        explicit_inference_override,
        allow,
        agent_name,
        result_schema,
        auto,
        sandbox_cfg,
        sandbox_on,
        cascade_group_arg,
        deny,
        max_iter,
        cancel: tokio_util::sync::CancellationToken::new(),
        fs_delegate,
        allow_intree,
        last_error: None,
    };

    let mut direct = DirectSink::new(sink);
    embed::run_loop(
        source,
        &mut direct,
        session,
        &gait_dir,
        &session_id,
        &mut driver,
        cascade.as_ref(),
    )
    .await?;

    // `driver.last_error` is set by `on_turn_error` whenever `run_loop`
    // swallowed a real turn failure to let its one-shot source exhaust
    // normally (see that fn's doc comment) — prefer it over the generic
    // fallback so the actual cause reaches the caller instead of a
    // contentless "no TurnResult" message.
    let turn_result = direct.result.take().ok_or_else(|| {
        driver
            .last_error
            .take()
            .unwrap_or_else(|| anyhow::anyhow!("run_loop returned without a TurnResult"))
    })?;
    Ok(run_result_to_outcome(turn_result))
}

/// Drive an already-live `TurnSource` (interactive/open case: ACP's
/// permission/resume protocol, a REPL's stdin-driven source, etc.) through
/// one or more sealed turns. Returns `TurnRunOutcome { opened: true, .. }`
/// when the source is live but produced no terminal turn this call, exactly
/// as `gait-workflow::exec_prompt_node`'s absent-prompt branch does today.
pub async fn run_turn_source(
    opts: TurnRunOpts,
    source: Box<dyn TurnSource>,
    sink: &mut dyn TurnSink,
) -> anyhow::Result<TurnRunOutcome> {
    run_sealed_turn(opts, EitherSource::Live(source), sink).await
}

/// Drive one literal one-shot prompt through a sealed turn. Exists as a
/// distinct entry point from `run_turn_source` purely so CLI-shaped callers
/// don't have to construct a one-shot `TurnSource` themselves — it is always
/// called with `prompt: Some(_)` in practice (a bare positional `gait
/// "task"` always has task text). The interactive/open case is exclusively
/// `run_turn_source`'s.
pub async fn run_single_prompt(
    opts: TurnRunOpts,
    prompt: Option<String>,
    sink: &mut dyn TurnSink,
) -> anyhow::Result<TurnRunOutcome> {
    let session_updates = opts
        .profile
        .as_ref()
        .map(|p| p.session_updates.clone())
        .unwrap_or_default();
    let repos = embed::Repos::production();
    let task = prompt.unwrap_or_default();
    let user_bodies = embed::user_turn_events(&repos, &opts.codebase, task);
    let source = OneTurnSource {
        session_updates: Some(session_updates),
        bodies: Some(user_bodies),
    };
    run_sealed_turn(opts, EitherSource::Literal(source), sink).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    // Regression for the swallowed-error bug: `on_turn_error` used to discard
    // its `anyhow::Error` entirely, so a real turn failure surfaced only as
    // the generic, contentless "run_loop returned without a TurnResult"
    // fallback. It must now (a) stash the error on the driver so that
    // fallback can recover the real cause, and (b) push it to the sink as a
    // diagnostic so it's visible in the stream immediately, not just after
    // the fact.
    #[test]
    fn on_turn_error_captures_the_real_error_and_diagnoses_it() {
        struct CapturingDiagSink {
            diagnostics: Vec<(Diag, String)>,
        }
        impl TurnSink for CapturingDiagSink {
            fn item(&mut self, _item: &StreamItem) {}
            fn result(&mut self, _r: &RunResult) {}
            fn session_id(&mut self, _id: &str) {}
            fn diagnostic(&mut self, level: Diag, msg: &str) {
                self.diagnostics.push((level, msg.to_string()));
            }
        }

        let mut driver = SealedTurnDriver {
            codebase: PathBuf::from("/tmp/cb"),
            inference: "test".into(),
            explicit_inference_override: false,
            allow: vec![],
            agent_name: None,
            result_schema: None,
            auto: false,
            sandbox_cfg: gait_core::config::SandboxConfig::default(),
            sandbox_on: true,
            cascade_group_arg: None,
            deny: vec![],
            max_iter: None,
            cancel: tokio_util::sync::CancellationToken::new(),
            fs_delegate: None,
            allow_intree: false,
            last_error: None,
        };
        let mut sink = CapturingDiagSink {
            diagnostics: vec![],
        };

        let flow = driver.on_turn_error(&mut sink, anyhow::anyhow!("boom: the real cause"));

        assert_eq!(
            flow,
            std::ops::ControlFlow::Continue(()),
            "must still let the one-shot source exhaust normally"
        );
        let captured = driver
            .last_error
            .expect("on_turn_error must stash the real error on the driver");
        assert!(captured.to_string().contains("boom: the real cause"));
        assert!(
            sink.diagnostics
                .iter()
                .any(|(lvl, msg)| *lvl == Diag::Error && msg.contains("boom: the real cause")),
            "the real error must reach the sink as a diagnostic, not vanish silently: {:?}",
            sink.diagnostics
        );
    }

    #[test]
    fn sealed_turn_driver_holds_paths() {
        let driver = SealedTurnDriver {
            codebase: PathBuf::from("/tmp/cb"),
            inference: "test".into(),
            explicit_inference_override: false,
            allow: vec!["run_workflow".into()],
            agent_name: None,
            result_schema: None,
            auto: false,
            sandbox_cfg: gait_core::config::SandboxConfig::default(),
            sandbox_on: true,
            cascade_group_arg: None,
            deny: vec![],
            max_iter: None,
            cancel: tokio_util::sync::CancellationToken::new(),
            fs_delegate: None,
            allow_intree: false,
            last_error: None,
        };
        // Verify the driver holds the paths used to construct BrainLaunch.
        assert_eq!(driver.codebase, PathBuf::from("/tmp/cb"));
    }

    #[tokio::test]
    async fn brain_launch_carries_injected_fs_delegate() {
        use std::path::Path;
        use std::sync::Arc;

        struct Fake;
        #[async_trait::async_trait]
        impl gait_core::embed::FsDelegate for Fake {
            async fn read_text_file(&self, _p: &Path) -> anyhow::Result<Option<String>> {
                Ok(Some("BUFFER".into()))
            }
            async fn write_text_file(&self, _p: &Path, _c: &str) -> anyhow::Result<()> {
                Ok(())
            }
        }

        let driver = SealedTurnDriver {
            codebase: std::path::PathBuf::from("/tmp/x"),
            inference: "test".into(),
            explicit_inference_override: false,
            allow: vec![],
            agent_name: None,
            result_schema: None,
            auto: false,
            sandbox_cfg: gait_core::config::SandboxConfig::default(),
            sandbox_on: true,
            cascade_group_arg: None,
            deny: vec![],
            max_iter: None,
            cancel: tokio_util::sync::CancellationToken::new(),
            fs_delegate: Some(Arc::new(Fake) as Arc<dyn gait_core::embed::FsDelegate>),
            allow_intree: false,
            last_error: None,
        };
        let bl = driver.brain_launch("sess-x");
        assert!(
            bl.fs_delegate.is_some(),
            "brain_launch must carry the injected delegate"
        );
    }

    #[test]
    fn cli_auto_override_wins_over_node_default() {
        let on = TurnProfileOverride {
            inference: None,
            explicit_inference_override: false,
            session_updates: vec![],
            allow: vec![],
            deny: vec![],
            agent: None,
            max_iter: None,
            result_schema: None,
            auto: Some(true),
        };
        let off = TurnProfileOverride {
            inference: None,
            explicit_inference_override: false,
            session_updates: vec![],
            allow: vec![],
            deny: vec![],
            agent: None,
            max_iter: None,
            result_schema: None,
            auto: None,
        };
        // base = false — mirrors the degenerate CLI node's node/wf default.
        assert!(
            TurnProfileOverride::resolve_auto(false, Some(&on)),
            "--auto must force auto on"
        );
        assert!(
            !TurnProfileOverride::resolve_auto(false, Some(&off)),
            "absent override falls back to node/wf default"
        );
        assert!(
            !TurnProfileOverride::resolve_auto(false, None),
            "no profile → node/wf default"
        );
        assert!(
            TurnProfileOverride::resolve_auto(true, Some(&off)),
            "absent override preserves a true base"
        );
    }
}

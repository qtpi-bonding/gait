use std::path::PathBuf;
use std::sync::Arc;

use gait_core::embed::{
    self, BrainLaunch, Diag, LoopConfig, SinkFrame, TurnDriver, TurnSink, TurnSource, TurnStart,
};
use gait_core::proto::gait::log::v1::{RunResult, StreamItem};
use gait_core::proto::gait::workflow::v1::{Node, PromptNode, Workflow};
use gait_core::Session;

use crate::exec_shell::NodeContext;
use crate::vars::{substitute, NodeOutput};

/// Per-node driver: supplies `brain_launch()` so `run_loop` can spawn `gait __brain`
/// for each prompt node. Workflow nodes are now fully sealed (Task 2).
struct PromptNodeDriver {
    codebase: PathBuf,
    /// Named inference selected for this prompt node.
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
    /// Whether the sandbox is active for this turn. Nodes default `true`
    /// (workflow nodes are always sandboxed); the degenerate CLI node
    /// overrides this from the CLI's `--sandbox`/`--no-sandbox`/config
    /// resolution (spec-06 §3 Task 3.1/3.2).
    sandbox_on: bool,
    /// Optional cascade group handle (pgid or cgroup path) from a parent
    /// spine — `None` for workflow nodes, `Some` when the degenerate CLI node
    /// enrolls a spawned brain into the CLI's own cascade group.
    cascade_group_arg: Option<String>,
    /// Deny list — empty for workflow nodes; the degenerate CLI node forwards
    /// `--deny`.
    deny: Vec<String>,
    /// Per-turn iteration cap (`--max-iter`) — `None` for workflow nodes.
    max_iter: Option<usize>,
    /// Live per-turn cancellation token (Task: wire turn cancellation), for
    /// parity with `turn/brain.rs::BrainDriver`. This driver only ever runs
    /// via `brain_launch()`'s spawned `gait __brain` subprocess (a real turn
    /// never executes through `turn_config()`'s `LoopConfig` in-process here),
    /// so today this token has no external owner to cancel it — the live
    /// cancellation path is the brain's own token, cancelled by the spine's
    /// teardown SIGTERM. Kept `Some(..)` (not `None`) so a future in-process
    /// caller of this driver doesn't silently lose cancellation.
    cancel: tokio_util::sync::CancellationToken,
    /// Optional editor-buffer delegate injected by a workflow-level consumer
    /// (e.g. a future ACP adapter). `None` for REPL/CLI/workflow nodes.
    fs_delegate: Option<std::sync::Arc<dyn gait_core::embed::FsDelegate>>,
    /// Launcher-set override for the in-tree safety guard (design §7), sourced
    /// from `--allow-intree`. `false` for REPL/ACP; the degenerate CLI node
    /// forwards the real flag value.
    allow_intree: bool,
    /// The real error from the most recent `run_loop` turn failure, captured by
    /// `on_turn_error` (which used to discard it). `run_prompt_node`'s literal-
    /// prompt branch prefers this over the generic "no TurnResult" fallback when
    /// present, so the actual failure reaches the caller instead of a
    /// contentless placeholder.
    last_error: Option<anyhow::Error>,
}

#[async_trait::async_trait(?Send)]
impl TurnDriver for PromptNodeDriver {
    fn brain_launch(&self, _session_id: &str) -> BrainLaunch {
        // `GAIT_BRAIN_EXE` overrides the brain binary — the seam that lets an
        // integration test point the spine at `mock_brain` instead of a real
        // `gait __brain` (the same trick `tests/common` uses for `run_loop`,
        // now reachable through the workflow path). Unset in production, where
        // the current executable is the gait binary itself.
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

    /// A prompt-node turn error surfaces as a node error, not as a loop break —
    /// `Continue` lets `run_loop`'s one-shot source exhaust normally so the loop
    /// returns `Ok(())`. That means the error does NOT reach the caller via
    /// `run_loop`'s `anyhow::Result` (there's nowhere left for `?` to propagate
    /// it through once the loop itself returns `Ok`) — it used to just vanish
    /// here. We stash it on `self` and surface it on the sink as a diagnostic so
    /// it's visible in the stream immediately, and `run_prompt_node` prefers it
    /// over the generic "no TurnResult" fallback once `run_loop` returns.
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

/// Capturing-and-forwarding sink: forwards every `TurnSink` call to the
/// workflow's `SinkFrame` channel (spec-06 §4 — begin_turn/item/end_turn/
/// result/session_id/diagnostic all round-trip, not just `item`), and
/// captures the final `TurnResult` so the caller can return it as a
/// `NodeOutput`.
struct CapturingSink {
    /// Borrowed workflow channel sender. Forwards every call we receive so the
    /// workflow's drain consumer sees the sub-turn's full output, verbatim.
    workflow_tx: tokio::sync::mpsc::UnboundedSender<SinkFrame>,
    result: Option<RunResult>,
    failed: bool,
}

impl CapturingSink {
    fn new(workflow_tx: tokio::sync::mpsc::UnboundedSender<SinkFrame>) -> Self {
        Self {
            workflow_tx,
            result: None,
            failed: false,
        }
    }
}

impl TurnSink for CapturingSink {
    fn begin_turn(&mut self) {
        let _ = self.workflow_tx.send(SinkFrame::BeginTurn);
    }
    fn item(&mut self, item: &StreamItem) {
        // Forward to the workflow's channel. `send` is sync + non-blocking;
        // if the receiver is gone the workflow is already torn down — ignore.
        let _ = self
            .workflow_tx
            .send(SinkFrame::Item(Box::new(item.clone())));
    }
    fn end_turn(&mut self) {
        let _ = self.workflow_tx.send(SinkFrame::EndTurn);
    }
    fn result(&mut self, r: &RunResult) {
        self.result = Some(r.clone());
        let _ = self
            .workflow_tx
            .send(SinkFrame::Result(Box::new(r.clone())));
    }
    fn session_id(&mut self, id: &str) {
        let _ = self.workflow_tx.send(SinkFrame::SessionId(id.to_string()));
    }
    fn diagnostic(&mut self, level: Diag, msg: &str) {
        if level == Diag::Error {
            self.failed = true;
        }
        let _ = self
            .workflow_tx
            .send(SinkFrame::Diagnostic(level, msg.to_string()));
    }
}

/// Single-turn source: yields exactly one `Input([user_turn, context_status])` then `None`.
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
            bodies,
            provenance: None,
        })
    }
}

/// Wrapper enum so the orchestrator can pass a by-value `impl TurnSource` to
/// `run_loop`.  `Box<dyn TurnSource>` does NOT itself satisfy `impl TurnSource`
/// (async_trait adds no blanket impl), so this enum bridges the gap: calling
/// `.next()` on a `Box<dyn TurnSource>` works as a method call through the
/// trait object.
pub enum NodeSource {
    Literal(OneTurnSource),
    Live(Box<dyn TurnSource>),
}

#[async_trait::async_trait(?Send)]
impl TurnSource for NodeSource {
    async fn next(&mut self) -> Option<TurnStart> {
        match self {
            NodeSource::Literal(s) => s.next().await,
            NodeSource::Live(b) => b.next().await,
        }
    }
}

/// Resolve the effective `auto` flag for a node. `base` is the node/workflow
/// default (i.e. `node.auto.unwrap_or(wf.auto)`); a `ProfileOverride.auto` of
/// `Some(b)` forces `b`, `None` falls back to `base`. The CLI sets
/// `Some(true)` when `--auto` is passed on a headless run; workflow nodes
/// carry no `ProfileOverride` and the base always wins.
fn resolve_auto(base: bool, ov: Option<&crate::single_prompt::ProfileOverride>) -> bool {
    ov.and_then(|p| p.auto).unwrap_or(base)
}

/// Drive a single prompt node through the unified `run_loop` boundary.
///
/// If the node has a literal `prompt` field it is used directly (one-shot),
/// ignoring `input`. If the node has no prompt (interactive) and `input` is
/// `Some`, it is driven off that live source. `input: None` on an absent-prompt
/// node returns `NodeOutput::open()` immediately (the headless/no-input case;
/// the runner's `OpenResumption::SuspendAndExit` never reaches this function at
/// all — see `runner::handle_open_node`).
///
/// One output path (spec-06 §4): every branch renders through
/// `ctx.workflow_tx`'s `SinkFrame` channel via `CapturingSink` — there is no
/// direct-sink bypass. `handle_open_node`'s caller sink sees the interactive
/// node's output via that channel's drain, same as every automated node.
pub async fn exec_prompt_node(
    node: &Node,
    p: &PromptNode,
    ctx: &NodeContext<'_>,
    wf: &Workflow,
    input: Option<Box<dyn TurnSource>>,
) -> anyhow::Result<NodeOutput> {
    use gait_core::config::resolver::{ConfigResolver, Overrides};
    let loaded = gait_core::config::load_config(ctx.gait_dir);
    // consumer layer: Workflow fields
    let consumer = Overrides {
        inference: wf.inference.clone(),
        allow: Some(wf.allow.clone()),
        ..Overrides::empty()
    };
    // per-call layer: Node fields (higher priority)
    let per_call = Overrides {
        inference: node.inference.clone(),
        allow: if node.allow.is_empty() {
            None
        } else {
            Some(node.allow.clone())
        },
        agent: node.agent.clone(),
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
    let auto = node.auto.unwrap_or(wf.auto);

    // output_schema_json: parse non-empty string to Value
    let result_schema: Option<serde_json::Value> = if node.output_schema_json.is_empty() {
        None
    } else {
        Some(serde_json::from_str(&node.output_schema_json).map_err(|e| {
            anyhow::anyhow!("node '{}': invalid output_schema_json: {}", node.id, e)
        })?)
    };

    let codebase = p
        .codebase
        .as_deref()
        .map(|cb| PathBuf::from(substitute(cb, &ctx.vars)))
        .unwrap_or_else(|| ctx.codebase.to_path_buf());

    let session_id = ctx.node_session.to_string();

    // Open the session up-front so `run_loop` can use `session.repos` for
    // worktree creation.
    let session = Arc::new(Session::open(
        ctx.gait_dir.to_path_buf(),
        codebase.clone(),
        session_id.clone(),
    )?);

    let sandbox_cfg = {
        let cfg = gait_core::config::load_config(ctx.gait_dir);
        cfg.sandbox
    };

    // spec-06 §3 Task 3.2 (the C1 fix): a CLI-only override, when present,
    // takes priority over the disk-resolved config above — this is what makes
    // `gait run --model/--allow/--deny/--agent/--max-iter/--result-schema`
    // still work now that `cmd_run` is routed through this same function
    // instead of building its own driver directly. Every real workflow node
    // has `ctx.profile == None` and is unaffected.
    let ov = ctx.profile;
    let session_updates = ov.map(|p| p.session_updates.clone()).unwrap_or_default();
    let inference = ov.and_then(|p| p.inference.clone()).unwrap_or(inference);
    let explicit_inference_override = ov.map(|p| p.explicit_inference_override).unwrap_or(false);
    let allow = ov
        .filter(|p| !p.allow.is_empty())
        .map(|p| p.allow.clone())
        .unwrap_or(allow);
    let deny = ov.map(|p| p.deny.clone()).unwrap_or_default();
    let agent_name = ov
        .and_then(|p| p.agent.clone())
        .or_else(|| node.agent.clone());
    let max_iter = ov.and_then(|p| p.max_iter);
    let result_schema = ov.and_then(|p| p.result_schema.clone()).or(result_schema);
    let auto = resolve_auto(auto, ov);
    let sandbox_on = ctx.sandbox_on.unwrap_or(true); // workflow nodes default sandboxed

    let mut driver = PromptNodeDriver {
        codebase: codebase.clone(),
        inference: inference.clone(),
        explicit_inference_override,
        allow: allow.clone(),
        agent_name,
        result_schema,
        auto,
        sandbox_cfg,
        sandbox_on,
        cascade_group_arg: ctx.cascade_group_arg.clone(),
        deny,
        max_iter,
        cancel: tokio_util::sync::CancellationToken::new(),
        fs_delegate: ctx.fs_delegate.clone(),
        allow_intree: ctx.allow_intree,
        last_error: None,
    };

    if !p.prompt.is_empty() || input.is_some() {
        // A caller may provide a live source even for a prompt-bearing node.
        // ACP uses this to keep its permission/resume protocol in the source;
        // ordinary CLI/workflow callers have no source and use the literal
        // one-shot source below.
        let source = if let Some(live) = input {
            NodeSource::Live(live)
        } else {
            let task = substitute(&p.prompt, &ctx.vars);
            let repos = gait_core::git::Repos::production();
            let user_bodies = embed::user_turn_events(&repos, &codebase, task);
            NodeSource::Literal(OneTurnSource {
                session_updates: Some(session_updates),
                bodies: Some(user_bodies),
            })
        };

        let mut sink = CapturingSink::new(ctx.workflow_tx.clone());
        embed::run_loop(
            source,
            &mut sink,
            session,
            ctx.gait_dir,
            &session_id,
            &mut driver,
            ctx.cascade,
        )
        .await?;

        // Map sink.result -> NodeOutput. `driver.last_error` is set by
        // `on_turn_error` whenever `run_loop` swallowed a real turn failure to
        // let its one-shot source exhaust normally (see that fn's doc comment)
        // — prefer it over the generic fallback so the actual cause reaches the
        // caller instead of a contentless "no TurnResult" message.
        let turn_result = sink.result.ok_or_else(|| {
            driver
                .last_error
                .take()
                .unwrap_or_else(|| anyhow::anyhow!("run_loop returned without a TurnResult"))
        })?;
        Ok(map_turn_result(turn_result, &session_id))
    } else {
        // Absent prompt: interactive node. `input: None` means no live source
        // was supplied (headless/no-input) — return open immediately without
        // driving anything.
        let Some(live) = input else {
            return Ok(NodeOutput::open());
        };
        let source = NodeSource::Live(live);
        let mut sink = CapturingSink::new(ctx.workflow_tx.clone());
        embed::run_loop(
            source,
            &mut sink,
            session,
            ctx.gait_dir,
            &session_id,
            &mut driver,
            ctx.cascade,
        )
        .await?;

        // Live node that returned (source exhausted) stays OPEN — no terminal
        // advance has been signalled. The run exits cleanly; resume re-enters.
        Ok(NodeOutput::open())
    }
}

fn map_turn_result(run_result: RunResult, session_id: &str) -> NodeOutput {
    use gait_core::proto::gait::log::v1::{turn_done, turn_result, PauseReason};
    let outcome = run_result
        .turns
        .last()
        .and_then(|turn| turn.outcome.as_ref());
    match outcome {
        Some(turn_result::Outcome::Done(done)) => match done.output.as_ref() {
            Some(turn_done::Output::Text(text)) => NodeOutput {
                text: text.message.clone(),
                exit_code: None,
                json: None,
                routed_to: None,
                session_id: session_id.to_string(),
            },
            Some(turn_done::Output::Structured(output)) => NodeOutput {
                text: output.message.clone(),
                exit_code: None,
                json: output
                    .result
                    .as_ref()
                    .and_then(|value| serde_json::to_value(value).ok()),
                routed_to: None,
                session_id: session_id.to_string(),
            },
            None => NodeOutput::open(),
        },
        Some(turn_result::Outcome::Pause(pause))
            if pause.reason == PauseReason::Permissions as i32 =>
        {
            NodeOutput {
                text: "(permissions required)".into(),
                exit_code: None,
                json: None,
                routed_to: None,
                session_id: session_id.to_string(),
            }
        }
        Some(turn_result::Outcome::Pause(pause)) if pause.reason == PauseReason::Clarify as i32 => {
            NodeOutput {
                text: "(paused for clarification)".into(),
                exit_code: None,
                json: None,
                routed_to: None,
                session_id: session_id.to_string(),
            }
        }
        Some(turn_result::Outcome::Pause(pause)) if pause.reason == PauseReason::Inform as i32 => {
            NodeOutput {
                text: "(paused to inform user)".into(),
                exit_code: None,
                json: None,
                routed_to: None,
                session_id: session_id.to_string(),
            }
        }
        Some(turn_result::Outcome::Pause(_)) => NodeOutput {
            text: "(paused for sandbox unlock)".into(),
            exit_code: None,
            json: None,
            routed_to: None,
            session_id: session_id.to_string(),
        },
        Some(turn_result::Outcome::Cancelled(_)) => NodeOutput {
            text: "(turn cancelled by user)".into(),
            exit_code: None,
            json: None,
            routed_to: None,
            session_id: session_id.to_string(),
        },
        Some(turn_result::Outcome::Failed(failed)) => NodeOutput {
            text: failed.message.clone(),
            exit_code: Some(1),
            json: None,
            routed_to: None,
            session_id: session_id.to_string(),
        },
        None => NodeOutput::open(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    // Regression for the swallowed-error bug: `on_turn_error` used to discard
    // its `anyhow::Error` entirely (an unused `_e` param), so a real
    // `relay_brain_turn` failure surfaced only as the generic, contentless
    // "run_loop returned without a TurnResult" fallback in `run_prompt_node`.
    // It must now (a) stash the error on the driver so that fallback can
    // recover the real cause, and (b) push it to the sink as a diagnostic so
    // it's visible in the stream immediately, not just after the fact.
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

        let mut driver = PromptNodeDriver {
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
    fn prompt_node_driver_holds_paths() {
        let driver = PromptNodeDriver {
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

        let driver = PromptNodeDriver {
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
        use crate::single_prompt::ProfileOverride;
        let on = ProfileOverride {
            explicit_inference_override: false,
            auto: Some(true),
            ..Default::default()
        };
        let off = ProfileOverride::default(); // auto: None
                                              // base = node.auto.unwrap_or(wf.auto) — false for the degenerate CLI node
        assert!(resolve_auto(false, Some(&on)), "--auto must force auto on");
        assert!(
            !resolve_auto(false, Some(&off)),
            "absent override falls back to node/wf default"
        );
        assert!(!resolve_auto(false, None), "no profile → node/wf default");
        assert!(
            resolve_auto(true, Some(&off)),
            "absent override preserves a true base"
        );
    }

    #[tokio::test]
    async fn capturing_sink_forwards_every_call_over_the_channel() {
        use gait_core::embed::{Diag, SinkFrame, TurnSink};
        use gait_core::proto::gait::log::v1::{stream_item, StreamItem, TextDelta};

        #[derive(Default)]
        struct SeqRecorder(Vec<SinkFrame>);
        impl TurnSink for SeqRecorder {
            fn begin_turn(&mut self) {
                self.0.push(SinkFrame::BeginTurn);
            }
            fn item(&mut self, i: &StreamItem) {
                self.0.push(SinkFrame::Item(Box::new(i.clone())));
            }
            fn end_turn(&mut self) {
                self.0.push(SinkFrame::EndTurn);
            }
            fn result(&mut self, r: &RunResult) {
                self.0.push(SinkFrame::Result(Box::new(r.clone())));
            }
            fn session_id(&mut self, id: &str) {
                self.0.push(SinkFrame::SessionId(id.into()));
            }
            fn diagnostic(&mut self, l: Diag, m: &str) {
                self.0.push(SinkFrame::Diagnostic(l, m.into()));
            }
        }

        fn delta(s: &str) -> StreamItem {
            StreamItem {
                item: Some(stream_item::Item::Delta(TextDelta { content: s.into() })),
            }
        }

        let play = |sink: &mut dyn TurnSink| {
            sink.begin_turn();
            sink.session_id("s0000001");
            sink.item(&delta("hel"));
            sink.item(&delta("lo"));
            sink.diagnostic(Diag::Error, "warn");
            sink.result(&RunResult {
                run_id: "r1".into(),
                state: Some(gait_core::proto::gait::log::v1::run_result::State::Paused(
                    Default::default(),
                )),
                turns: vec![gait_core::proto::gait::log::v1::TurnResult {
                    turn_id: "t1".into(),
                    outcome: Some(
                        gait_core::proto::gait::log::v1::turn_result::Outcome::Pause(
                            Default::default(),
                        ),
                    ),
                    ..Default::default()
                }],
                ..Default::default()
            });
            sink.end_turn();
        };

        let mut baseline = SeqRecorder::default();
        play(&mut baseline);

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<SinkFrame>();
        {
            let mut cap = CapturingSink::new(tx);
            play(&mut cap);
        }
        let mut drained = SeqRecorder::default();
        while let Some(f) = rx.recv().await {
            f.replay(&mut drained);
        }

        assert_eq!(
            baseline.0.len(),
            drained.0.len(),
            "channel dropped {} of {} sink calls",
            baseline.0.len() - drained.0.len(),
            baseline.0.len()
        );
    }
}

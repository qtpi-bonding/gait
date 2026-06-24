use std::path::PathBuf;
use std::sync::Arc;

use gait_core::embed::{
    self, BrainLaunch, Diag, LoopConfig, TurnDriver, TurnSink, TurnSource, TurnStart,
};
use gait_core::proto::gait::log::v1::StreamItem;
use gait_core::proto::gait::workflow::v1::{Node, PromptNode, Workflow};
use gait_core::turn::TurnResult;
use gait_core::Session;

use crate::exec_shell::NodeContext;
use crate::vars::{substitute, NodeOutput};

/// Per-node driver: supplies `brain_launch()` so `run_loop` can spawn `gait __brain`
/// for each prompt node. Workflow nodes are now fully sealed (Task 2).
struct PromptNodeDriver {
    codebase: PathBuf,
    /// Resolved model string ("provider/model-id").
    model: String,
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
}

#[async_trait::async_trait(?Send)]
impl TurnDriver for PromptNodeDriver {
    fn brain_launch(&self, _session_id: &str) -> BrainLaunch {
        let api_key = gait_core::auth::resolve_api_key(
            self.model.split_once('/').map(|(p, _)| p).unwrap_or(""),
        )
        .unwrap_or_default();
        let gait_bin = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("gait"));
        BrainLaunch::new(
            self.codebase.clone(),
            api_key,
            gait_bin,
            self.sandbox_cfg.clone(),
            true, // workflow nodes are always sandboxed
            self.auto,
            None,
            None, // cascade_group_arg
            self.agent_name.clone(),
            Some(self.model.clone()),
            self.allow.clone(),
            vec![], // deny
            self.result_schema.clone(),
        )
    }

    fn turn_config(&mut self) -> LoopConfig {
        LoopConfig {
            auto: self.auto,
            max_iter: None,
            preview_policy: gait_core::config::PreviewPolicy::default(),
            cancel: None,
            sandbox_on: true,
            sandbox_cfg: self.sandbox_cfg.clone(),
            relay: None,
            initial_git: None,
            daemons: vec![],
        }
    }

    /// A prompt-node turn error surfaces as a node error, not as a loop break —
    /// return `Continue` so the loop propagates the error via `run_loop`'s
    /// `anyhow::Result`.
    fn on_turn_error(
        &mut self,
        _sink: &mut dyn TurnSink,
        _e: anyhow::Error,
    ) -> std::ops::ControlFlow<()> {
        std::ops::ControlFlow::Continue(())
    }
}

/// Capturing-and-forwarding sink: forwards every `StreamItem` to the
/// workflow's channel (preserving the Phase 3b contract that sub-turn deltas
/// reach the workflow's consumer), and captures the final `TurnResult` so the
/// caller can return it as a `NodeOutput`.
struct CapturingSink {
    /// Borrowed workflow channel sender. Forwards every item we receive so the
    /// workflow's `drive(rx, sink)` consumer sees the sub-turn's events.
    workflow_tx: tokio::sync::mpsc::UnboundedSender<StreamItem>,
    result: Option<TurnResult>,
    failed: bool,
}

impl CapturingSink {
    fn new(workflow_tx: tokio::sync::mpsc::UnboundedSender<StreamItem>) -> Self {
        Self {
            workflow_tx,
            result: None,
            failed: false,
        }
    }
}

impl TurnSink for CapturingSink {
    fn item(&mut self, item: &StreamItem) {
        // Forward to the workflow's channel. `send` is sync + non-blocking;
        // if the receiver is gone the workflow is already torn down — ignore.
        let _ = self.workflow_tx.send(item.clone());
    }
    fn result(&mut self, r: &TurnResult) {
        self.result = Some(r.clone());
    }
    fn session_id(&mut self, _id: &str) {}
    fn diagnostic(&mut self, level: Diag, _msg: &str) {
        if level == Diag::Error {
            self.failed = true;
        }
    }
}

/// Single-turn source: yields exactly one `Input([user_turn, context_status])` then `None`.
pub struct OneTurnSource {
    pub bodies: Option<Vec<gait_core::proto::gait::log::v1::event::Body>>,
}

#[async_trait::async_trait(?Send)]
impl TurnSource for OneTurnSource {
    async fn next(&mut self) -> Option<TurnStart> {
        self.bodies.take().map(TurnStart::Input)
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

/// Drive a single prompt node through the unified `run_loop` boundary.
///
/// `input` is `Some(live_source)` when the caller has a live interactive source
/// (e.g. stdin for `gait workflow run`). If the node has a literal `prompt`
/// field it is used directly (one-shot), ignoring `input`. If the node has no
/// prompt and `input` is `None`, returns `NodeOutput::open()` immediately —
/// the run exits cleanly and the node is left open for resume.
pub async fn exec_prompt_node(
    node: &Node,
    p: &PromptNode,
    ctx: &NodeContext<'_>,
    wf: &Workflow,
    input: Option<Box<dyn TurnSource>>,
    ui_sink: Option<&mut dyn TurnSink>,
) -> anyhow::Result<NodeOutput> {
    // Fast path: absent prompt + no input → open immediately, before any model
    // resolution or session setup (those require a valid model config).
    if p.prompt.is_empty() && input.is_none() {
        return Ok(NodeOutput::open());
    }

    use gait_core::config::resolver::{ConfigResolver, Overrides};
    let loaded = gait_core::config::load_config(ctx.gait_dir);
    // consumer layer: Workflow fields
    let consumer = Overrides {
        model: wf.model.clone(),
        allow: Some(wf.allow.clone()),
        ..Overrides::empty()
    };
    // per-call layer: Node fields (higher priority)
    let per_call = Overrides {
        model: node.model.clone(),
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
    let model = resolved_cfg
        .model
        .clone()
        .expect("resolve_checked guarantees model is Some");
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

    let session_id = if p.continue_session.is_empty() {
        ctx.session_id.to_string()
    } else {
        p.continue_session.clone()
    };

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

    let mut driver = PromptNodeDriver {
        codebase: codebase.clone(),
        model: model.clone(),
        allow: allow.clone(),
        agent_name: node.agent.clone(),
        result_schema,
        auto,
        sandbox_cfg,
    };

    if !p.prompt.is_empty() {
        // Literal prompt: substitute vars, create one-shot source, run.
        let task = substitute(&p.prompt, &ctx.vars);
        let repos = gait_core::git::Repos::production();
        let user_bodies = embed::user_turn_events(&repos, &codebase, task);
        let source = NodeSource::Literal(OneTurnSource {
            bodies: Some(user_bodies),
        });

        let mut sink = CapturingSink::new(ctx.workflow_tx.clone());
        embed::run_loop(
            source,
            &mut sink,
            session,
            ctx.gait_dir,
            &session_id,
            &mut driver,
            None,
        )
        .await?;

        // Map sink.result -> NodeOutput
        let turn_result = sink
            .result
            .ok_or_else(|| anyhow::anyhow!("run_loop returned without a TurnResult"))?;
        Ok(map_turn_result(turn_result))
    } else {
        // Absent prompt: interactive node.
        let Some(live) = input else {
            // Headless and no input -> leave the node OPEN and exit.
            return Ok(NodeOutput::open());
        };
        let source = NodeSource::Live(live);

        match ui_sink {
            Some(mut ui) => {
                // Foreground interactive node: render straight through the
                // caller's sink so begin_turn/result/diagnostic all reach it
                // (the channel hop would drop everything but `item`).
                embed::run_loop(
                    source,
                    &mut ui,
                    session,
                    ctx.gait_dir,
                    &session_id,
                    &mut driver,
                    None,
                )
                .await?;
            }
            None => {
                let mut sink = CapturingSink::new(ctx.workflow_tx.clone());
                embed::run_loop(
                    source,
                    &mut sink,
                    session,
                    ctx.gait_dir,
                    &session_id,
                    &mut driver,
                    None,
                )
                .await?;
            }
        }

        // Live node that returned (source exhausted) stays OPEN — no terminal
        // advance has been signalled. The run exits cleanly; resume re-enters.
        Ok(NodeOutput::open())
    }
}

fn map_turn_result(turn_result: TurnResult) -> NodeOutput {
    match turn_result {
        TurnResult::Done {
            message, result, ..
        } => NodeOutput {
            text: message,
            exit_code: None,
            json: result,
            routed_to: None,
        },
        TurnResult::Permissions => NodeOutput {
            text: "(permissions required)".into(),
            exit_code: None,
            json: None,
            routed_to: None,
        },
        TurnResult::Clarify { .. } => NodeOutput {
            text: "(paused for clarification)".into(),
            exit_code: None,
            json: None,
            routed_to: None,
        },
        TurnResult::Inform { .. } => NodeOutput {
            text: "(paused to inform user)".into(),
            exit_code: None,
            json: None,
            routed_to: None,
        },
        TurnResult::Unlock { .. } => NodeOutput {
            text: "(paused for sandbox unlock)".into(),
            exit_code: None,
            json: None,
            routed_to: None,
        },
        TurnResult::Cancelled => NodeOutput {
            text: "(turn cancelled by user)".into(),
            exit_code: None,
            json: None,
            routed_to: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn prompt_node_driver_holds_paths() {
        let driver = PromptNodeDriver {
            codebase: PathBuf::from("/tmp/cb"),
            model: "test/model".into(),
            allow: vec!["run_workflow".into()],
            agent_name: None,
            result_schema: None,
            auto: false,
            sandbox_cfg: gait_core::config::SandboxConfig::default(),
        };
        // Verify the driver holds the paths used to construct BrainLaunch.
        assert_eq!(driver.codebase, PathBuf::from("/tmp/cb"));
    }
}

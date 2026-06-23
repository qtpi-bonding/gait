use std::path::PathBuf;
use std::sync::Arc;

use gait_core::embed::{
    self, BrainLaunch, Diag, LoopConfig, TurnDriver, TurnSink, TurnSource, TurnStart,
};
use gait_core::proto::gait::log::v1::StreamItem;
use gait_core::turn::TurnResult;
use gait_core::Session;

use crate::exec_shell::NodeContext;
use crate::schema::{NodeDef, WorkflowDef};
use crate::vars::{substitute, NodeOutput};

/// Per-node driver: supplies `brain_launch()` so `run_loop` can spawn `gait __brain`
/// for each prompt node. Workflow nodes are now fully sealed (Task 2).
struct PromptNodeDriver {
    gait_dir: PathBuf,
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
    fn brain_launch(&self, session_id: &str) -> BrainLaunch {
        // Resolve API key from the model's provider name.
        let api_key = gait_core::auth::resolve_api_key(
            self.model.split_once('/').map(|(p, _)| p).unwrap_or(""),
        )
        .unwrap_or_default();

        // Build brain_args: the same argv the spine would pass to `gait __brain`.
        // Include --session, --model, --agent (if any), --allow entries, --sandbox-locked.
        let mut brain_args: Vec<String> = vec![
            "__brain".into(),
            "--gait-dir".into(),
            self.gait_dir.to_string_lossy().into_owned(),
            "--codebase".into(),
            self.codebase.to_string_lossy().into_owned(),
            "--session".into(),
            session_id.to_string(),
            "--model".into(),
            self.model.clone(),
            "--sandbox-locked".into(), // workflow nodes are always sealed
            "--auto".into(),           // workflow nodes run non-interactive
        ];
        if let Some(ref agent) = self.agent_name {
            brain_args.push("--agent".into());
            brain_args.push(agent.clone());
        }
        for a in &self.allow {
            brain_args.push("--allow".into());
            brain_args.push(a.clone());
        }

        let gait_bin = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("gait"));

        BrainLaunch::new(
            brain_args,
            self.codebase.clone(),
            api_key,
            gait_bin,
            self.sandbox_cfg.clone(),
            true, // workflow nodes are always sandboxed
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
struct OneTurnSource {
    bodies: Option<Vec<gait_core::proto::gait::log::v1::event::Body>>,
}

#[async_trait::async_trait(?Send)]
impl TurnSource for OneTurnSource {
    async fn next(&mut self) -> Option<TurnStart> {
        self.bodies.take().map(TurnStart::Input)
    }
}

/// Drive a single prompt node through the unified `run_loop` boundary.
///
/// Replaces the old direct `run_turn` + `Arc<dyn Provider>` flow. The driver
/// builds a profile via `build_workflow_node_profile` (Task 1), opens a
/// session, drives exactly one turn, and forwards every sub-turn `StreamItem`
/// to the workflow's channel via `CapturingSink` (preserving the Phase 3b
/// contract that prompt sub-turns publish to the workflow's consumer).
pub async fn exec_prompt_node(
    node: &NodeDef,
    prompt_text: &str,
    ctx: &NodeContext<'_>,
    workflow_def: &WorkflowDef,
) -> anyhow::Result<NodeOutput> {
    let task = substitute(prompt_text, &ctx.vars);

    use gait_core::config::resolver::{ConfigResolver, Overrides};
    let loaded = gait_core::config::load_config(ctx.gait_dir);
    // consumer layer: WorkflowDef fields
    let consumer = Overrides {
        model: workflow_def.model.clone(),
        allow: Some(workflow_def.allow.clone()),
        ..Overrides::empty()
    };
    // per-call layer: NodeDef fields (higher priority)
    let per_call = Overrides {
        model: node.model.clone(),
        allow: node.allow.clone(),
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
    let auto = node.auto.unwrap_or(workflow_def.auto);

    let codebase = node
        .codebase
        .as_ref()
        .map(|cb| PathBuf::from(substitute(cb, &ctx.vars)))
        .unwrap_or_else(|| ctx.codebase.to_path_buf());

    // Open the session up-front so `run_loop` can use `session.repos` for
    // worktree creation.
    let session = Arc::new(Session::open(
        ctx.gait_dir.to_path_buf(),
        codebase.clone(),
        ctx.session_id.to_string(),
    )?);

    // Single-turn source. The body is appended before the fold (Phase 4
    // Option A) — `run_loop` does the append-then-fold.
    let repos = gait_core::git::Repos::production();
    let user_bodies = embed::user_turn_events(&repos, &codebase, task);
    let source = OneTurnSource {
        bodies: Some(user_bodies),
    };

    // Capturing-and-forwarding sink: forwards every sub-turn item to the
    // workflow's channel and captures the final TurnResult.
    let mut sink = CapturingSink::new(ctx.workflow_tx.clone());

    let sandbox_cfg = {
        let cfg = gait_core::config::load_config(ctx.gait_dir);
        cfg.sandbox
    };

    let mut driver = PromptNodeDriver {
        gait_dir: ctx.gait_dir.to_path_buf(),
        codebase: codebase.clone(),
        model: model.clone(),
        allow: allow.clone(),
        agent_name: node.agent.clone(),
        result_schema: node.output_schema.clone(),
        auto,
        sandbox_cfg,
    };

    // Drive one turn. relay_brain_turn inside run_loop writes the sub-turn
    // transcript to the session's log.jsonl; the sink forwards items to the
    // workflow's consumer.
    embed::run_loop(
        source,
        &mut sink,
        session,
        ctx.gait_dir,
        ctx.session_id,
        &mut driver,
        None, // no cascade kill slot for workflow nodes
    )
    .await?;

    // Surface the captured TurnResult as a NodeOutput. Match the legacy
    // mapping so callers (runner.rs) see the same shape.
    let turn_result = sink
        .result
        .ok_or_else(|| anyhow::anyhow!("run_loop returned without a TurnResult"))?;

    Ok(match turn_result {
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
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn prompt_node_driver_holds_paths() {
        let driver = PromptNodeDriver {
            gait_dir: PathBuf::from("/tmp/gd"),
            codebase: PathBuf::from("/tmp/cb"),
            model: "test/model".into(),
            allow: vec!["run_workflow".into()],
            agent_name: None,
            result_schema: None,
            auto: false,
            sandbox_cfg: gait_core::config::SandboxConfig::default(),
        };
        // Verify the driver holds the paths used to construct BrainLaunch.
        assert_eq!(driver.gait_dir, PathBuf::from("/tmp/gd"));
        assert_eq!(driver.codebase, PathBuf::from("/tmp/cb"));
    }
}

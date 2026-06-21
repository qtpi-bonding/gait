use std::path::PathBuf;
use std::sync::Arc;

use gait_core::embed::{self, Diag, LoopConfig, TurnDriver, TurnSink, TurnSource, TurnStart};
use gait_core::profile::{build_workflow_node_profile, WorkflowNodeProfileOpts};
use gait_core::proto::gait::log::v1::StreamItem;
use gait_core::turn::TurnResult;
use gait_core::Session;

use crate::exec_shell::NodeContext;
use crate::schema::{NodeDef, WorkflowDef};
use crate::vars::{substitute, NodeOutput};

/// Inputs the driver needs to (re-)build the per-node `Profile` for each turn.
/// We don't store the `Profile` directly because `Profile` does not derive
/// `Clone` — `run_loop` calls `resolve_profile` per turn, and we want a fresh
/// resolve from the same per-node opts each time.
struct PromptNodeDriverOpts {
    /// Resolved model string ("provider/model-id").
    model: String,
    /// Allow list (replaces config.allow).
    allow: Vec<String>,
    /// Optional named agent (path resolved at profile-build time).
    agent_name: Option<String>,
    /// Inline system prompt body.
    system_prompt: Option<String>,
    /// JSON Schema for `structured_done.result`.
    result_schema: Option<serde_json::Value>,
    /// auto permissions gate.
    auto: bool,
}

struct PromptNodeDriver {
    gait_dir: PathBuf,
    session_id: String,
    codebase: PathBuf,
    opts: PromptNodeDriverOpts,
}

#[async_trait::async_trait(?Send)]
impl TurnDriver for PromptNodeDriver {
    async fn resolve_profile(&mut self) -> anyhow::Result<gait_core::profile::ResolvedProfile> {
        let config = gait_core::config::load_config(&self.gait_dir);
        let profile = build_workflow_node_profile(WorkflowNodeProfileOpts {
            config: &config,
            gait_dir: &self.gait_dir,
            model: Some(self.opts.model.clone()),
            agent_name: self.opts.agent_name.clone(),
            system_prompt: self.opts.system_prompt.as_deref(),
            allow: self.opts.allow.clone(),
            result_schema: self.opts.result_schema.clone(),
        });
        let api_key = gait_core::auth::resolve_api_key(
            profile.model.split_once('/').map(|(p, _)| p).unwrap_or(""),
        );
        let repos = gait_core::git::Repos::production();
        let runner = std::sync::Arc::new(crate::runner_impl::GaitWorkflowRunner::new(
            self.gait_dir.clone(),
            self.codebase.clone(),
        ));
        let resolved = profile
            .resolve(
                self.codebase.clone(),
                self.session_id.clone(),
                self.gait_dir.clone(),
                repos,
                api_key,
                Some(runner),
            )
            .await?;
        Ok(resolved)
    }

    fn turn_config(&mut self) -> LoopConfig {
        LoopConfig {
            auto: self.opts.auto,
            max_iter: None,
            preview_policy: gait_core::config::PreviewPolicy::default(),
            cancel: None,
            sandbox_on: true,
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

    let model = node
        .model
        .clone()
        .or_else(|| workflow_def.model.clone())
        .unwrap_or_else(|| "minimax/MiniMax-M3".into());
    let allow = node
        .allow
        .clone()
        .unwrap_or_else(|| workflow_def.allow.clone());
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

    let driver_opts = PromptNodeDriverOpts {
        model: model.clone(),
        allow: allow.clone(),
        agent_name: node.agent.clone(),
        system_prompt: node.system_prompt.clone(),
        result_schema: node.output_schema.clone(),
        auto,
    };
    let mut driver = PromptNodeDriver {
        gait_dir: ctx.gait_dir.to_path_buf(),
        session_id: ctx.session_id.to_string(),
        codebase: codebase.clone(),
        opts: driver_opts,
    };

    // Drive one turn. The per-turn `Recorder` inside `run_loop` writes the
    // sub-turn transcript to the session's `log.jsonl`; the sink forwards
    // channel items to the workflow's consumer.
    embed::run_loop(
        source,
        &mut sink,
        session,
        ctx.gait_dir,
        ctx.session_id,
        &mut driver,
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
    fn prompt_node_driver_opts_capture_gait_dir() {
        let opts = PromptNodeDriverOpts {
            model: "test/model".into(),
            allow: vec!["run_workflow".into()],
            agent_name: None,
            system_prompt: None,
            result_schema: None,
            auto: false,
        };
        let driver = PromptNodeDriver {
            gait_dir: PathBuf::from("/tmp/gd"),
            session_id: "s1".into(),
            codebase: PathBuf::from("/tmp/cb"),
            opts,
        };
        // Verify the driver holds the paths we'll use to construct GaitWorkflowRunner.
        assert_eq!(driver.gait_dir, PathBuf::from("/tmp/gd"));
        assert_eq!(driver.codebase, PathBuf::from("/tmp/cb"));
    }
}

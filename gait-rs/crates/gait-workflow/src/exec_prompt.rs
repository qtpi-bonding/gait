use gait_core::embed::{Diag, SinkFrame, TurnSink};
use gait_core::proto::gait::log::v1::{RunResult, StreamItem};
use gait_core::proto::gait::workflow::v1::{Node, PromptNode, Workflow};

use crate::exec_shell::NodeContext;
use crate::vars::{substitute, NodeOutput};

/// Capturing-and-forwarding sink: forwards every `TurnSink` call to the
/// workflow's `SinkFrame` channel (spec-06 §4 — begin_turn/item/end_turn/
/// result/session_id/diagnostic all round-trip, not just `item`), and
/// captures the final `TurnResult` so the caller can return it as a
/// `NodeOutput`.
///
/// This is workflow's own drain-channel plumbing (it owns
/// `workflow_tx: UnboundedSender<SinkFrame>` and constructs `SinkFrame`s) —
/// unlike `gait_turn_runner::DirectSink`, which forwards straight to a
/// caller's sink with no channel — so it stays here even though the actual
/// sealed-turn execution now lives in `gait-turn-runner` (Task 2).
pub(crate) struct CapturingSink {
    /// Borrowed workflow channel sender. Forwards every call we receive so the
    /// workflow's drain consumer sees the sub-turn's full output, verbatim.
    workflow_tx: tokio::sync::mpsc::UnboundedSender<SinkFrame>,
    result: Option<RunResult>,
    failed: bool,
}

impl CapturingSink {
    pub(crate) fn new(workflow_tx: tokio::sync::mpsc::UnboundedSender<SinkFrame>) -> Self {
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

/// Map the shared runner's `TurnRunOutcome` to workflow's own `NodeOutput`.
/// Same shape `map_turn_result` had pre-Task-2, but reading the runner's
/// already-mapped `TurnRunOutcome` fields instead of a raw `RunResult` — the
/// text/json/exit_code → `TurnRunOutcome` mapping itself now lives in
/// `gait_turn_runner::run_result_to_outcome`.
fn map_turn_outcome(outcome: gait_turn_runner::TurnRunOutcome, session_id: &str) -> NodeOutput {
    if outcome.opened {
        return NodeOutput::open();
    }
    NodeOutput {
        text: outcome.text.unwrap_or_default(),
        exit_code: outcome.exit_code,
        json: outcome.json,
        routed_to: None,
        session_id: session_id.to_string(),
    }
}

/// Drive a single prompt node through the shared sealed-turn runner
/// (`gait_turn_runner::run_turn_source`/`run_single_prompt`).
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
    input: Option<Box<dyn gait_core::embed::TurnSource>>,
) -> anyhow::Result<NodeOutput> {
    // One path, no duplication: the config resolution / CLI-override folding /
    // `TurnRunOpts` construction lives solely in
    // `RealTurnExecutor::build_turn_opts` — `exec_prompt_node` calls it with a
    // default `TurnRequest` (proto node fields, no script-level overrides).
    let exec = crate::turn_exec::RealTurnExecutor::new(node, p, wf, ctx);
    let turn_opts = exec.build_turn_opts(&crate::turn_exec::TurnRequest::default())?;
    let session_id = ctx.node_session.to_string();

    if !p.prompt.is_empty() || input.is_some() {
        // A caller may provide a live source even for a prompt-bearing node.
        // ACP uses this to keep its permission/resume protocol in the source;
        // ordinary CLI/workflow callers have no source and use the literal
        // one-shot prompt below.
        let mut sink = CapturingSink::new(ctx.workflow_tx.clone());
        let outcome = if let Some(live) = input {
            gait_turn_runner::run_turn_source(turn_opts, live, &mut sink).await?
        } else {
            let task = substitute(&p.prompt, &ctx.vars);
            gait_turn_runner::run_single_prompt(turn_opts, Some(task), &mut sink).await?
        };
        Ok(map_turn_outcome(outcome, &session_id))
    } else {
        // Absent prompt: interactive node. `input: None` means no live source
        // was supplied (headless/no-input) — return open immediately without
        // driving anything.
        let Some(live) = input else {
            return Ok(NodeOutput::open());
        };
        let mut sink = CapturingSink::new(ctx.workflow_tx.clone());
        let _outcome = gait_turn_runner::run_turn_source(turn_opts, live, &mut sink).await?;

        // Live node that returned (source exhausted) stays OPEN — no terminal
        // advance has been signalled. The run exits cleanly; resume re-enters.
        Ok(NodeOutput::open())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

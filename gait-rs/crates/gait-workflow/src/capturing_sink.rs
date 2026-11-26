//! Capturing-and-forwarding sink: forwards every `TurnSink` call to the
//! workflow's `SinkFrame` channel (spec-06 §4 — begin_turn/item/end_turn/
//! result/session_id/diagnostic all round-trip, not just `item`).
//!
//! This is workflow's own drain-channel plumbing (it owns
//! `workflow_tx: UnboundedSender<SinkFrame>` and constructs `SinkFrame`s) —
//! unlike `gait_turn_runner::DirectSink`, which forwards straight to a
//! caller's sink with no channel — so it stays here even though the actual
//! sealed-turn execution now lives in `gait-turn-runner`.

use gait_core::embed::{Diag, SinkFrame, TurnSink};
use gait_core::proto::gait::log::v1::{RunResult, StreamItem};

/// Capturing-and-forwarding sink: forwards every `TurnSink` call to the
/// workflow's `SinkFrame` channel.
pub(crate) struct CapturingSink {
    /// Borrowed workflow channel sender. Forwards every call we receive so the
    /// workflow's drain consumer sees the sub-turn's full output, verbatim.
    workflow_tx: tokio::sync::mpsc::UnboundedSender<SinkFrame>,
}

impl CapturingSink {
    pub(crate) fn new(workflow_tx: tokio::sync::mpsc::UnboundedSender<SinkFrame>) -> Self {
        Self { workflow_tx }
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
        let _ = self
            .workflow_tx
            .send(SinkFrame::Result(Box::new(r.clone())));
    }
    fn session_id(&mut self, id: &str) {
        let _ = self.workflow_tx.send(SinkFrame::SessionId(id.to_string()));
    }
    fn diagnostic(&mut self, level: Diag, msg: &str) {
        let _ = self
            .workflow_tx
            .send(SinkFrame::Diagnostic(level, msg.to_string()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gait_core::embed::{Diag, SinkFrame, TurnSink};
    use gait_core::proto::gait::log::v1::{stream_item, StreamItem, TextDelta};

    #[tokio::test]
    async fn capturing_sink_forwards_every_call_over_the_channel() {
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

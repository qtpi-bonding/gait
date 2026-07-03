//! Spec-06 §4 Task 2.3: golden-output parity — the channel-routed path
//! (CapturingSink -> SinkFrame channel -> SinkFrame::replay) must reproduce a
//! direct sink's call sequence verbatim (modulo the event `id`, freshly
//! minted per `ev()` call since each side runs the scenario independently).
//!
//! `run_loop` drives the brain as a subprocess, so there is no in-process
//! provider seam to script on that path. This test instead scripts the exact
//! `TurnSink` call sequence a real turn emits (deltas, a tool call carrying a
//! permission preview, a diagnostic, structured-done, cancel) and plays it
//! into (a) a direct `RecordingSink` and (b) a `CapturingSink` whose channel
//! is drained via `SinkFrame::replay` into a second `RecordingSink` — this is
//! precisely the round-trip Task 2.2's runner change introduced.

use gait_core::embed::{Diag, RecordingSink, SinkFrame, TurnSink};
use gait_core::proto::envelope::mint;
use gait_core::proto::events;
use gait_core::proto::gait::log::v1::{event, stream_item, StreamItem, TextDelta};
use gait_core::turn::TurnResult;

fn ev(body: event::Body) -> StreamItem {
    StreamItem {
        item: Some(stream_item::Item::Event(mint(body, None, 0, 0))),
    }
}

/// Zero the id and ts (minted fresh per `ev()` call — `ts` is wall-clock, so it
/// can differ by a millisecond between the two independent `play_golden` runs
/// — flaky otherwise) on every `Event` item. The parity check cares about
/// payload fidelity, not identity/timing, mirroring the plan's `norm()`.
fn norm_items(items: &[StreamItem]) -> Vec<StreamItem> {
    items
        .iter()
        .cloned()
        .map(|mut it| {
            if let Some(stream_item::Item::Event(ev)) = it.item.as_mut() {
                ev.id.clear();
                ev.ts = 0;
            }
            it
        })
        .collect()
}

/// The golden scenario: begin_turn, session_id, two text deltas, a tool call
/// (with a permission preview riding in its own event), a diagnostic ping,
/// structured-done (mapped to `TurnResult::Done`), end_turn.
fn play_golden(sink: &mut dyn TurnSink) {
    sink.begin_turn();
    sink.session_id("s0000001");
    sink.item(&StreamItem {
        item: Some(stream_item::Item::Delta(TextDelta {
            content: "Hello, ".into(),
        })),
    });
    sink.item(&StreamItem {
        item: Some(stream_item::Item::Delta(TextDelta {
            content: "world!".into(),
        })),
    });
    sink.item(&ev(events::tool_call(
        "call-1".into(),
        "read".into(),
        &serde_json::json!({"path": "README.md"}),
    )));
    sink.item(&ev(events::permissions(&[])));
    sink.diagnostic(Diag::Info, "structured_done pending");
    sink.result(&TurnResult::Done {
        message: "done".into(),
        result: Some(serde_json::json!({"ok": true})),
        changes: None,
    });
    sink.end_turn();
}

#[tokio::test]
async fn channel_routed_output_matches_direct_output() {
    // (a) Direct path — a caller sink driven straight, as `CliSink::stdio`
    // would be in `cmd_run` today.
    let mut direct = RecordingSink::default();
    play_golden(&mut direct);

    // (b) Channel-routed path — CapturingSink forwards every call as a
    // SinkFrame; drain replays each frame onto a second sink, exactly as
    // `run_workflow`'s pool `select!` does (runner.rs, post-Task-2.2).
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<SinkFrame>();
    {
        let mut cap_sink = ChannelSink { tx };
        play_golden(&mut cap_sink);
    }
    let mut drained = RecordingSink::default();
    while let Ok(frame) = rx.try_recv() {
        frame.replay(&mut drained);
    }

    assert_eq!(
        norm_items(&direct.items),
        norm_items(&drained.items),
        "items must survive the channel round-trip verbatim (modulo minted event id)"
    );
    // `TurnResult` has no `PartialEq`; normalize both sides to (message, result)
    // pairs for the comparison — the only fields the golden scenario sets.
    fn norm_results(rs: &[TurnResult]) -> Vec<(String, Option<serde_json::Value>)> {
        rs.iter()
            .map(|r| match r {
                TurnResult::Done {
                    message, result, ..
                } => (message.clone(), result.clone()),
                other => (format!("{other:?}"), None),
            })
            .collect()
    }
    assert_eq!(
        norm_results(&direct.results),
        norm_results(&drained.results),
        "the final TurnResult must survive the channel round-trip verbatim"
    );
    assert_eq!(
        direct.diagnostics, drained.diagnostics,
        "diagnostics must survive the channel round-trip verbatim"
    );
}

/// Minimal `TurnSink` that forwards every call as a `SinkFrame`, mirroring
/// `exec_prompt::CapturingSink` without depending on its private type.
struct ChannelSink {
    tx: tokio::sync::mpsc::UnboundedSender<SinkFrame>,
}
impl TurnSink for ChannelSink {
    fn begin_turn(&mut self) {
        let _ = self.tx.send(SinkFrame::BeginTurn);
    }
    fn item(&mut self, item: &StreamItem) {
        let _ = self.tx.send(SinkFrame::Item(Box::new(item.clone())));
    }
    fn end_turn(&mut self) {
        let _ = self.tx.send(SinkFrame::EndTurn);
    }
    fn result(&mut self, r: &TurnResult) {
        let _ = self.tx.send(SinkFrame::Result(r.clone()));
    }
    fn session_id(&mut self, id: &str) {
        let _ = self.tx.send(SinkFrame::SessionId(id.to_string()));
    }
    fn diagnostic(&mut self, level: Diag, msg: &str) {
        let _ = self.tx.send(SinkFrame::Diagnostic(level, msg.to_string()));
    }
}

//! Agnostic turn-output port: classify channel items + the TurnSink contract.
use crate::proto::gait::log::v1::{
    event, payload, stream_item, Event, RunResult, SessionStart, StreamItem,
};
use std::io::Write;

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) enum StreamClass {
    Trace,
    Error,
}

pub(crate) fn classify(item: &StreamItem) -> StreamClass {
    match &item.item {
        Some(stream_item::Item::Event(ev)) => classify_event(ev),
        _ => StreamClass::Trace, // Delta or empty
    }
}
fn classify_event(ev: &Event) -> StreamClass {
    match ev.body.as_ref() {
        Some(event::Body::Payload(p)) => match &p.kind {
            Some(payload::Kind::Error(_)) => StreamClass::Error,
            Some(payload::Kind::Raw(r))
                if r.r#type == "workflow_error" || r.r#type == "node_error" =>
            {
                StreamClass::Error
            }
            _ => StreamClass::Trace,
        },
        _ => StreamClass::Trace,
    }
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Diag {
    Info,
    Error,
}

pub trait TurnSink {
    fn item(&mut self, item: &StreamItem);
    fn result(&mut self, r: &RunResult);
    fn session_id(&mut self, id: &str);
    fn diagnostic(&mut self, level: Diag, msg: &str);
    /// Turn lifecycle hooks (default no-op). The unified `run_loop` calls these
    /// around each turn; the CLI sink ignores them, the REPL sink uses them to
    /// begin/flush its per-turn view state.
    fn begin_turn(&mut self) {}
    fn end_turn(&mut self) {}
}

/// Forwarding impl so a `&mut dyn TurnSink` trait object (and reborrows of one)
/// satisfies `impl TurnSink` — `run_loop` takes `&mut impl TurnSink`, so callers
/// holding only a trait object can pass `&mut *sink` straight through.
impl TurnSink for &mut dyn TurnSink {
    fn item(&mut self, item: &StreamItem) {
        (**self).item(item);
    }
    fn result(&mut self, r: &RunResult) {
        (**self).result(r);
    }
    fn session_id(&mut self, id: &str) {
        (**self).session_id(id);
    }
    fn diagnostic(&mut self, level: Diag, msg: &str) {
        (**self).diagnostic(level, msg);
    }
    fn begin_turn(&mut self) {
        (**self).begin_turn();
    }
    fn end_turn(&mut self) {
        (**self).end_turn();
    }
}

/// A single `TurnSink` invocation, reified so it can cross an mpsc channel and be
/// replayed onto a downstream sink call-for-call — the widened channel element
/// that lets a captured sub-turn round-trip EVERY sink call (spec-06 §4).
#[derive(Clone)]
pub enum SinkFrame {
    BeginTurn,
    // Boxed: StreamItem (>600 bytes) would otherwise make every SinkFrame that
    // large, even the zero-payload variants (clippy::large_enum_variant).
    Item(Box<StreamItem>),
    EndTurn,
    Result(Box<RunResult>),
    SessionId(String),
    Diagnostic(Diag, String),
}

impl SinkFrame {
    pub fn replay(self, sink: &mut dyn TurnSink) {
        match self {
            SinkFrame::BeginTurn => sink.begin_turn(),
            SinkFrame::Item(it) => sink.item(&it),
            SinkFrame::EndTurn => sink.end_turn(),
            SinkFrame::Result(r) => sink.result(&r),
            SinkFrame::SessionId(id) => sink.session_id(&id),
            SinkFrame::Diagnostic(level, msg) => sink.diagnostic(level, &msg),
        }
    }
}

pub async fn drive(
    mut rx: tokio::sync::mpsc::UnboundedReceiver<StreamItem>,
    sink: &mut impl TurnSink,
) {
    while let Some(item) = rx.recv().await {
        sink.item(&item);
    }
}

/// The CLI/headless sink: routes channel items by class to stdout/stderr.
pub struct CliSink<O: Write, E: Write> {
    out: O,
    err: E,
    quiet: bool,
    pub failed: bool,
}

impl CliSink<std::io::Stdout, std::io::Stderr> {
    pub fn stdio(quiet: bool) -> Self {
        Self {
            out: std::io::stdout(),
            err: std::io::stderr(),
            quiet,
            failed: false,
        }
    }
}
impl<O: Write, E: Write> CliSink<O, E> {
    pub fn new(out: O, err: E, quiet: bool) -> Self {
        Self {
            out,
            err,
            quiet,
            failed: false,
        }
    }
}

impl<O: Write, E: Write> TurnSink for CliSink<O, E> {
    fn item(&mut self, item: &StreamItem) {
        let to_err = match classify(item) {
            StreamClass::Error => true,
            StreamClass::Trace => !self.quiet,
        };
        if to_err {
            if let Ok(j) = serde_json::to_string(item) {
                let _ = writeln!(self.err, "{j}");
            }
        }
        // NOTE: Error items go to stderr but do NOT flip the exit code (hard-failures-only policy).
    }
    fn result(&mut self, r: &RunResult) {
        if let Ok(j) = serde_json::to_string(r) {
            let _ = writeln!(self.out, "{j}");
        }
    }
    fn session_id(&mut self, id: &str) {
        let ss = SessionStart {
            session_id: id.to_string(),
            assigned_by: None,
            codebase_path: None,
            notify_done: false,
        };
        if let Ok(j) = serde_json::to_string(&ss) {
            let _ = writeln!(self.out, "{j}");
        }
    }
    fn diagnostic(&mut self, level: Diag, msg: &str) {
        if level == Diag::Error {
            self.failed = true;
        }
        if level == Diag::Error || !self.quiet {
            let _ = writeln!(self.err, "[gait] {msg}");
        }
    }
}

/// An in-memory `TurnSink` for tests and programmatic embedding: records every
/// frame it receives. Not used in production rendering paths.
#[derive(Default)]
pub struct RecordingSink {
    pub items: Vec<StreamItem>,
    pub results: Vec<RunResult>,
    pub diagnostics: Vec<(Diag, String)>,
}

impl TurnSink for RecordingSink {
    fn item(&mut self, item: &StreamItem) {
        self.items.push(item.clone());
    }
    fn result(&mut self, r: &RunResult) {
        self.results.push(r.clone());
    }
    fn session_id(&mut self, _id: &str) {}
    fn diagnostic(&mut self, level: Diag, msg: &str) {
        self.diagnostics.push((level, msg.to_string()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::envelope::EventScope;
    use crate::proto::events;
    fn ev(body: event::Body) -> StreamItem {
        StreamItem {
            item: Some(stream_item::Item::Event(crate::proto::envelope::mint(
                body,
                None,
                EventScope::Session,
            ))),
        }
    }
    #[test]
    fn sink_frame_replays_every_variant_in_order() {
        let frames = vec![
            SinkFrame::BeginTurn,
            SinkFrame::SessionId("s0000001".into()),
            SinkFrame::Item(Box::new(ev(events::tool_call(
                "c1".into(),
                "read".into(),
                &serde_json::json!({}),
            )))),
            SinkFrame::Diagnostic(Diag::Error, "boom".into()),
            SinkFrame::Result(Box::new(RunResult {
                run_id: "r1".into(),
                state: Some(crate::proto::gait::log::v1::run_result::State::Paused(
                    Default::default(),
                )),
                ..Default::default()
            })),
            SinkFrame::EndTurn,
        ];
        let mut rec = RecordingSink::default();
        for f in frames {
            f.replay(&mut rec);
        }
        assert_eq!(rec.items.len(), 1);
        assert_eq!(rec.results.len(), 1);
        assert_eq!(rec.diagnostics, vec![(Diag::Error, "boom".to_string())]);
    }

    #[test]
    fn error_event_is_error() {
        assert_eq!(
            classify(&ev(events::error("boom".into(), "tool"))),
            StreamClass::Error
        );
    }
    #[test]
    fn tool_call_is_trace() {
        assert_eq!(
            classify(&ev(events::tool_call(
                "c1".into(),
                "read".into(),
                &serde_json::json!({})
            ))),
            StreamClass::Trace
        );
    }
    #[test]
    fn classify_routes_workflow_errors_to_error() {
        use crate::proto::gait::log::v1::{
            event, payload, stream_item, Event, Payload, RawEvent, StreamItem,
        };
        fn raw_item(ty: &str) -> StreamItem {
            let body = event::Body::Payload(Payload {
                kind: Some(payload::Kind::Raw(RawEvent {
                    r#type: ty.into(),
                    payload: None,
                })),
            });
            StreamItem {
                item: Some(stream_item::Item::Event(Event {
                    body: Some(body),
                    ..Default::default()
                })),
            }
        }
        assert_eq!(classify(&raw_item("workflow_error")), StreamClass::Error);
        assert_eq!(classify(&raw_item("node_error")), StreamClass::Error);
        // Non-error raw events are Trace.
        for ty in [
            "workflow_start",
            "node_start",
            "node_skip",
            "node_complete",
            "hook_run",
            "workflow_gate",
            "workflow_complete",
        ] {
            assert_eq!(
                classify(&raw_item(ty)),
                StreamClass::Trace,
                "{ty} should be Trace"
            );
        }
    }
    #[test]
    fn aggregate_run_result_serializes_directly() {
        let r = RunResult {
            run_id: "r1".into(),
            state: Some(crate::proto::gait::log::v1::run_result::State::Complete(
                Default::default(),
            )),
            ..Default::default()
        };
        let value: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
        assert_eq!(value["runId"], "r1");
        assert!(value.get("complete").is_some());
    }

    #[test]
    fn cli_sink_trace_to_err_not_out() {
        let (mut o, mut e) = (Vec::new(), Vec::new());
        let mut s = CliSink::new(&mut o, &mut e, false);
        s.item(&ev(events::tool_call(
            "c1".into(),
            "read".into(),
            &serde_json::json!({}),
        )));
        assert!(o.is_empty());
        assert!(!e.is_empty());
    }
    #[test]
    fn cli_sink_error_event_to_err_does_not_set_failed() {
        let (mut o, mut e) = (Vec::new(), Vec::new());
        let mut s = CliSink::new(&mut o, &mut e, true); // quiet
        s.item(&ev(events::error("boom".into(), "tool")));
        assert!(!s.failed, "error EVENT must not flip exit");
        assert!(!e.is_empty(), "error still shown on stderr even when quiet");
        assert!(o.is_empty());
    }
    #[test]
    fn cli_sink_diagnostic_error_sets_failed() {
        let (mut o, mut e) = (Vec::new(), Vec::new());
        let mut s = CliSink::new(&mut o, &mut e, true);
        s.diagnostic(Diag::Error, "resolve failed");
        assert!(s.failed);
        assert!(!e.is_empty());
    }
    #[test]
    fn cli_sink_session_id_is_proto_on_stdout() {
        let (mut o, mut e) = (Vec::new(), Vec::new());
        let mut s = CliSink::new(&mut o, &mut e, false);
        s.session_id("s1234567");
        let v: serde_json::Value = serde_json::from_slice(&o).unwrap();
        assert_eq!(v["sessionId"], "s1234567"); // pbjson camelCase
    }
    #[test]
    fn recording_sink_collects_items_and_results() {
        let mut s = RecordingSink::default();
        s.item(&ev(events::tool_call(
            "c1".into(),
            "read".into(),
            &serde_json::json!({}),
        )));
        s.result(&RunResult {
            run_id: "r1".into(),
            state: Some(crate::proto::gait::log::v1::run_result::State::Paused(
                Default::default(),
            )),
            ..Default::default()
        });
        s.diagnostic(Diag::Error, "boom");
        assert_eq!(s.items.len(), 1);
        assert_eq!(s.results.len(), 1);
        assert_eq!(s.diagnostics, vec![(Diag::Error, "boom".to_string())]);
    }
}

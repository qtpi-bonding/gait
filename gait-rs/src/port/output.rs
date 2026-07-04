//! Agnostic turn-output port: classify channel items + the TurnSink contract.
use crate::proto::events;
use crate::proto::gait::log::v1::{
    event, payload, run_result, stream_item, Event, RunResult, SessionStart, StreamItem,
    StructuredDoneOutput, Unlock,
};
use crate::turn::TurnResult;
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
    fn result(&mut self, r: &crate::turn::TurnResult);
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
    fn result(&mut self, r: &crate::turn::TurnResult) {
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
    Result(TurnResult),
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

fn extract_kind(body: event::Body) -> Option<payload::Kind> {
    match body {
        event::Body::Payload(p) => p.kind,
        _ => None,
    }
}

pub fn run_result_proto(r: &crate::turn::TurnResult) -> RunResult {
    use crate::turn::TurnResult;
    // Returns `(Option<oneof>, ended)` — failures legitimately carry no oneof,
    // so the field is `None` for `TurnResult::Failed`. `ended` is always set
    // (the wire field is the discriminant; `result` may be None).
    let (result, ended): (
        Option<crate::proto::gait::log::v1::run_result::Result>,
        &str,
    ) = match r {
        TurnResult::Done {
            message,
            result,
            changes,
            implicit,
        } => (
            Some(run_result::Result::Done(StructuredDoneOutput {
                message: message.clone(),
                result: events::to_value_opt(result),
                changes: changes.clone(),
            })),
            if *implicit {
                "implicit_done"
            } else {
                "structured_done"
            },
        ),
        TurnResult::Clarify { items } => (
            Some(match extract_kind(events::clarify(items.as_slice())) {
                Some(payload::Kind::Clarify(c)) => run_result::Result::Clarify(c),
                _ => run_result::Result::Clarify(Default::default()),
            }),
            "clarify",
        ),
        TurnResult::Inform { items } => (
            Some(match extract_kind(events::inform(items.as_slice())) {
                Some(payload::Kind::Inform(i)) => run_result::Result::Inform(i),
                _ => run_result::Result::Inform(Default::default()),
            }),
            "inform",
        ),
        TurnResult::Permissions => (
            Some(run_result::Result::Permissions(Default::default())),
            "permissions",
        ),
        TurnResult::Unlock { items } => (
            Some(run_result::Result::Unlock(Unlock {
                items: items.clone(),
            })),
            "unlock",
        ),
        TurnResult::Cancelled => (
            Some(run_result::Result::Cancelled(Default::default())),
            "cancelled",
        ),
        TurnResult::Failed { ended, .. } => {
            // Failure carries no oneof — `ended` is the only reason signal.
            (None, ended.as_str())
        }
    };
    RunResult {
        result,
        ended: ended.to_string(),
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
    fn result(&mut self, r: &TurnResult) {
        if let Ok(j) = serde_json::to_string(&run_result_proto(r)) {
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
    pub results: Vec<TurnResult>,
    pub diagnostics: Vec<(Diag, String)>,
}

impl TurnSink for RecordingSink {
    fn item(&mut self, item: &StreamItem) {
        self.items.push(item.clone());
    }
    fn result(&mut self, r: &TurnResult) {
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
    use crate::proto::events;
    fn ev(body: event::Body) -> StreamItem {
        StreamItem {
            item: Some(stream_item::Item::Event(crate::proto::envelope::mint(
                body, None, 0, 0,
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
            SinkFrame::Result(crate::turn::TurnResult::Permissions),
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
    fn run_result_failed_carries_ended_and_no_oneof() {
        use crate::turn::TurnResult;
        let r = TurnResult::Failed {
            ended: "max_steps".into(),
            message: "reached max steps (100) without turn_complete".into(),
        };
        let p = run_result_proto(&r);
        assert_eq!(p.ended, "max_steps");
        assert!(p.result.is_none(), "failure carries no oneof result");
    }

    #[test]
    fn run_result_sets_ended_for_done_split() {
        use crate::turn::TurnResult;
        let implicit = TurnResult::Done {
            message: "".into(),
            result: None,
            changes: None,
            implicit: true,
        };
        let structured = TurnResult::Done {
            message: "ok".into(),
            result: None,
            changes: None,
            implicit: false,
        };
        assert_eq!(run_result_proto(&implicit).ended, "implicit_done");
        assert_eq!(run_result_proto(&structured).ended, "structured_done");
        assert_eq!(
            run_result_proto(&TurnResult::Permissions).ended,
            "permissions"
        );
        assert_eq!(run_result_proto(&TurnResult::Cancelled).ended, "cancelled");
    }

    #[test]
    fn run_result_permissions_maps() {
        let v: serde_json::Value = serde_json::from_str(
            &serde_json::to_string(&run_result_proto(&crate::turn::TurnResult::Permissions))
                .unwrap(),
        )
        .unwrap();
        assert!(v.get("permissions").is_some());
    }
    #[test]
    fn run_result_done_serializes_with_changes() {
        use crate::proto::gait::log::v1::Changes;
        let r = crate::turn::TurnResult::Done {
            message: "ok".into(),
            result: None,
            changes: Some(Changes {
                added: vec!["a.rs".into()],
                modified: vec![],
                deleted: vec![],
            }),
            implicit: false,
        };
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&run_result_proto(&r)).unwrap()).unwrap();
        assert_eq!(v["done"]["changes"]["added"][0], "a.rs");
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
        s.result(&crate::turn::TurnResult::Permissions);
        s.diagnostic(Diag::Error, "boom");
        assert_eq!(s.items.len(), 1);
        assert_eq!(s.results.len(), 1);
        assert_eq!(s.diagnostics, vec![(Diag::Error, "boom".to_string())]);
    }
}

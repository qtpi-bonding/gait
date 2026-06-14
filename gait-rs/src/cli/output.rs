//! CLI adapter for the output port: routes by class to stdout/stderr.
use std::io::Write;
use crate::port::output::{classify, run_result_proto, Diag, StreamClass, TurnSink};
use crate::proto::gait::log::v1::{SessionStart, StreamItem};
use crate::turn::TurnResult;

pub struct StdioSink<O: Write, E: Write> { out: O, err: E, quiet: bool, pub failed: bool }

impl StdioSink<std::io::Stdout, std::io::Stderr> {
    pub fn stdio(quiet: bool) -> Self {
        Self { out: std::io::stdout(), err: std::io::stderr(), quiet, failed: false }
    }
}
impl<O: Write, E: Write> StdioSink<O, E> {
    pub fn new(out: O, err: E, quiet: bool) -> Self { Self { out, err, quiet, failed: false } }
}

impl<O: Write, E: Write> TurnSink for StdioSink<O, E> {
    fn item(&mut self, item: &StreamItem) {
        let to_err = match classify(item) { StreamClass::Error => true, StreamClass::Trace => !self.quiet };
        if to_err { if let Ok(j) = serde_json::to_string(item) { let _ = writeln!(self.err, "{j}"); } }
        // NOTE: Error items go to stderr but do NOT flip the exit code (hard-failures-only policy).
    }
    fn result(&mut self, r: &TurnResult) {
        if let Ok(j) = serde_json::to_string(&run_result_proto(r)) { let _ = writeln!(self.out, "{j}"); }
    }
    fn session_id(&mut self, id: &str) {
        let ss = SessionStart { session_id: id.to_string(), assigned_by: None };
        if let Ok(j) = serde_json::to_string(&ss) { let _ = writeln!(self.out, "{j}"); }
    }
    fn diagnostic(&mut self, level: Diag, msg: &str) {
        if level == Diag::Error { self.failed = true; }
        if level == Diag::Error || !self.quiet { let _ = writeln!(self.err, "[gait] {msg}"); }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::{events, wire};
    use crate::proto::gait::log::v1::{stream_item, StreamItem};
    fn item(body: crate::proto::gait::log::v1::event::Body) -> StreamItem {
        StreamItem { item: Some(stream_item::Item::Event(wire::body_to_event_for_test(body))) }
    }
    #[test]
    fn trace_to_err_not_out() {
        let (mut o, mut e) = (Vec::new(), Vec::new());
        let mut s = StdioSink::new(&mut o, &mut e, false);
        s.item(&item(events::tool_call("c1".into(), "read".into(), &serde_json::json!({}))));
        assert!(o.is_empty()); assert!(!e.is_empty());
    }
    #[test]
    fn error_event_to_err_does_not_set_failed() {
        let (mut o, mut e) = (Vec::new(), Vec::new());
        let mut s = StdioSink::new(&mut o, &mut e, true); // quiet
        s.item(&item(events::error("boom".into(), "tool")));
        assert!(!s.failed, "error EVENT must not flip exit");
        assert!(!e.is_empty(), "error still shown on stderr even when quiet");
        assert!(o.is_empty());
    }
    #[test]
    fn diagnostic_error_sets_failed() {
        let (mut o, mut e) = (Vec::new(), Vec::new());
        let mut s = StdioSink::new(&mut o, &mut e, true);
        s.diagnostic(Diag::Error, "resolve failed");
        assert!(s.failed); assert!(!e.is_empty());
    }
    #[test]
    fn session_id_is_proto_on_stdout() {
        let (mut o, mut e) = (Vec::new(), Vec::new());
        let mut s = StdioSink::new(&mut o, &mut e, false);
        s.session_id("s1234567");
        let v: serde_json::Value = serde_json::from_slice(&o).unwrap();
        assert_eq!(v["sessionId"], "s1234567"); // pbjson camelCase
    }
}

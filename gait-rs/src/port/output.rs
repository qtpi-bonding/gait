//! Agnostic turn-output port: classify channel items + the TurnSink contract.
use crate::proto::gait::log::v1::{event, payload, stream_item, Event, RunResult, StreamItem, run_result, TurnOutput};
use crate::proto::events;


#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum StreamClass { Trace, Error }

pub fn classify(item: &StreamItem) -> StreamClass {
    match &item.item {
        Some(stream_item::Item::Event(ev)) => classify_event(ev),
        _ => StreamClass::Trace, // Delta or empty
    }
}
fn classify_event(ev: &Event) -> StreamClass {
    match ev.body.as_ref() {
        Some(event::Body::Payload(p)) => match &p.kind {
            Some(payload::Kind::Error(_)) => StreamClass::Error,
            Some(payload::Kind::Raw(r)) if r.r#type == "workflow_error" || r.r#type == "node_error" => StreamClass::Error,
            _ => StreamClass::Trace,
        },
        _ => StreamClass::Trace,
    }
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Diag { Info, Error }

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

pub async fn drive(mut rx: tokio::sync::mpsc::UnboundedReceiver<StreamItem>, sink: &mut impl TurnSink) {
    while let Some(item) = rx.recv().await { sink.item(&item); }
}

fn extract_kind(body: event::Body) -> Option<payload::Kind> {
    match body { event::Body::Payload(p) => p.kind, _ => None }
}

pub fn run_result_proto(r: &crate::turn::TurnResult) -> RunResult {
    use crate::turn::TurnResult;
    let result = match r {
        TurnResult::Done { message, result, changes } => run_result::Result::Done(TurnOutput {
            message: message.clone(), result: events::to_value_opt(result), changes: changes.clone(),
        }),
        TurnResult::Clarify { items } => match extract_kind(events::clarify(items)) {
            Some(payload::Kind::Clarify(c)) => run_result::Result::Clarify(c),
            _ => run_result::Result::Clarify(Default::default()),
        },
        TurnResult::Inform { items } => match extract_kind(events::inform(items)) {
            Some(payload::Kind::Inform(i)) => run_result::Result::Inform(i),
            _ => run_result::Result::Inform(Default::default()),
        },
        TurnResult::Permissions => run_result::Result::Permissions(Default::default()),
    };
    RunResult { result: Some(result) }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::{events, wire};
    fn ev(body: event::Body) -> StreamItem {
        StreamItem { item: Some(stream_item::Item::Event(wire::body_to_event_for_test(body))) }
    }
    #[test]
    fn error_event_is_error() {
        assert_eq!(classify(&ev(events::error("boom".into(), "tool"))), StreamClass::Error);
    }
    #[test]
    fn tool_call_is_trace() {
        assert_eq!(classify(&ev(events::tool_call("c1".into(), "read".into(), &serde_json::json!({})))), StreamClass::Trace);
    }
    #[test]
    fn classify_routes_workflow_errors_to_error() {
        use crate::proto::gait::log::v1::{StreamItem, stream_item, Event, event, Payload, payload, RawEvent};
        fn raw_item(ty: &str) -> StreamItem {
            let body = event::Body::Payload(Payload {
                kind: Some(payload::Kind::Raw(RawEvent { r#type: ty.into(), payload: None })),
            });
            StreamItem { item: Some(stream_item::Item::Event(Event { body: Some(body), ..Default::default() })) }
        }
        assert_eq!(classify(&raw_item("workflow_error")), StreamClass::Error);
        assert_eq!(classify(&raw_item("node_error")), StreamClass::Error);
        // Non-error raw events are Trace.
        for ty in ["workflow_start", "node_start", "node_skip", "node_complete", "hook_run", "workflow_gate", "workflow_complete"] {
            assert_eq!(classify(&raw_item(ty)), StreamClass::Trace, "{ty} should be Trace");
        }
    }
    #[test]
    fn run_result_permissions_maps() {
        let v: serde_json::Value = serde_json::from_str(&serde_json::to_string(&run_result_proto(&crate::turn::TurnResult::Permissions)).unwrap()).unwrap();
        assert!(v.get("permissions").is_some());
    }
    #[test]
    fn run_result_done_serializes_with_changes() {
        use crate::proto::gait::log::v1::Changes;
        let r = crate::turn::TurnResult::Done { message: "ok".into(), result: None,
            changes: Some(Changes { added: vec!["a.rs".into()], modified: vec![], deleted: vec![] }) };
        let v: serde_json::Value = serde_json::from_str(&serde_json::to_string(&run_result_proto(&r)).unwrap()).unwrap();
        assert_eq!(v["done"]["changes"]["added"][0], "a.rs");
    }
}

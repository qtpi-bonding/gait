//! Agnostic turn-output port: classify channel items + the TurnSink contract.
use crate::proto::gait::log::v1::{event, payload, stream_item, Event, StreamItem};

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
}

pub async fn drive(mut rx: tokio::sync::mpsc::UnboundedReceiver<StreamItem>, sink: &mut impl TurnSink) {
    while let Some(item) = rx.recv().await { sink.item(&item); }
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
}

//! Domain events emitted by the initialized gait processes.

pub use concord_telemetry::{Principal, PrincipalKind, Verification};

pub mod gait {
    pub mod events {
        pub mod v1 {
            include!(concat!(env!("OUT_DIR"), "/gait.events.v1.rs"));
            include!(concat!(env!("OUT_DIR"), "/gait.events.v1.serde.rs"));
        }
    }
}

pub const DESCRIPTOR: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/events_descriptor.bin"));

#[cfg(test)]
mod tests {
    use super::gait::events::v1::{PromptAccepted, PromptSource};
    use concord_telemetry::testing::capture;

    #[test]
    fn prompt_accepted_round_trips_through_the_derive() {
        let id = format!("prompt-{}", std::process::id());
        let event = PromptAccepted {
            source_record_id: id.clone(),
            session_id: "session-1".into(),
            message_count: 2,
            message_ids: vec!["message-1".into(), "message-2".into()],
            source: PromptSource::Direct as i32,
            author: None,
        };
        let cap = capture();
        concord_telemetry::event!(event.clone());
        let got = cap.single_event_where::<PromptAccepted>(|value| value.source_record_id == id);
        assert_eq!(got.source_record_id, event.source_record_id);
        assert_eq!(got.session_id, event.session_id);
        assert_eq!(got.message_count, event.message_count);
        assert_eq!(got.message_ids, event.message_ids);
        assert_eq!(got.source, event.source);
        assert_eq!(got.author, event.author);
    }
}

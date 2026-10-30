#![cfg(feature = "telemetry")]

use gait_events::gait::events::v1::AgentSpawned;

#[test]
fn parented_span_gives_events_the_caller_trace_id() {
    let cap = concord_telemetry::testing::capture();
    let span = tracing::info_span!("t");
    concord_telemetry::propagation::set_parent(&span, |key| {
        (key == "traceparent")
            .then(|| "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01".to_string())
    });
    span.in_scope(|| {
        concord_telemetry::event!(AgentSpawned {
            source_record_id: "trace-join-child".to_owned(),
            session_id: "trace-join-parent".to_owned(),
            child_session_id: "trace-join-child".to_owned(),
            agent_name: "probe".to_owned(),
        });
    });
    let expected: [u8; 16] = [
        0x0a, 0xf7, 0x65, 0x19, 0x16, 0xcd, 0x43, 0xdd, 0x84, 0x48, 0xeb, 0x21, 0x1c, 0x80, 0x31,
        0x9c,
    ];
    assert_eq!(
        cap.trace_id_for(<AgentSpawned as concord_telemetry::Event>::TYPE_NAME),
        Some(expected)
    );
}

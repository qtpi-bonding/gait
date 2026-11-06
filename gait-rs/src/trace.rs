//! Spans around the turn's steps and tools, so a trace shows the gaps between
//! them and not one opaque prompt bar. Without the `telemetry` feature these
//! are no-ops.
use std::future::Future;

#[cfg(feature = "telemetry")]
pub(crate) type Span = tracing::Span;
#[cfg(not(feature = "telemetry"))]
pub(crate) struct Span;

/// The span of one model step: the request, its stream and the tool calls it triggers.
pub(crate) fn step_span(step: usize) -> Span {
    #[cfg(feature = "telemetry")]
    return tracing::info_span!("gait.step", step);
    #[cfg(not(feature = "telemetry"))]
    {
        let _ = step;
        Span
    }
}

/// The span of one tool call.
pub(crate) fn tool_span(tool: &str) -> Span {
    #[cfg(feature = "telemetry")]
    return tracing::info_span!("gait.tool", tool);
    #[cfg(not(feature = "telemetry"))]
    {
        let _ = tool;
        Span
    }
}

/// Runs `fut` inside `span`.
pub(crate) async fn within<F: Future>(span: Span, fut: F) -> F::Output {
    #[cfg(feature = "telemetry")]
    return tracing::Instrument::instrument(fut, span).await;
    #[cfg(not(feature = "telemetry"))]
    {
        let _ = span;
        fut.await
    }
}

/// Stamps a daemon's start record with the carrier of the turn starting it.
pub(crate) fn with_start_carrier(
    #[allow(unused_mut)] mut start: crate::proto::gait::log::v1::DaemonStart,
) -> crate::proto::gait::log::v1::DaemonStart {
    #[cfg(feature = "telemetry")]
    {
        let carrier = concord_telemetry::carrier::capture();
        start.traceparent = carrier.traceparent;
        start.tracestate = carrier.tracestate;
        start.baggage = carrier.baggage;
    }
    start
}

/// A short span for a daemon's wake, linked to the turn that started the
/// daemon. Closed as soon as it is made: it marks the wake and its cause.
pub(crate) fn note_daemon_wake(update: &crate::proto::gait::log::v1::DaemonUpdate) {
    #[cfg(feature = "telemetry")]
    {
        let span = tracing::info_span!(parent: None, "gait.daemon_update", daemon = %update.name);
        concord_telemetry::carrier::link_from(
            &span,
            &concord_telemetry::TraceCarrier {
                traceparent: update.traceparent.clone(),
                tracestate: update.tracestate.clone(),
                baggage: update.baggage.clone(),
            },
        );
    }
    #[cfg(not(feature = "telemetry"))]
    let _ = update;
}

/// [`note_daemon_wake`] for a completion body; other bodies are ignored.
pub(crate) fn note_daemon_wake_body(body: &crate::proto::gait::log::v1::event::Body) {
    use crate::proto::gait::log::v1::{event::Body, payload::Kind};
    if let Body::Payload(payload) = body {
        if let Some(Kind::DaemonUpdate(update)) = payload.kind.as_ref() {
            note_daemon_wake(update);
        }
    }
}

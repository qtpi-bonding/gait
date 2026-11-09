//! Spans around the turn's steps and tools, so a trace shows the gaps between
//! them and not one opaque prompt bar. Without the `telemetry` feature these
//! are no-ops.
use std::future::Future;

#[cfg(feature = "telemetry")]
pub(crate) type Span = tracing::Span;
#[cfg(not(feature = "telemetry"))]
pub(crate) struct Span;

/// The span of one turn in the spine: the brain's and the tools' spans hang
/// below it. `turn_id` and `model` are recorded once the turn has begun.
pub(crate) fn turn_span(session_id: &str) -> Span {
    #[cfg(feature = "telemetry")]
    return tracing::info_span!(
        "gait.turn",
        session_id = %session_id,
        turn_id = tracing::field::Empty,
        model = tracing::field::Empty,
    );
    #[cfg(not(feature = "telemetry"))]
    {
        let _ = session_id;
        Span
    }
}

/// The id of this turn: the `turn_id` the upstream put in the incoming
/// baggage (structmux/multimatrix mint it), else `session_id:started_ms`.
pub(crate) fn turn_id(session_id: &str, started_ms: u64) -> String {
    #[cfg(feature = "telemetry")]
    if let Some(id) = concord_telemetry::carrier::capture()
        .baggage
        .as_deref()
        .and_then(turn_id_from_baggage)
    {
        return id;
    }
    format!("{session_id}:{started_ms}")
}

/// `turn_id` from a W3C baggage header value (`k=v,k2=v2;prop`).
pub(crate) fn turn_id_from_baggage(baggage: &str) -> Option<String> {
    baggage.split(',').find_map(|member| {
        let (key, rest) = member.trim().split_once('=')?;
        let value = rest.split(';').next()?.trim();
        (key.trim() == "turn_id" && !value.is_empty()).then(|| value.to_owned())
    })
}

/// Records the turn's id and model on the current `gait.turn` span.
pub(crate) fn note_turn(turn_id: &str, model: &str) {
    #[cfg(feature = "telemetry")]
    {
        let span = tracing::Span::current();
        span.record("turn_id", turn_id);
        span.record("model", model);
    }
    #[cfg(not(feature = "telemetry"))]
    let _ = (turn_id, model);
}

/// The root span of the sealed brain's turn: the spine's trace continues here
/// through `TRACEPARENT`/`BAGGAGE`, and every `gait.step` below it exports under
/// the spine's turn.
pub(crate) fn brain_span(session_id: &str) -> Span {
    #[cfg(feature = "telemetry")]
    {
        let span = tracing::info_span!("gait.brain", session_id = %session_id);
        concord_telemetry::propagation::set_parent_from_env(&span);
        return span;
    }
    #[cfg(not(feature = "telemetry"))]
    {
        let _ = session_id;
        Span
    }
}

/// Ends the process after exporting what is buffered. `std::process::exit`
/// skips destructors, so the telemetry guard would never flush, and the spine
/// kills the brain's process group moments after it sees `Terminated`.
pub(crate) fn exit(code: i32) -> ! {
    crate::telemetry::flush();
    std::process::exit(code)
}

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

#[cfg(test)]
mod tests {
    use super::turn_id_from_baggage;

    #[test]
    fn the_turn_id_comes_out_of_a_baggage_header() {
        assert_eq!(
            turn_id_from_baggage("action_id=a1, turn_id=t-9;prop=1,session_id=s").as_deref(),
            Some("t-9")
        );
        assert_eq!(turn_id_from_baggage("action_id=a1"), None);
        assert_eq!(turn_id_from_baggage("turn_id="), None);
        assert_eq!(turn_id_from_baggage(""), None);
    }
}

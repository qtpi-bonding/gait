//! Spans around the turn's steps and tools, so a trace shows the gaps between
//! them and not one opaque prompt bar. Without the `telemetry` feature these
//! are no-ops.
use std::future::Future;

#[cfg(feature = "telemetry")]
pub(crate) type Span = tracing::Span;
#[cfg(not(feature = "telemetry"))]
#[derive(Clone)]
pub(crate) struct Span;

/// The span of one turn in the spine: the brain's and the tools' spans hang
/// below it. `turn_id` and `model` are recorded once the turn has begun.
pub(crate) fn turn_span(session_id: &str) -> Span {
    #[cfg(feature = "telemetry")]
    return tracing::info_span!(
        "gait.turn",
        gait_session_id = %session_id,
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

#[cfg(feature = "telemetry")]
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

/// How the spine tells the brain which turn it is running: the id the spine
/// already put on `gait.turn`, so both processes name the turn the same way.
pub(crate) const TURN_ID_ENV: &str = "GAIT_TURN_ID";

#[cfg(feature = "telemetry")]
/// A process-wide counter for ids of events that can repeat within a turn.
pub(crate) fn next_seq() -> u64 {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// The session and turn this brain process works on. The brain runs exactly one
/// turn, so a process-wide value is exact; set when the root span is made.
static BRAIN_IDS: std::sync::OnceLock<(String, String)> = std::sync::OnceLock::new();

#[cfg(feature = "telemetry")]
/// (session_id, turn_id) of the turn this brain runs, once it has started.
pub(crate) fn brain_ids() -> (String, String) {
    BRAIN_IDS.get().cloned().unwrap_or_default()
}

/// The root span of the sealed brain's turn: the spine's trace continues here
/// through `TRACEPARENT`/`BAGGAGE`, and every `gait.step` below it exports under
/// the spine's turn.
pub(crate) fn brain_span(session_id: &str) -> Span {
    BRAIN_IDS.get_or_init(|| {
        (session_id.to_owned(), std::env::var(TURN_ID_ENV).unwrap_or_default())
    });
    #[cfg(feature = "telemetry")]
    {
        let span = tracing::info_span!(
            "gait.brain",
            gait_session_id = %session_id,
            turn_id = %std::env::var(TURN_ID_ENV).unwrap_or_default(),
            error.code = tracing::field::Empty,
            error.class = tracing::field::Empty,
        );
        concord_telemetry::propagation::set_parent_from_env(&span);
        span
    }
    #[cfg(not(feature = "telemetry"))]
    {
        let _ = session_id;
        Span
    }
}

/// The root span of a hand process: it continues the spine's `gait.tool` span
/// through `TRACEPARENT`/`BAGGAGE`. It is exported only when the hand's seal
/// lets the process reach the OTLP endpoint (the session's network policy).
pub(crate) fn hand_span(tool: &str, session_id: &str) -> Span {
    #[cfg(feature = "telemetry")]
    {
        let span = tracing::info_span!(
            "gait.hand",
            tool,
            gait_session_id = %session_id,
            error.code = tracing::field::Empty,
            error.class = tracing::field::Empty,
        );
        concord_telemetry::propagation::set_parent_from_env(&span);
        span
    }
    #[cfg(not(feature = "telemetry"))]
    {
        let _ = (tool, session_id);
        Span
    }
}

/// Records the code of `error` on `span` (`error.code`, `error.class`).
pub(crate) fn record_error_code(span: &Span, error: &anyhow::Error) {
    #[cfg(feature = "telemetry")]
    {
        let facts = coded_error::facts_anyhow(error);
        span.record("error.code", facts.code);
        span.record("error.class", facts.class.as_str());
    }
    #[cfg(not(feature = "telemetry"))]
    let _ = (span, error);
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

/// The span of one tool call. `ok` and `error.code` are recorded when it ends.
pub(crate) fn tool_span(tool: &str, session_id: &str, turn_id: &str) -> Span {
    #[cfg(feature = "telemetry")]
    return tracing::info_span!(
        "gait.tool",
        tool,
        gait_session_id = %session_id,
        turn_id = %turn_id,
        ok = tracing::field::Empty,
        error.code = tracing::field::Empty,
    );
    #[cfg(not(feature = "telemetry"))]
    {
        let _ = (tool, session_id, turn_id);
        Span
    }
}

/// How a tool call ended, from what the spine can see of its feedback: a
/// permission denial, a sandbox denial, or a tool-reported error. A command
/// that ran and exited non-zero is an ordinary result, not a failure here.
pub(crate) fn tool_outcome_code(feedback: &gait_proto::gait::sandbox::v1::Feedback) -> Option<&'static str> {
    if feedback.denied.is_some() {
        Some("gait.tool.permission_denied")
    } else if feedback.sandbox_denied.is_some() {
        Some("gait.tool.sandbox_denied")
    } else if feedback.is_error == Some(true) {
        Some("gait.tool.failed")
    } else {
        None
    }
}

/// Records the outcome on the current `gait.tool` span.
pub(crate) fn note_tool_outcome(code: Option<&str>) {
    #[cfg(feature = "telemetry")]
    {
        let span = tracing::Span::current();
        span.record("ok", code.is_none());
        if let Some(code) = code {
            span.record("error.code", code);
        }
    }
    #[cfg(not(feature = "telemetry"))]
    let _ = code;
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
    use super::tool_outcome_code;
    #[cfg(feature = "telemetry")]
    use super::turn_id_from_baggage;
    use gait_proto::gait::sandbox::v1::Feedback;

    #[test]
    fn a_tool_outcome_names_the_denial_or_error() {
        assert_eq!(tool_outcome_code(&Feedback::default()), None);
        let sandbox = Feedback { sandbox_denied: Some(Default::default()), ..Default::default() };
        assert_eq!(tool_outcome_code(&sandbox), Some("gait.tool.sandbox_denied"));
        let failed = Feedback { is_error: Some(true), ..Default::default() };
        assert_eq!(tool_outcome_code(&failed), Some("gait.tool.failed"));
        let denied = Feedback { denied: Some(Default::default()), ..Default::default() };
        assert_eq!(tool_outcome_code(&denied), Some("gait.tool.permission_denied"));
    }

    #[cfg(feature = "telemetry")]
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

    #[cfg(feature = "telemetry")]
    #[test]
    #[serial_test::serial]
    fn the_hand_span_continues_the_trace_in_its_environment() {
        let _capture = concord_telemetry::testing::capture_exclusive();
        std::env::set_var("TRACEPARENT", "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01");
        let span = super::hand_span("shell", "s-hand");
        std::env::remove_var("TRACEPARENT");
        super::record_error_code(&span, &anyhow::anyhow!("plain"));
        drop(span);
        let finished = concord_telemetry::testing::spans()
            .into_iter()
            .rev()
            .find(|s| s.name == "gait.hand")
            .expect("the hand span finished");
        assert_eq!(finished.span_context.trace_id().to_string(), "0af7651916cd43dd8448eb211c80319c");
        assert_eq!(finished.parent_span_id.to_string(), "b7ad6b7169203331");
        assert!(finished.attributes.iter().any(|kv| kv.key.as_str() == "error.code"));
    }
}

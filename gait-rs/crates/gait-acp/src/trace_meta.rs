//! The trace carrier in ACP `_meta`, and the spans gait-acp opens around it.
//!
//! The carrier travels under the W3C key names `traceparent`, `tracestate` and
//! `baggage`. A request gait is asked to do (prompt, cancel, load) makes its
//! span a child of the carrier. An answer to something gait asked (a
//! permission or elicitation reply) gets a span of its own with a link, so the
//! turn that asked is not re-rooted under the client's trace.
use concord_telemetry::{TraceCarrier, carrier};
use tracing::Span;

/// The carrier in a `_meta` object. Missing or non-string keys read as absent.
pub fn carrier_from_meta(meta: Option<&serde_json::Map<String, serde_json::Value>>) -> TraceCarrier {
    TraceCarrier::from_pairs(|key| {
        meta.and_then(|values| values.get(key))
            .and_then(|value| value.as_str())
            .map(str::to_owned)
    })
}

/// A span for an ACP request that changes or reads session state. It is parented
/// on the client's carrier when `_meta` has one and is a root span when not: a
/// missing carrier on these requests is ordinary, not a break in a trace.
pub fn handler_span(span: Span, meta: Option<&serde_json::Map<String, serde_json::Value>>) -> Span {
    carrier::parent_on(&span, &carrier_from_meta(meta));
    span
}

/// Opens the span for an answer gait receives to its own request, linked to the
/// answerer's carrier and not parented on it.
pub fn answer_span(kind: &'static str, meta: Option<&serde_json::Map<String, serde_json::Value>>) -> Span {
    let span = tracing::info_span!(parent: None, "acp.answer", answer.kind = kind);
    let answerer = carrier_from_meta(meta);
    carrier::link_from(&span, &answerer);
    // The link no longer adopts the answerer's labels: add them.
    gait_core::telemetry::adopt_labels(&span, answerer.baggage.as_deref());
    span
}

/// Records on the running `acp.prompt` that a cancel was requested, linked to
/// the `acp.cancel` span that received it, so the cancel's trace connects to
/// its outcome.
pub fn note_cancel_requested(prompt: &Span, cancel: &Span) {
    // Link on the trace only: the cancel's baggage (its own request_id,
    // action_id) would replace the prompt span's labels and be stamped beside
    // them.
    let requested = TraceCarrier {
        baggage: None,
        ..cancel.in_scope(carrier::capture)
    };
    carrier::link_from(prompt, &requested);
    prompt.in_scope(|| tracing::info!("cancel_requested"));
}

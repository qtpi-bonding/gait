//! What a model call reports to telemetry: one `ModelCallFailed` per failed
//! attempt, so a monitor sees quota pressure (429s that retries absorb) before
//! turns start failing. The caller says whose call it is with [`with_call`];
//! the transport does not know the session, turn or model.
use std::future::Future;

use crate::provider::ProviderError;
#[cfg(feature = "telemetry")]
use crate::provider::TimeoutPhase;

/// Who a model call belongs to.
#[derive(Clone, Debug, Default)]
pub struct CallContext {
    pub session_id: String,
    pub turn_id: String,
    /// "provider/model-id"
    pub model: String,
    /// The inference profile name this call runs under ("" when unnamed).
    pub inference: String,
    /// The configured reasoning effort ("" when unset).
    pub reasoning_effort: String,
}

tokio::task_local! {
    static CALL: CallContext;
}

/// Counts failures in this process, so two steps of one turn that both fail at
/// attempt 1 still get distinct event ids.
#[cfg(feature = "telemetry")]
static FAILURES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Runs `fut` with `ctx` as the owner of every model call made inside it.
pub async fn with_call<F: Future>(ctx: CallContext, fut: F) -> F::Output {
    CALL.scope(ctx, fut).await
}

#[cfg(feature = "telemetry")]
pub(crate) fn report_failed(error: &ProviderError, attempt: u32, max_attempts: u32, will_retry: bool) {
    use gait_events::gait::events::v1::{ModelCallFailed, ModelFailureKind as K};
    let ctx = CALL.try_with(Clone::clone).unwrap_or_default();
    let (kind, http_status, request_id) = match error {
        ProviderError::Http { status, request_id, .. } => {
            (K::HttpStatus, u32::from(*status), request_id.clone().unwrap_or_default())
        }
        ProviderError::Timeout { phase } => (
            match phase {
                TimeoutPhase::Connect => K::TimeoutConnect,
                TimeoutPhase::Ttfb | TimeoutPhase::Request => K::TimeoutTtfb,
                TimeoutPhase::Idle => K::TimeoutIdle,
                TimeoutPhase::Absolute => K::TimeoutAbsolute,
            },
            0,
            String::new(),
        ),
        ProviderError::Overloaded => (K::Overloaded, 0, String::new()),
        _ => (K::Transport, 0, String::new()),
    };
    concord_telemetry::event!(ModelCallFailed {
        source_record_id: format!(
            "{}:{}:{}",
            ctx.session_id,
            ctx.turn_id,
            FAILURES.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ),
        gait_session_id: ctx.session_id,
        turn_id: ctx.turn_id,
        model: ctx.model,
        attempt,
        attempt_max: max_attempts,
        kind: kind as i32,
        http_status,
        retry_after_ms: error.retry_after().map_or(0, |d| d.as_millis() as u64),
        provider_request_id: request_id,
        will_retry,
    });
}

/// The span of one model call attempt, a child of the step that made it.
#[cfg(feature = "telemetry")]
pub(crate) type CallSpan = tracing::Span;
#[cfg(not(feature = "telemetry"))]
#[derive(Clone)]
pub(crate) struct CallSpan;

pub(crate) fn call_span(attempt: u32) -> CallSpan {
    #[cfg(feature = "telemetry")]
    {
        let model = CALL.try_with(|ctx| ctx.model.clone()).unwrap_or_default();
        tracing::info_span!(
            "gait.provider.call",
            model,
            attempt,
            http_status = tracing::field::Empty,
            error.code = tracing::field::Empty,
            error.class = tracing::field::Empty,
        )
    }
    #[cfg(not(feature = "telemetry"))]
    {
        let _ = attempt;
        CallSpan
    }
}

/// What the request asked for, for the call span.
#[derive(Clone, Debug, Default)]
#[cfg_attr(not(feature = "telemetry"), allow(dead_code))]
pub(crate) struct RequestAttrs {
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub max_tokens: Option<u32>,
    /// The provider's own model id, used when no `CallContext` names one.
    pub model_id: String,
}

/// Token counts of one call. `input` is the whole prompt (cached included, as
/// OTel's `gen_ai.usage.input_tokens` is); `reasoning` is a subset of `output`.
#[derive(Clone, Copy, Debug, Default)]
#[cfg_attr(not(feature = "telemetry"), allow(dead_code))]
pub(crate) struct CallUsage {
    pub input: u32,
    pub output: u32,
    pub cached: u32,
    pub cache_write: u32,
    pub reasoning: u32,
}

/// The `gait.model_call` span of one model call: opened before the first
/// attempt, it covers every retry and the whole stream body, and ends (with
/// duration, time to first token and tokens per second computed here from the
/// stream) when the call finishes or the stream is dropped. Attributes follow
/// the OTel gen_ai conventions (`gen_ai.provider.name`, not the older
/// `gen_ai.system`); gait's own measurements are `gait.*`.
#[cfg(feature = "telemetry")]
pub(crate) struct ModelCall {
    span: tracing::Span,
    start: std::time::Instant,
    first_token: Option<std::time::Instant>,
    output_tokens: u32,
    /// When the answer finished: the wait for a trailing usage chunk is not generation time.
    answered: Option<std::time::Instant>,
    ended: bool,
}
#[cfg(not(feature = "telemetry"))]
pub(crate) struct ModelCall;

impl ModelCall {
    pub(crate) fn begin(request: &RequestAttrs) -> Self {
        #[cfg(feature = "telemetry")]
        {
            let ctx = CALL.try_with(Clone::clone).unwrap_or_default();
            let (provider, model) = match ctx.model.split_once('/') {
                Some((p, m)) => (p.to_owned(), m.to_owned()),
                None => (String::new(), request.model_id.clone()),
            };
            // f32 -> f64 through its decimal form, so 0.9 stays 0.9.
            let float = |v: Option<f32>| v.and_then(|v| v.to_string().parse::<f64>().ok());
            let span = tracing::info_span!(
                "gait.model_call",
                gen_ai.operation.name = "chat",
                gen_ai.provider.name = %provider,
                gen_ai.request.model = %model,
                gen_ai.request.temperature = float(request.temperature),
                gen_ai.request.top_p = float(request.top_p),
                gen_ai.request.max_tokens = request.max_tokens.map(u64::from),
                gait.inference = %ctx.inference,
                gait.reasoning_effort = %ctx.reasoning_effort,
                gen_ai.response.model = tracing::field::Empty,
                gen_ai.response.finish_reasons = tracing::field::Empty,
                gen_ai.usage.input_tokens = tracing::field::Empty,
                gen_ai.usage.output_tokens = tracing::field::Empty,
                gait.usage.cached_tokens = tracing::field::Empty,
                gait.usage.reasoning_tokens = tracing::field::Empty,
                gait.usage.cache_write_tokens = tracing::field::Empty,
                gait.time_to_first_token_ms = tracing::field::Empty,
                gait.duration_ms = tracing::field::Empty,
                gait.output_tokens_per_second = tracing::field::Empty,
                gait.server.time_to_first_token_ms = tracing::field::Empty,
                gait.server.generation_tokens_per_second = tracing::field::Empty,
                http_status = tracing::field::Empty,
                error.type = tracing::field::Empty,
                error.code = tracing::field::Empty,
                error.class = tracing::field::Empty,
            );
            Self { span, start: std::time::Instant::now(), first_token: None, output_tokens: 0, answered: None, ended: false }
        }
        #[cfg(not(feature = "telemetry"))]
        {
            let _ = request;
            Self
        }
    }

    /// The span, so the transport's per-attempt spans become its children.
    pub(crate) fn span(&self) -> CallSpan {
        #[cfg(feature = "telemetry")]
        return self.span.clone();
        #[cfg(not(feature = "telemetry"))]
        CallSpan
    }

    /// The first visible output (text, reasoning or tool call) arrived.
    pub(crate) fn first_token(&mut self) {
        #[cfg(feature = "telemetry")]
        if self.first_token.is_none() {
            let now = std::time::Instant::now();
            self.first_token = Some(now);
            self.span.record("gait.time_to_first_token_ms", millis(now - self.start));
        }
    }

    /// The answer is complete (finish chunk or `[DONE]`); timing stops here.
    pub(crate) fn answered(&mut self) {
        #[cfg(feature = "telemetry")]
        if self.answered.is_none() {
            self.answered = Some(std::time::Instant::now());
        }
    }

    pub(crate) fn response_model(&mut self, model: &str) {
        #[cfg(feature = "telemetry")]
        if !model.is_empty() {
            self.span.record("gen_ai.response.model", model);
        }
        #[cfg(not(feature = "telemetry"))]
        let _ = model;
    }

    pub(crate) fn finish_reason(&mut self, reason: &str) {
        #[cfg(feature = "telemetry")]
        self.span.record("gen_ai.response.finish_reasons", reason);
        #[cfg(not(feature = "telemetry"))]
        let _ = reason;
    }

    pub(crate) fn usage(&mut self, usage: CallUsage) {
        #[cfg(feature = "telemetry")]
        {
            self.output_tokens = usage.output;
            self.span.record("gen_ai.usage.input_tokens", u64::from(usage.input));
            self.span.record("gen_ai.usage.output_tokens", u64::from(usage.output));
            self.span.record("gait.usage.cached_tokens", u64::from(usage.cached));
            self.span.record("gait.usage.reasoning_tokens", u64::from(usage.reasoning));
            self.span.record("gait.usage.cache_write_tokens", u64::from(usage.cache_write));
        }
        #[cfg(not(feature = "telemetry"))]
        let _ = usage;
    }

    /// What the server itself reported, recorded next to gait's own numbers.
    pub(crate) fn server_stats(&mut self, ttft_ms: Option<f64>, tokens_per_second: Option<f64>) {
        #[cfg(feature = "telemetry")]
        {
            if let Some(v) = ttft_ms {
                self.span.record("gait.server.time_to_first_token_ms", v);
            }
            if let Some(v) = tokens_per_second {
                self.span.record("gait.server.generation_tokens_per_second", v);
            }
        }
        #[cfg(not(feature = "telemetry"))]
        let _ = (ttft_ms, tokens_per_second);
    }

    pub(crate) fn fail(&mut self, error: &ProviderError) {
        #[cfg(feature = "telemetry")]
        {
            use coded_error::CodedError;
            self.span.record("error.type", error.code());
            record_failure(&self.span, error);
        }
        #[cfg(not(feature = "telemetry"))]
        let _ = error;
    }

    /// Records the timing and ends the span; also runs when the stream is dropped.
    pub(crate) fn end(&mut self) {
        #[cfg(feature = "telemetry")]
        if !self.ended {
            self.ended = true;
            let now = self.answered.unwrap_or_else(std::time::Instant::now);
            self.span.record("gait.duration_ms", millis(now - self.start));
            if let Some(first) = self.first_token {
                let generating = (now - first).as_secs_f64();
                if self.output_tokens > 0 && generating > 0.0 {
                    self.span.record("gait.output_tokens_per_second", f64::from(self.output_tokens) / generating);
                }
            }
        }
    }
}

#[cfg(feature = "telemetry")]
fn millis(d: std::time::Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

impl Drop for ModelCall {
    fn drop(&mut self) {
        self.end();
    }
}

/// Runs `fut` inside `span`.
pub(crate) async fn within<F: Future>(span: CallSpan, fut: F) -> F::Output {
    #[cfg(feature = "telemetry")]
    return tracing::Instrument::instrument(fut, span).await;
    #[cfg(not(feature = "telemetry"))]
    {
        let _ = span;
        fut.await
    }
}

/// Records a failed attempt's code (and HTTP status) on its span.
pub(crate) fn record_failure(span: &CallSpan, error: &ProviderError) {
    #[cfg(feature = "telemetry")]
    {
        use coded_error::CodedError;
        span.record("error.code", error.code());
        span.record("error.class", error.class().as_str());
        if let ProviderError::Http { status, .. } = error {
            span.record("http_status", u64::from(*status));
        }
    }
    #[cfg(not(feature = "telemetry"))]
    let _ = (span, error);
}

#[cfg(not(feature = "telemetry"))]
pub(crate) fn report_failed(_: &ProviderError, _: u32, _: u32, _: bool) {}

#[cfg(all(test, feature = "telemetry"))]
mod tests {
    use super::*;
    use gait_events::gait::events::v1::{ModelCallFailed, ModelFailureKind as K};

    #[tokio::test]
    async fn a_429_reports_status_retry_after_and_who_it_belongs_to() {
        let capture = concord_telemetry::testing::capture_exclusive();
        let error = ProviderError::Http {
            status: 429,
            body: "slow down".into(),
            retry_after: Some(std::time::Duration::from_secs(7)),
            request_id: Some("req_1".into()),
        };
        with_call(
            CallContext { session_id: "s-429".into(), turn_id: "t1".into(), model: "openai/gpt-x".into(), ..Default::default() },
            async { report_failed(&error, 1, 3, true) },
        )
        .await;
        let event = capture.single_event_where::<ModelCallFailed>(|e| e.gait_session_id == "s-429");
        assert_eq!(event.kind, K::HttpStatus as i32);
        assert_eq!((event.http_status, event.retry_after_ms), (429, 7000));
        assert_eq!((event.attempt, event.attempt_max, event.will_retry), (1, 3, true));
        assert_eq!((event.provider_request_id.as_str(), event.model.as_str()), ("req_1", "openai/gpt-x"));
    }

    #[tokio::test]
    async fn two_failures_at_the_same_attempt_get_distinct_ids() {
        let capture = concord_telemetry::testing::capture_exclusive();
        let ctx = CallContext {
            session_id: "s-distinct".into(),
            turn_id: "t".into(),
            model: "m".into(),
            ..Default::default()
        };
        with_call(ctx, async {
            report_failed(&ProviderError::Overloaded, 1, 3, true);
            report_failed(&ProviderError::Overloaded, 1, 3, true);
        })
        .await;
        let ids: std::collections::HashSet<_> = capture
            .events_where::<ModelCallFailed>(|e| e.gait_session_id == "s-distinct")
            .into_iter()
            .map(|e| e.source_record_id)
            .collect();
        assert_eq!(ids.len(), 2, "a monitor that dedupes on the id must see both");
    }

    #[test]
    fn a_timeout_maps_to_its_phase() {
        let capture = concord_telemetry::testing::capture_exclusive();
        report_failed(&ProviderError::Timeout { phase: TimeoutPhase::Ttfb }, 3, 3, false);
        let event = capture.single_event_where::<ModelCallFailed>(|e| e.attempt == 3 && e.gait_session_id.is_empty());
        assert_eq!(event.kind, K::TimeoutTtfb as i32);
        assert!(!event.will_retry);
    }
}

#[cfg(all(test, feature = "telemetry"))]
mod call_span_tests {
    use super::*;

    #[test]
    fn a_failed_attempt_records_its_code_and_status_on_the_span() {
        let _capture = concord_telemetry::testing::capture_exclusive();
        let span = call_span(2);
        record_failure(
            &span,
            &ProviderError::Http { status: 429, body: String::new(), retry_after: None, request_id: None },
        );
        drop(span);
        let finished = concord_telemetry::testing::spans()
            .into_iter()
            .rev()
            .find(|s| s.name == "gait.provider.call")
            .expect("the call span finished");
        let value = |key: &str| {
            finished.attributes.iter().find(|kv| kv.key.as_str() == key).map(|kv| kv.value.to_string())
        };
        assert_eq!(value("attempt").as_deref(), Some("2"));
        assert_eq!(value("http_status").as_deref(), Some("429"));
        assert_eq!(value("error.code").as_deref(), Some("gait.provider.http"));
    }
}

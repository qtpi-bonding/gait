//! Transport-layer knobs and helpers shared by every LLM provider.
//!
//! Defaults are the production policy; tests can override individual fields by
//! struct literal. Provider adapters receive the transport as a required
//! dependency and cannot access its raw HTTP client.

use crate::provider::{ProviderError, TimeoutPhase};
use eventsource_stream::Eventsource;
use futures_util::{Stream, StreamExt};
use reqwest::header::{HeaderMap, HeaderValue};
use serde::de::DeserializeOwned;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;
const MAX_JSON_RESPONSE_BYTES: usize = 64 * 1024;

/// A complete, replayable JSON-over-HTTP request for an SSE endpoint.
///
/// Adapters own the URL, headers, and provider-specific body. The transport
/// owns request execution and may clone this value only before the first SSE
/// event has been consumed.
#[derive(Clone)]
pub struct SseRequest {
    pub url: String,
    pub headers: HeaderMap,
    pub body: serde_json::Value,
}

/// A one-shot JSON-over-HTTP request.
///
/// The request body, URL, and headers are provider-specific. The transport
/// owns execution, response limits, timeouts, and error handling.
#[derive(Clone)]
pub struct JsonRequest {
    pub url: String,
    pub headers: HeaderMap,
    pub body: serde_json::Value,
}

impl JsonRequest {
    pub fn new(url: String, headers: HeaderMap, body: serde_json::Value) -> Self {
        Self { url, headers, body }
    }
}

impl SseRequest {
    pub fn new(url: String, headers: HeaderMap, body: serde_json::Value) -> Self {
        Self { url, headers, body }
    }
}

/// Mark a credential-bearing header value as sensitive before placing it in an
/// [`SseRequest`] or [`JsonRequest`]. Sensitive values are redacted from
/// bounded HTTP error bodies and reqwest's header debug formatting.
pub fn sensitive_header(value: &str) -> Result<HeaderValue, ProviderError> {
    let mut value = HeaderValue::from_str(value)
        .map_err(|error| ProviderError::Other(format!("invalid sensitive header: {error}")))?;
    value.set_sensitive(true);
    Ok(value)
}

pub type GuardedSseStream =
    Pin<Box<dyn Stream<Item = Result<eventsource_stream::Event, ProviderError>> + Send>>;

type RawSseStream = Pin<
    Box<
        dyn Stream<
                Item = Result<
                    eventsource_stream::Event,
                    eventsource_stream::EventStreamError<reqwest::Error>,
                >,
            > + Send,
    >,
>;

/// Mandatory HTTP execution boundary shared by every production adapter.
#[derive(Clone)]
pub struct ProviderTransport {
    http: reqwest::Client,
    config: TransportConfig,
}

impl ProviderTransport {
    pub fn new(config: TransportConfig) -> Result<Self, ProviderError> {
        let http = reqwest::Client::builder()
            .connect_timeout(config.connect_timeout)
            .build()
            .map_err(ProviderError::from_reqwest)?;
        Ok(Self { http, config })
    }

    pub fn config(&self) -> TransportConfig {
        self.config
    }

    /// Execute one JSON request. Unlike SSE streams, JSON requests are never
    /// retried; the configured deadline covers sending the request and reading
    /// the complete response body.
    pub async fn post_json<T: DeserializeOwned>(
        &self,
        request: JsonRequest,
    ) -> Result<T, ProviderError> {
        tokio::time::timeout(self.config.json_timeout, self.post_json_inner(request))
            .await
            .map_err(|_| ProviderError::Timeout {
                phase: TimeoutPhase::Request,
            })?
    }

    async fn post_json_inner<T: DeserializeOwned>(
        &self,
        request: JsonRequest,
    ) -> Result<T, ProviderError> {
        let secrets = sensitive_values(&request.headers);
        let response = self
            .http
            .post(&request.url)
            .headers(request.headers)
            .json(&request.body)
            .send()
            .await
            .map_err(ProviderError::from_reqwest)?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let retry_after = parse_retry_after(&response);
            let body = bounded_error_body(response).await;
            return Err(ProviderError::Http {
                status,
                body: truncate_utf8(redact_secrets(body, &secrets), MAX_ERROR_BODY_BYTES),
                retry_after,
            });
        }

        let body = bounded_success_body(response, MAX_JSON_RESPONSE_BYTES).await?;
        serde_json::from_slice(&body).map_err(ProviderError::Json)
    }

    /// Execute a replayable request and return a stream that has already crossed
    /// the first-event retry boundary and is guarded by idle/absolute deadlines.
    pub async fn stream(&self, request: SseRequest) -> Result<GuardedSseStream, ProviderError> {
        let transport = self.clone();
        let request_template = request.clone();
        let cancel = CancellationToken::new();
        let stream = connect_and_first_event(self.config, &cancel, move || {
            let transport = transport.clone();
            let request = request_template.clone();
            async move { transport.open(request).await }
        })
        .await?;
        Ok(Box::pin(stream))
    }

    async fn open(&self, request: SseRequest) -> Result<RawSseStream, ProviderError> {
        let secrets = sensitive_values(&request.headers);
        let response = self
            .http
            .post(&request.url)
            .headers(request.headers)
            .json(&request.body)
            .send()
            .await
            .map_err(ProviderError::from_reqwest)?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let retry_after = parse_retry_after(&response);
            let body = bounded_error_body(response).await;
            return Err(ProviderError::Http {
                status,
                body: truncate_utf8(redact_secrets(body, &secrets), MAX_ERROR_BODY_BYTES),
                retry_after,
            });
        }

        Ok(Box::pin(response.bytes_stream().eventsource()))
    }
}

fn sensitive_values(headers: &HeaderMap) -> Vec<String> {
    headers
        .values()
        .filter(|value| value.is_sensitive())
        .filter_map(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
        .flat_map(|value| {
            let mut values = vec![value.to_owned()];
            if let Some(token) = value
                .strip_prefix("Bearer ")
                .filter(|token| !token.is_empty())
            {
                values.push(token.to_owned());
            }
            values
        })
        .collect()
}

fn redact_secrets(mut body: String, secrets: &[String]) -> String {
    for secret in secrets {
        body = body.replace(secret, "[REDACTED]");
    }
    body
}

fn truncate_utf8(mut value: String, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value;
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
    value
}

async fn bounded_error_body(response: reqwest::Response) -> String {
    let mut bytes = Vec::with_capacity(MAX_ERROR_BODY_BYTES.min(4096));
    let mut chunks = response.bytes_stream();
    while bytes.len() < MAX_ERROR_BODY_BYTES {
        let Some(chunk) = chunks.next().await else {
            break;
        };
        let Ok(chunk) = chunk else {
            break;
        };
        let remaining = MAX_ERROR_BODY_BYTES - bytes.len();
        bytes.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

async fn bounded_success_body(
    response: reqwest::Response,
    max_bytes: usize,
) -> Result<Vec<u8>, ProviderError> {
    let mut bytes = Vec::with_capacity(max_bytes.min(4096));
    let mut chunks = response.bytes_stream();
    while let Some(chunk) = chunks.next().await {
        let chunk = chunk.map_err(ProviderError::from_reqwest)?;
        if bytes.len().saturating_add(chunk.len()) > max_bytes {
            return Err(ProviderError::ResponseTooLarge { max_bytes });
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

/// All knobs the LLM path cares about. `Default` *is* the production policy.
#[derive(Debug, Clone, Copy)]
pub struct TransportConfig {
    /// Time budget for the TCP/TLS handshake (no body yet).
    pub connect_timeout: Duration,
    /// Time budget for the first byte after the request has been sent.
    pub ttfb_timeout: Duration,
    /// Time budget between successive SSE events on a healthy stream.
    pub idle_timeout: Duration,
    /// Hard ceiling on the whole stream lifecycle, regardless of activity.
    pub absolute_cap: Duration,
    /// Hard ceiling for one-shot JSON requests, including response-body reads.
    pub json_timeout: Duration,
    /// Total attempts (1 = no retry).
    pub max_attempts: u8,
    /// Initial exponential-backoff window before jitter is applied.
    pub initial_backoff: Duration,
    /// Backoff never exceeds this value.
    pub max_backoff: Duration,
}

impl Default for TransportConfig {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(10),
            ttfb_timeout: Duration::from_secs(30),
            idle_timeout: Duration::from_secs(120),
            absolute_cap: Duration::from_secs(600),
            json_timeout: Duration::from_secs(30),
            max_attempts: 3,
            initial_backoff: Duration::from_millis(500),
            max_backoff: Duration::from_secs(8),
        }
    }
}

/// Full-jitter exponential backoff: `sleep ∈ [0, min(max, initial * 2^attempt)]`.
///
/// `rand::random` is seeded by `getrandom` per-process, so parallel brain processes
/// produce decorrelated jitter — important when several sub-agents retry the same
/// endpoint simultaneously.
pub(crate) fn backoff_delay(cfg: &TransportConfig, attempt: u32) -> Duration {
    // Saturate so a maliciously-large `attempt` (e.g. u32::MAX) can't overflow.
    let exp = cfg.initial_backoff.saturating_mul(1u32 << attempt.min(16));
    let cap = exp.min(cfg.max_backoff);
    let frac: f64 = rand::random::<f64>(); // [0, 1)
    cap.mul_f64(frac)
}

/// Wrap the raw SSE event stream with two nested deadlines: idle between
/// successive events, and absolute as an overall ceiling. The first-event TTFB
/// deadline is NOT this guard's concern — by the time a stream reaches here its
/// first event has already been consumed by `connect_and_first_event`, which owns
/// the retryable "connect → first event" unit and shares its budget with the
/// connect phase. That helper also performs the `set_phase(Streaming)` transition
/// that used to live on this guard's `first` branch.
/// Every SSE event from the server — content, thinking, role frames, keepalives,
/// `[DONE]` — counts as transport liveness and resets the idle timer. We measure
/// events, not parsed chunks, so a model that emits thinking tokens the parser swallows
/// is correctly seen as alive. Emits the matching `ProviderError::Timeout { phase }` on
/// overrun and stops the stream (terminal — once any event has arrived, a timeout here
/// is never retried; a partial completion must never be re-requested).
#[cfg(test)]
pub(crate) fn guard_idle_sse<E>(
    inner: impl Stream<Item = Result<eventsource_stream::Event, eventsource_stream::EventStreamError<E>>>
        + Send
        + 'static,
    cfg: TransportConfig,
) -> impl Stream<Item = Result<eventsource_stream::Event, ProviderError>> + Send + 'static
where
    E: std::fmt::Display + Send + 'static,
{
    guard_idle_sse_until(inner, cfg, tokio::time::Instant::now() + cfg.absolute_cap)
}

fn guard_idle_sse_until<E>(
    inner: impl Stream<Item = Result<eventsource_stream::Event, eventsource_stream::EventStreamError<E>>>
        + Send
        + 'static,
    cfg: TransportConfig,
    deadline: tokio::time::Instant,
) -> impl Stream<Item = Result<eventsource_stream::Event, ProviderError>> + Send + 'static
where
    E: std::fmt::Display + Send + 'static,
{
    async_stream::stream! {
    tokio::pin!(inner);
    loop {
    let next = inner.next();
    tokio::select! {
    biased;
    _ = tokio::time::sleep_until(deadline) => {
    yield Err(ProviderError::Timeout { phase: TimeoutPhase::Absolute });
    return;
    }
    r = tokio::time::timeout(cfg.idle_timeout, next) => match r {
    Err(_)         => { yield Err(ProviderError::Timeout { phase: TimeoutPhase::Idle }); return; }
    Ok(None)       => { gait_types::diagnostic::set_phase(gait_types::diagnostic::Phase::StreamEnd); return; }
    Ok(Some(item)) => match item {
    Ok(event) => { yield Ok(event); }
    Err(e) => { yield Err(ProviderError::Sse(e.to_string())); return; }
    },
    },
    }
    }
    }
}

/// Parse the HTTP `Retry-After` header as a number of seconds. Returns `None`
/// for missing, non-numeric, or HTTP-date headers (we only honour the delta-seconds
/// form — it's what every LLM provider we talk to sends today).
pub(crate) fn parse_retry_after(resp: &reqwest::Response) -> Option<Duration> {
    resp.headers()
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
}

/// The single retryable unit that owns "connect → await first event", sharing
/// one budget across both phases. `connect` performs the whole per-attempt
/// request (POST + `.bytes_stream().eventsource()`), so a retry re-issues the
/// entire request rather than resuming a partial one — replayability holds
/// because nothing has been consumed from the server yet.
///
/// Retryable: a bounded `connect()` overrun, a bounded first-event overrun
/// (both zero-byte), and any connect-phase error `connect()` itself returns
/// (e.g. `Connect`, `Http 429/5xx`) via the unchanged `is_retryable`
/// classification. Terminal: a real SSE error or the stream ending before any
/// event — once bytes have been produced by the server, this helper does not
/// re-request them.
///
/// Each attempt resets the phase clock (`set_phase(LlmRequest)`) so a bounded
/// retry sequence cannot falsely trip the stall watchdog.
pub(crate) async fn connect_and_first_event<C, Fut, S, E>(
    cfg: TransportConfig,
    cancel: &CancellationToken,
    connect: C,
) -> Result<
    impl Stream<Item = Result<eventsource_stream::Event, ProviderError>> + Send + 'static,
    ProviderError,
>
where
    C: Fn() -> Fut,
    Fut: Future<Output = Result<S, ProviderError>>,
    S: Stream<Item = Result<eventsource_stream::Event, eventsource_stream::EventStreamError<E>>>
        + Send
        + 'static,
    E: std::fmt::Display + Send + 'static,
{
    let mut attempt: u32 = 0;
    let absolute_deadline = tokio::time::Instant::now() + cfg.absolute_cap;
    // Loop breaks with (Option<first_event>, owned_rest_stream). `None` = the
    // stream ended before any event (terminal, not a timeout) — the single
    // generator below drains an immediately-empty guarded stream in that case.
    let (first, rest): (Option<eventsource_stream::Event>, std::pin::Pin<Box<S>>) = loop {
        // Reset the phase clock each attempt (watchdog discipline, parity with retry_phase1).
        gait_types::diagnostic::set_phase(gait_types::diagnostic::Phase::LlmRequest);
        let attempt_res: Result<
            (Option<eventsource_stream::Event>, std::pin::Pin<Box<S>>),
            ProviderError,
        > = async {
            // 1) Connect (POST + header check) — BOUNDED, else a header-stall hangs forever.
            let stream = tokio::select! {
                biased;
                _ = tokio::time::sleep_until(absolute_deadline) => {
                    return Err(ProviderError::Timeout { phase: TimeoutPhase::Absolute });
                }
                result = tokio::time::timeout(cfg.ttfb_timeout, connect()) => match result {
                    Err(_) => return Err(ProviderError::Timeout { phase: TimeoutPhase::Ttfb }),
                    Ok(result) => result?,
                }
            };
            let mut stream = Box::pin(stream); // owned, Unpin, Send + 'static
                                               // 2) First event — BOUNDED. Zero bytes on overrun → retryable Ttfb.
            tokio::select! {
                biased;
                _ = tokio::time::sleep_until(absolute_deadline) => {
                    Err(ProviderError::Timeout { phase: TimeoutPhase::Absolute })
                }
                result = tokio::time::timeout(cfg.ttfb_timeout, stream.next()) => match result {
                    Err(_) => Err(ProviderError::Timeout { phase: TimeoutPhase::Ttfb }),
                    Ok(None) => Ok((None, stream)),
                    Ok(Some(Err(e))) => Err(ProviderError::Sse(e.to_string())),
                    Ok(Some(Ok(first))) => Ok((Some(first), stream)),
                }
            }
        }
        .await;

        match attempt_res {
            Ok(pair) => break pair,
            Err(e) => {
                let attempts_used = attempt + 1;
                if !e.is_retryable() || attempts_used >= cfg.max_attempts as u32 {
                    return Err(e);
                }
                let delay = e
                    .retry_after()
                    .unwrap_or_else(|| backoff_delay(&cfg, attempt));
                eprintln!(
                    "[gait] {}",
                    retry_diagnostic(attempts_used, cfg.max_attempts, &e)
                );
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return Err(ProviderError::Other("cancelled".into())),
                    _ = tokio::time::sleep_until(absolute_deadline) => {
                        return Err(ProviderError::Timeout { phase: TimeoutPhase::Absolute });
                    }
                    _ = tokio::time::sleep(delay) => {}
                }
                attempt += 1;
            }
        }
    };

    // Single generator (one opaque type). Guard only idle+absolute on the remainder.
    let guarded = guard_idle_sse_until(rest, cfg, absolute_deadline);
    Ok(async_stream::stream! {
        if let Some(first) = first {
            gait_types::diagnostic::set_phase(gait_types::diagnostic::Phase::Streaming);
            yield Ok(first);
        }
        tokio::pin!(guarded);
        while let Some(item) = guarded.next().await { yield item; }
    })
}

fn retry_diagnostic(attempt: u32, max_attempts: u8, error: &ProviderError) -> String {
    let reason = match error {
        ProviderError::Connect(_) => "connect",
        ProviderError::Timeout { phase } => match phase {
            TimeoutPhase::Connect => "connect_timeout",
            TimeoutPhase::Ttfb => "ttfb_timeout",
            TimeoutPhase::Idle => "idle_timeout",
            TimeoutPhase::Absolute => "absolute_timeout",
            TimeoutPhase::Request => "request_timeout",
        },
        ProviderError::Http { status, .. } => {
            return format!(
                "transport_retry attempt={attempt}/{max_attempts} reason=http_status_{status}"
            )
        }
        ProviderError::Overloaded => "overloaded",
        ProviderError::Sse(_) => "sse",
        ProviderError::StreamIncomplete { .. } => "stream_incomplete",
        ProviderError::ResponseTooLarge { .. } => "response_too_large",
        ProviderError::Json(_) => "json",
        ProviderError::Other(_) => "other",
    };
    format!("transport_retry attempt={attempt}/{max_attempts} reason={reason}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{ProviderError, TimeoutPhase};

    #[test]
    fn retryable_classification() {
        use std::time::Duration;
        assert!(ProviderError::Http {
            status: 429,
            body: "".into(),
            retry_after: None
        }
        .is_retryable());
        assert!(ProviderError::Http {
            status: 503,
            body: "".into(),
            retry_after: None
        }
        .is_retryable());
        assert!(!ProviderError::Http {
            status: 400,
            body: "".into(),
            retry_after: None
        }
        .is_retryable());
        assert!(ProviderError::Timeout {
            phase: TimeoutPhase::Ttfb
        }
        .is_retryable());
        assert!(!ProviderError::Timeout {
            phase: TimeoutPhase::Idle
        }
        .is_retryable());
        assert!(!ProviderError::StreamIncomplete { received_bytes: 0 }.is_retryable());
        assert!(!ProviderError::Overloaded.is_retryable());
        // Touch Duration so the import is not flagged as unused on minimal configs.
        let _ = Duration::from_secs(0);
    }

    // The hanging-connect regression (a server that accepts the connection then
    // never sends response headers must not hang forever) is now covered by
    // `connect_hang_is_bounded_not_infinite` below, ported onto
    // `connect_and_first_event` (this helper subsumes the old `retry_phase1`).

    #[test]
    fn backoff_is_bounded_and_increasing() {
        let cfg = TransportConfig::default();
        let b0 = backoff_delay(&cfg, 0);
        let b1 = backoff_delay(&cfg, 1);
        // Full jitter: each draw is in [0, capped_exp].
        assert!(b0 <= cfg.initial_backoff);
        assert!(b1 <= cfg.max_backoff);
        // initial * 2^1 = 1s, jittered down — a hard upper bound.
        assert!(b1 <= Duration::from_millis(1000));
    }

    // Task 1: guard_idle_sse now owns only idle + absolute. A stream whose first
    // event arrives immediately, then stalls past idle_timeout, must yield
    // Timeout{Idle} — never Timeout{Ttfb} (that responsibility moved to
    // connect_and_first_event, Task 2).
    #[tokio::test(start_paused = true)]
    async fn guard_idle_only_covers_idle_and_absolute() {
        use futures_util::stream;

        let event = eventsource_stream::Event {
            event: String::new(),
            data: "first".to_string(),
            id: String::new(),
            retry: None,
        };
        // One event immediately, then the stream never yields again (simulates a
        // healthy connection that stalls between events).
        type BoxedEvents = std::pin::Pin<
            Box<
                dyn Stream<
                        Item = Result<
                            eventsource_stream::Event,
                            eventsource_stream::EventStreamError<std::io::Error>,
                        >,
                    > + Send,
            >,
        >;
        let inner: BoxedEvents =
            Box::pin(stream::once(async { Ok(event) }).chain(stream::pending()));

        let cfg = TransportConfig {
            idle_timeout: Duration::from_millis(50),
            absolute_cap: Duration::from_secs(60),
            ..TransportConfig::default()
        };

        let guarded = guard_idle_sse(inner, cfg);
        tokio::pin!(guarded);

        // First item passes through untouched.
        match guarded.next().await {
            Some(Ok(e)) => assert_eq!(e.data, "first"),
            other => panic!("expected first event, got {other:?}"),
        }

        // Advance past idle_timeout with nothing else arriving.
        tokio::time::advance(cfg.idle_timeout + Duration::from_millis(10)).await;

        match guarded.next().await {
            Some(Err(ProviderError::Timeout {
                phase: TimeoutPhase::Idle,
            })) => {}
            other => panic!("expected Timeout{{Idle}}, got {other:?}"),
        }
    }

    // Concrete stream type shared by connect_and_first_event tests: connect()
    // closures must return the same `S` on every call, so we box+pin the raw
    // eventsource stream once here.
    type BoxedSse = std::pin::Pin<
        Box<
            dyn Stream<
                    Item = Result<
                        eventsource_stream::Event,
                        eventsource_stream::EventStreamError<std::io::Error>,
                    >,
                > + Send,
        >,
    >;

    fn sse_event(data: &str) -> eventsource_stream::Event {
        eventsource_stream::Event {
            event: String::new(),
            data: data.to_string(),
            id: String::new(),
            retry: None,
        }
    }

    // AC1: a zero-byte first-event TTFB timeout on attempt 1 is retried, and a
    // fresh connect on attempt 2 that succeeds hands off the captured first
    // event plus the remainder of the stream.
    #[tokio::test(start_paused = true)]
    async fn first_event_timeout_zero_bytes_retries_then_succeeds() {
        use futures_util::stream;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let calls = Arc::new(AtomicUsize::new(0));
        let cfg = TransportConfig {
            ttfb_timeout: Duration::from_millis(20),
            max_attempts: 3,
            initial_backoff: Duration::from_millis(1),
            max_backoff: Duration::from_millis(1),
            ..TransportConfig::default()
        };
        let cancel = CancellationToken::new();

        let calls2 = calls.clone();
        let connect = move || {
            let calls = calls2.clone();
            async move {
                let n = calls.fetch_add(1, Ordering::SeqCst);
                let s: BoxedSse = if n == 0 {
                    // Attempt 1: connects fine, but never yields a first event.
                    Box::pin(stream::pending())
                } else {
                    // Attempt 2: yields two events immediately.
                    Box::pin(stream::iter(vec![
                        Ok(sse_event("first")),
                        Ok(sse_event("second")),
                    ]))
                };
                Ok::<_, ProviderError>(s)
            }
        };

        let result_stream = connect_and_first_event(cfg, &cancel, connect).await;
        let stream = result_stream.expect("expected Ok stream after retry");
        tokio::pin!(stream);

        match stream.next().await {
            Some(Ok(e)) => assert_eq!(e.data, "first"),
            other => panic!("expected first event, got {other:?}"),
        }
        match stream.next().await {
            Some(Ok(e)) => assert_eq!(e.data, "second"),
            other => panic!("expected second event, got {other:?}"),
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "connect should be called twice"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn connect_error_retries_then_delivers_one_sequence() {
        use futures_util::stream;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let calls = Arc::new(AtomicUsize::new(0));
        let cfg = TransportConfig {
            max_attempts: 2,
            initial_backoff: Duration::ZERO,
            max_backoff: Duration::ZERO,
            ..TransportConfig::default()
        };
        let cancel = CancellationToken::new();
        let observed = calls.clone();
        let connect = move || {
            let observed = observed.clone();
            async move {
                if observed.fetch_add(1, Ordering::SeqCst) == 0 {
                    Err(ProviderError::Connect("temporary".into()))
                } else {
                    Ok::<BoxedSse, ProviderError>(Box::pin(stream::iter(vec![Ok(sse_event(
                        "only-sequence",
                    ))])))
                }
            }
        };

        let stream = connect_and_first_event(cfg, &cancel, connect)
            .await
            .expect("connect error should retry");
        let events = stream.collect::<Vec<_>>().await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].as_ref().expect("event").data, "only-sequence");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn stream_error_after_first_event_is_never_replayed() {
        use futures_util::stream;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        let connect = move || {
            let observed = observed.clone();
            async move {
                observed.fetch_add(1, Ordering::SeqCst);
                let bad_utf8 = String::from_utf8(vec![0xff]).unwrap_err();
                Ok::<BoxedSse, ProviderError>(Box::pin(stream::iter(vec![
                    Ok(sse_event("first")),
                    Err(eventsource_stream::EventStreamError::Utf8(bad_utf8)),
                ])))
            }
        };
        let cfg = TransportConfig {
            max_attempts: 3,
            ..TransportConfig::default()
        };
        let cancel = CancellationToken::new();

        let stream = connect_and_first_event(cfg, &cancel, connect)
            .await
            .expect("first event establishes the stream");
        let items = stream.collect::<Vec<_>>().await;
        assert!(matches!(items.first(), Some(Ok(event)) if event.data == "first"));
        assert!(matches!(items.get(1), Some(Err(ProviderError::Sse(_)))));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn sensitive_headers_and_diagnostics_never_render_credentials() {
        let header = sensitive_header("Bearer top-secret-token").expect("valid header");
        assert!(header.is_sensitive());

        let error = ProviderError::Http {
            status: 503,
            body: "top-secret-token".into(),
            retry_after: Some(Duration::ZERO),
        };
        let diagnostic = retry_diagnostic(1, 3, &error);
        assert_eq!(
            diagnostic,
            "transport_retry attempt=1/3 reason=http_status_503"
        );
        assert!(!diagnostic.contains("top-secret-token"));

        let secrets = vec!["Bearer top-secret-token".into(), "top-secret-token".into()];
        assert_eq!(
            redact_secrets(
                "echo Bearer top-secret-token and top-secret-token".into(),
                &secrets,
            ),
            "echo [REDACTED] and [REDACTED]"
        );
        assert_eq!(truncate_utf8("éé".into(), 3), "é");
    }

    #[test]
    fn transport_retains_configured_connect_timeout() {
        let cfg = TransportConfig {
            connect_timeout: Duration::from_millis(17),
            ..TransportConfig::default()
        };
        let transport = ProviderTransport::new(cfg).expect("transport");
        assert_eq!(
            transport.config().connect_timeout,
            Duration::from_millis(17)
        );
    }

    // AC3: a real SSE error as the first item is terminal — no retry, no
    // reconnection, connect is called exactly once.
    #[tokio::test(start_paused = true)]
    async fn first_event_real_error_is_terminal_no_retry() {
        use futures_util::stream;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let calls = Arc::new(AtomicUsize::new(0));
        let cfg = TransportConfig {
            ttfb_timeout: Duration::from_millis(20),
            max_attempts: 3,
            initial_backoff: Duration::from_millis(1),
            max_backoff: Duration::from_millis(1),
            ..TransportConfig::default()
        };
        let cancel = CancellationToken::new();

        let calls2 = calls.clone();
        let connect = move || {
            let calls = calls2.clone();
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                let bad_utf8 = String::from_utf8(vec![0xff]).unwrap_err();
                let s: BoxedSse = Box::pin(stream::iter(vec![Err(
                    eventsource_stream::EventStreamError::Utf8(bad_utf8),
                )]));
                Ok::<_, ProviderError>(s)
            }
        };

        let result = connect_and_first_event(cfg, &cancel, connect).await;
        match result {
            Err(ProviderError::Sse(_)) => {}
            other => panic!(
                "expected terminal Sse error, got a stream result: {}",
                other.is_ok()
            ),
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "connect should be called exactly once"
        );
    }

    // CRITICAL-1 regression, ported from retry_phase1_bounds_a_hanging_request:
    // a connect() whose future never resolves (a header stall before any bytes)
    // must be bounded by ttfb_timeout and exhausted via retries, not hang forever.
    #[tokio::test(start_paused = true)]
    async fn connect_hang_is_bounded_not_infinite() {
        let cfg = TransportConfig {
            ttfb_timeout: Duration::from_millis(20),
            max_attempts: 2,
            initial_backoff: Duration::from_millis(1),
            max_backoff: Duration::from_millis(1),
            ..TransportConfig::default()
        };
        let cancel = CancellationToken::new();

        let connect = || async {
            // Never resolves — simulates a stalled header wait.
            std::future::pending::<Result<BoxedSse, ProviderError>>().await
        };

        let result = connect_and_first_event(cfg, &cancel, connect).await;
        match result {
            Err(ProviderError::Timeout {
                phase: TimeoutPhase::Ttfb,
            }) => {}
            other => panic!(
                "expected retryable TTFB timeout, got a stream result: {}",
                other.is_ok()
            ),
        }
    }
}

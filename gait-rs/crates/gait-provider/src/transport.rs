//! Transport-layer knobs and helpers shared by every LLM provider.
//!
//! Lives at `pub(crate)` scope on purpose: this is not a public API, and it is
//! intentionally not `serde`-derived. Defaults are the policy; tests can override
//! individual fields by struct-literal.

use crate::provider::{ProviderError, TimeoutPhase};
use futures_util::{Stream, StreamExt};
use std::future::Future;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

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
pub(crate) fn guard_idle_sse<E>(
    inner: impl Stream<Item = Result<eventsource_stream::Event, eventsource_stream::EventStreamError<E>>>
        + Send
        + 'static,
    cfg: TransportConfig,
) -> impl Stream<Item = Result<eventsource_stream::Event, ProviderError>> + Send + 'static
where
    E: std::fmt::Display + Send + 'static,
{
    async_stream::stream! {
    tokio::pin!(inner);
    let deadline = tokio::time::Instant::now() + cfg.absolute_cap;
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
            let stream = match tokio::time::timeout(cfg.ttfb_timeout, connect()).await {
                Err(_) => {
                    return Err(ProviderError::Timeout {
                        phase: TimeoutPhase::Ttfb,
                    })
                }
                Ok(r) => r?, // connect-phase errors (Connect / Http 429·5xx) surface here
            };
            let mut stream = Box::pin(stream); // owned, Unpin, Send + 'static
                                               // 2) First event — BOUNDED. Zero bytes on overrun → retryable Ttfb.
            match tokio::time::timeout(cfg.ttfb_timeout, stream.next()).await {
                Err(_) => Err(ProviderError::Timeout {
                    phase: TimeoutPhase::Ttfb,
                }),
                Ok(None) => Ok((None, stream)), // ended before any event → terminal
                Ok(Some(Err(e))) => Err(ProviderError::Sse(e.to_string())), // real error → terminal
                Ok(Some(Ok(first))) => Ok((Some(first), stream)), // got it → hand off
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
                eprintln!(
                    "[gait] transport_retry attempt={attempts_used}/{} reason={e:?}",
                    cfg.max_attempts
                );
                let delay = e
                    .retry_after()
                    .unwrap_or_else(|| backoff_delay(&cfg, attempt));
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return Err(ProviderError::Other("cancelled".into())),
                    _ = tokio::time::sleep(delay) => {}
                }
                attempt += 1;
            }
        }
    };

    // Single generator (one opaque type). Guard only idle+absolute on the remainder.
    let guarded = guard_idle_sse(rest, cfg);
    Ok(async_stream::stream! {
        if let Some(first) = first {
            gait_types::diagnostic::set_phase(gait_types::diagnostic::Phase::Streaming);
            yield Ok(first);
        }
        tokio::pin!(guarded);
        while let Some(item) = guarded.next().await { yield item; }
    })
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

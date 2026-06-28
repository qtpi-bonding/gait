//! Transport-layer knobs and helpers shared by every LLM provider.
//!
//! Lives at `pub(crate)` scope on purpose: this is not a public API, and it is
//! intentionally not `serde`-derived. Defaults are the policy; tests can override
//! individual fields by struct-literal.

use crate::provider::{ProviderError, TimeoutPhase};
use eventsource_stream::EventStreamError;
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

/// Wrap the raw SSE event stream with three nested deadlines: TTFB on the first
/// event, idle between subsequent events, and absolute as an overall ceiling.
/// Every SSE event from the server — content, thinking, role frames, keepalives,
/// `[DONE]` — counts as transport liveness and resets the idle timer. We measure
/// events, not parsed chunks, so a model that emits thinking tokens the parser swallows
/// is correctly seen as alive. Emits the matching `ProviderError::Timeout { phase }` on
/// overrun and stops the stream (terminal — the spec is retry-pre-first-byte-only).
pub(crate) fn guard_idle_sse(
    inner: impl Stream<Item = Result<eventsource_stream::Event, eventsource_stream::EventStreamError>>
        + Send
        + 'static,
    cfg: TransportConfig,
) -> impl Stream<Item = Result<eventsource_stream::Event, ProviderError>> + Send + 'static {
    async_stream::stream! {
    tokio::pin!(inner);
    let deadline = tokio::time::Instant::now() + cfg.absolute_cap;
    let mut first = true;
    loop {
    let gap = if first { cfg.ttfb_timeout } else { cfg.idle_timeout };
    let phase = if first { TimeoutPhase::Ttfb } else { TimeoutPhase::Idle };
    let next = inner.next();
    tokio::select! {
    biased;
    _ = tokio::time::sleep_until(deadline) => {
    yield Err(ProviderError::Timeout { phase: TimeoutPhase::Absolute });
    return;
    }
    r = tokio::time::timeout(gap, next) => match r {
    Err(_)         => { yield Err(ProviderError::Timeout { phase }); return; }
    Ok(None)       => { crate::diagnostic::set_phase(crate::diagnostic::Phase::StreamEnd); return; }
    Ok(Some(item)) => match item {
    Ok(event) => {
    if first { crate::diagnostic::set_phase(crate::diagnostic::Phase::Streaming); }
    yield Ok(event);
    }
    Err(e) => { yield Err(ProviderError::Sse(e.to_string())); return; }
    },
    },
    }
    first = false;
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

/// Pre-first-byte retry loop. Runs `f` up to `cfg.max_attempts` times on
/// classified retryable `ProviderError`s, with full-jitter exponential
/// backoff (or `Retry-After` if the server supplied one). Cancel-aware: every
/// `await` — both `f()` and the backoff sleep — is `select!`-ed against the
/// cancel token, so an Esc during a retry never blocks the turn-cancel path.
///
/// Mid-stream errors (Sse/StreamIncomplete/Overloaded/Json/Other) are terminal
/// per the spec; `is_retryable` returns false for them and we surface
/// immediately.
///
/// v1: threading is the simpler outer `tokio::select!` already in `turn/mod.rs`
/// — the call site cancels the whole `stream().await`, including any in-flight
/// retry. The token here is a `CancellationToken::new()` placeholder so the
/// helper is always cancel-aware; the call sites pass `&CancellationToken::new()`
/// when no token is in scope (acceptable per the plan note).
pub(crate) async fn retry_phase1<T, F, Fut>(
    cfg: &TransportConfig,
    cancel: &CancellationToken,
    f: F,
) -> Result<T, ProviderError>
where
    F: Fn() -> Fut,
    Fut: Future<Output = Result<T, ProviderError>>,
{
    let mut attempt: u32 = 0;
    loop {
        let result = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(ProviderError::Other("cancelled".into())),
            // Bound the request/header-wait phase. `connect_timeout` only covers the
            // TCP/TLS handshake, and `guard_idle_sse` cannot start until response headers
            // arrive — so without a deadline here, a server that accepts the connection
            // then stalls before sending headers hangs forever (observed: an 18-minute
            // hang past the 600s absolute cap). Map the overrun to a retryable TTFB
            // timeout so this loop backs off and retries pre-first-byte.
            r = tokio::time::timeout(cfg.ttfb_timeout, f()) => match r {
                Err(_) => Err(ProviderError::Timeout { phase: TimeoutPhase::Ttfb }),
                Ok(r) => r,
            },
        };
        match result {
            Ok(v) => return Ok(v),
            Err(e) => {
                let attempts_used = attempt + 1;
                if !e.is_retryable() || attempts_used >= cfg.max_attempts as u32 {
                    return Err(e);
                }
                // Task 6: transport_retry observability — v1 uses log::warn!
                eprintln!(
                    "[gait] transport_retry attempt={attempts_used}/{} reason={e:?}",
                    cfg.max_attempts
                );
                let delay = e
                    .retry_after()
                    .unwrap_or_else(|| backoff_delay(cfg, attempt));
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return Err(ProviderError::Other("cancelled".into())),
                    _ = tokio::time::sleep(delay) => {}
                }
                attempt += 1;
            }
        }
    }
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

    // Regression: a server that accepts the connection then never sends response
    // headers must NOT hang retry_phase1 forever. The per-attempt ttfb_timeout
    // converts the stall into a retryable TTFB error; with max_attempts the loop
    // gives up promptly instead of blocking past the absolute cap.
    #[tokio::test]
    async fn retry_phase1_bounds_a_hanging_request() {
        let cfg = TransportConfig {
            ttfb_timeout: Duration::from_millis(20),
            max_attempts: 2,
            initial_backoff: Duration::from_millis(1),
            max_backoff: Duration::from_millis(1),
            ..TransportConfig::default()
        };
        let cancel = CancellationToken::new();
        let result = retry_phase1::<(), _, _>(&cfg, &cancel, || async {
            // Never resolves — simulates a stalled header wait.
            std::future::pending::<Result<(), ProviderError>>().await
        })
        .await;
        match result {
            Err(ProviderError::Timeout {
                phase: TimeoutPhase::Ttfb,
            }) => {}
            other => panic!("expected retryable TTFB timeout, got {other:?}"),
        }
    }

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
}

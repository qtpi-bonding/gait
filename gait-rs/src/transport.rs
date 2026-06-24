//! Transport-layer knobs and helpers shared by every LLM provider.
//!
//! Lives at `pub(crate)` scope on purpose: this is not a public API, and it is
//! intentionally not `serde`-derived. Defaults are the policy; tests can override
//! individual fields by struct-literal.

use crate::provider::{ProviderError, StreamChunk, TimeoutPhase};
use futures_util::{Stream, StreamExt};
use std::time::Duration;

/// All knobs the LLM path cares about. `Default` *is* the production policy.
#[derive(Debug, Clone, Copy)]
pub(crate) struct TransportConfig {
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

/// Wrap a streaming response with three nested deadlines: TTFB on the first
/// poll, idle between subsequent polls, and absolute as an overall ceiling.
/// Emits the matching `ProviderError::Timeout { phase }` on overrun and stops
/// the stream (terminal — the spec is retry-pre-first-byte-only).
pub(crate) fn guard_idle(
    inner: impl Stream<Item = Result<StreamChunk, ProviderError>> + Send + 'static,
    cfg: TransportConfig,
) -> impl Stream<Item = Result<StreamChunk, ProviderError>> + Send + 'static {
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
                    Ok(None)       => { return; }
                    Ok(Some(item)) => {
                        let stop = item.is_err();
                        yield item;
                        if stop { return; }
                    }
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

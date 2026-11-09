//! What a model call reports to telemetry: one `ModelCallFailed` per failed
//! attempt, so a monitor sees quota pressure (429s that retries absorb) before
//! turns start failing. The caller says whose call it is with [`with_call`];
//! the transport does not know the session, turn or model.
use std::future::Future;

use crate::provider::{ProviderError, TimeoutPhase};

/// Who a model call belongs to.
#[derive(Clone, Debug, Default)]
pub struct CallContext {
    pub session_id: String,
    pub turn_id: String,
    /// "provider/model-id"
    pub model: String,
}

tokio::task_local! {
    static CALL: CallContext;
}

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
        source_record_id: format!("{}:{}:{attempt}", ctx.session_id, ctx.turn_id),
        session_id: ctx.session_id,
        turn_id: ctx.turn_id,
        model: ctx.model,
        attempt,
        max_attempts,
        kind: kind as i32,
        http_status,
        retry_after_ms: error.retry_after().map_or(0, |d| d.as_millis() as u64),
        request_id,
        will_retry,
    });
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
            CallContext { session_id: "s-429".into(), turn_id: "t1".into(), model: "openai/gpt-x".into() },
            async { report_failed(&error, 1, 3, true) },
        )
        .await;
        let event = capture.single_event_where::<ModelCallFailed>(|e| e.session_id == "s-429");
        assert_eq!(event.kind, K::HttpStatus as i32);
        assert_eq!((event.http_status, event.retry_after_ms), (429, 7000));
        assert_eq!((event.attempt, event.max_attempts, event.will_retry), (1, 3, true));
        assert_eq!((event.request_id.as_str(), event.model.as_str()), ("req_1", "openai/gpt-x"));
    }

    #[test]
    fn a_timeout_maps_to_its_phase() {
        let capture = concord_telemetry::testing::capture_exclusive();
        report_failed(&ProviderError::Timeout { phase: TimeoutPhase::Ttfb }, 3, 3, false);
        let event = capture.single_event_where::<ModelCallFailed>(|e| e.attempt == 3 && e.session_id.is_empty());
        assert_eq!(event.kind, K::TimeoutTtfb as i32);
        assert!(!event.will_retry);
    }
}

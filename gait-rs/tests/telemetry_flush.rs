//! The brain flushes telemetry right before the spine kills it, so a collector
//! that is down must not hold the process: the flush is bounded by the lib's
//! deadline. Its own test binary, because the telemetry subscriber is global.
#![cfg(feature = "telemetry")]

use std::time::{Duration, Instant};

#[test]
fn flush_returns_promptly_when_the_collector_is_unreachable() {
    // 192.0.2.0/24 is TEST-NET-1: nothing answers, so an export blocks until
    // its own timeout rather than failing at once.
    std::env::set_var("OTEL_EXPORTER_OTLP_ENDPOINT", "http://192.0.2.1:4318");
    gait_core::telemetry::init("gait-flush-test", None, false);
    for _ in 0..50 {
        tracing::info_span!("gait.flush_test").in_scope(|| tracing::info!("buffered"));
    }
    let began = Instant::now();
    gait_core::telemetry::flush();
    let took = began.elapsed();
    assert!(took < Duration::from_secs(10), "flush held the process for {took:?}");
    // A second flush has nothing left to do.
    let again = Instant::now();
    gait_core::telemetry::flush();
    assert!(again.elapsed() < Duration::from_secs(1));
}

//! Task 8: Watchdog firing characterization.
//!
//! Pins "exit-over-hang" and the cold-start-grace behavior:
//!
//! - A **completely silent** brain (zero Diagnostic pings) is NOT killed by the
//!   watchdog — cold-start grace keeps it disarmed until the first ping. The
//!   brain runs until its process exits naturally. This is intentional: the
//!   watchdog only monitors *staleness between pings*, not the initial startup.
//!
//! - A brain that **pings once then goes silent** IS killed: the first ping arms
//!   the watchdog; on the next read-timeout (ping_ms) the stall check fires
//!   because elapsed > stall_cap.
//!
//! `WatchdogCfg::default()` reads env vars so tests complete in seconds:
//!   `GAIT_TEST_WATCHDOG_MS`      — stall_cap_ms  (default 90 000)
//!   `GAIT_TEST_WATCHDOG_PING_MS` — ping_ms       (default 15 000)

mod common;
use common::*;
use gait_proto::gait::sandbox::v1::{signal, Diagnostic, Signal};

fn one_diagnostic_ping() -> Signal {
    // A single Diagnostic frame arms the watchdog (sets last_ping_at).
    Signal {
        corr_id: 0,
        body: Some(signal::Body::Diagnostic(Diagnostic {
            record_json: r#"{"phase":"streaming"}"#.into(),
            phase: "streaming".into(),
            elapsed_ms: 100,
        })),
    }
}

/// PINS cold-start grace: a brain that emits NOTHING is not killed; run_loop
/// returns only after the mock process exits (the 500 ms sleep below).
/// If future code removes cold-start grace this test will timeout → deliberate.
#[tokio::test]
async fn silent_brain_runs_to_process_exit_not_watchdog_kill() {
    let tmp = tempfile::tempdir().unwrap();
    // Short delay so the test completes quickly (mock exits after 500 ms).
    set_delay(tmp.path(), 500);
    // No frames → mock exits cleanly with no output.
    let (sink, _calls) = run_one(tmp.path(), "wd_silent", &[]).await;
    // The watchdog still gives the process cold-start grace, but once it exits
    // without Terminated the coordinator closes the already-started lifecycle
    // as failed.
    assert!(
        sink.diagnostics
            .iter()
            .any(|(d, _)| matches!(d, gait_core::embed::Diag::Error)),
        "silent exit must surface the failed lifecycle; got: {:?}",
        sink.diagnostics
    );
    assert!(matches!(
        sink.results[0].turns[0].outcome,
        Some(gait_core::proto::gait::log::v1::turn_result::Outcome::Failed(_))
    ));
}

/// PINS exit-over-hang: a brain that pings once then hangs is killed by the
/// watchdog. The ping arms the stall clock; after ping_ms of silence the stall
/// check fires (elapsed since last ping > stall_cap).
///
/// Timing: stall_cap=2 s, ping_ms=1 s → watchdog fires ~1 s after the single
/// ping, test completes in ~1–2 s.
#[tokio::test]
async fn pinging_then_silent_brain_is_killed_by_watchdog() {
    std::env::set_var("GAIT_TEST_WATCHDOG_MS", "2000");
    std::env::set_var("GAIT_TEST_WATCHDOG_PING_MS", "1000");

    let tmp = tempfile::tempdir().unwrap();
    // Hang AFTER sending the ping: brain emits the ping, then blocks far longer
    // than the watchdog. The spine kills it before the hang completes.
    set_hang(tmp.path(), 120_000);
    let (sink, _calls) = run_one(tmp.path(), "wd_ping", &[one_diagnostic_ping()]).await;

    // Watchdog breach → emit_terminal_failure → brain_failed=true → on_turn_error
    // → MockDriver pushes Diag::Error.
    assert!(
        sink.diagnostics
            .iter()
            .any(|(d, _)| matches!(d, gait_core::embed::Diag::Error)),
        "watchdog-killed brain must surface Diag::Error; got: {:?}",
        sink.diagnostics
    );
    // Most critical: this function returned at all (didn't hang for 120 s).

    std::env::remove_var("GAIT_TEST_WATCHDOG_MS");
    std::env::remove_var("GAIT_TEST_WATCHDOG_PING_MS");
}

mod common;
use common::*;

/// Pins the 6899efd terminal-failure behavior:
///
/// 1. `Terminated { failed: true }` causes `run_loop` to return `Ok(true)`,
///    which routes to `driver.on_turn_error` (embed.rs:204-231). The harness
///    `MockDriver::on_turn_error` surfaces this as a `Diag::Error` on the sink.
/// 2. The spine (spine.rs:514-533) writes a `DiagRecord::terminal` line to
///    `diagnostic.jsonl` for any failed Terminated — **always**, regardless
///    of `GAIT_DEBUG` (the gated branch at spine.rs:619-621 is only for the
///    per-phase `Signal::Body::Diagnostic` pings, not the terminal record).
///
/// If a future refactor starts gating the terminal record on `GAIT_DEBUG`,
/// or stops surfacing the error on the sink, this test fails loudly.
#[tokio::test]
async fn failed_terminated_sets_error_and_writes_diagnostic() {
    let tmp = tempfile::tempdir().unwrap();
    let (sink, _calls) = run_one(tmp.path(), "tf1", &[terminated_fail("boom")]).await;

    // (1) brain_failed=true → run_loop Ok(true) → on_turn_error → Diag::Error.
    assert!(
        sink.diagnostics
            .iter()
            .any(|(d, _)| matches!(d, gait_core::embed::Diag::Error)),
        "failed turn must surface a Diag::Error on the sink; got {:?}",
        sink.diagnostics
    );

    // (2) Terminal DiagRecord is written to diagnostic.jsonl regardless of
    //     GAIT_DEBUG — that's the load-bearing "always explained" rule.
    let diag_path = gait_core::session::live::diagnostic_path(tmp.path(), "tf1");
    assert!(
        diag_path.exists(),
        "diagnostic.jsonl must exist for a failed Terminated (no GAIT_DEBUG gate); \
         diagnostics={:?}",
        sink.diagnostics
    );
    let body = std::fs::read_to_string(&diag_path).unwrap();
    assert!(
        body.contains("\"event\":\"error\""),
        "diagnostic.jsonl must contain a terminal error record: {body}"
    );
    assert!(
        body.contains("brain reported failure via Terminated signal"),
        "diagnostic.jsonl must carry the terminal-failure detail: {body}"
    );
}

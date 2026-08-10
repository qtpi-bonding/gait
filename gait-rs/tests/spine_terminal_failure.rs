mod common;
use common::*;

/// Pins the 6899efd terminal-failure behavior:
///
/// 1. `Terminated { failed: true }` causes `run_loop` to return `Ok(true)`,
///    which routes to `driver.on_turn_error` (embed.rs:204-231). The harness
///    `MockDriver::on_turn_error` surfaces this as a `Diag::Error` on the sink.
/// 2. The spine routes the terminal diagnostic through `DiagnosticSink`, so
///    it follows the same `GAIT_DEBUG` visibility gate as other diagnostics.
///
/// The user-facing error must remain visible even when diagnostic tracing is
/// disabled.
#[tokio::test]
async fn failed_terminated_sets_error_without_debug_log() {
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

    // (2) Debug tracing remains disabled, while the user-facing error above
    //     still explains the failure.
    let diag_path = gait_core::session::live::diagnostic_path(tmp.path(), "tf1");
    assert!(
        !diag_path.exists(),
        "diagnostic.jsonl must remain absent when GAIT_DEBUG is unset; \
         diagnostics={:?}",
        sink.diagnostics
    );
}

use crate::common::*;
use gait_proto::gait::sandbox::v1::{signal, Diagnostic, Signal};

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

/// What a brain that died mid-turn leaves behind.
struct DeadBrain {
    /// The session log's read-error line.
    line: String,
    /// The whole session log.
    log: String,
    /// The transcript the model reads on the next turn.
    transcript: String,
    /// diagnostic.jsonl, which stays on this machine.
    diagnostic: String,
}

/// Runs one turn whose brain pings, writes `stderr`, then ends as `how` says.
async fn dead_brain(session: &str, stderr: &str, how: &str) -> DeadBrain {
    assert!(
        std::env::var_os("GAIT_DEBUG").is_none(),
        "these tests prove what is kept without GAIT_DEBUG"
    );
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("mock_stderr"), stderr).unwrap();
    std::fs::write(tmp.path().join("mock_exit"), how).unwrap();
    // The ping puts the brain mid-turn, so its death is a failure.
    let ping = Signal {
        corr_id: 0,
        body: Some(signal::Body::Diagnostic(Diagnostic {
            record_json: r#"{"phase":"streaming"}"#.into(),
            phase: "streaming".into(),
            elapsed_ms: 100,
        })),
    };
    run_one(tmp.path(), session, &[ping]).await;
    let log_path = gait_core::session::live::log_path(tmp.path(), session);
    let log = std::fs::read_to_string(&log_path).expect("session log");
    let line = log
        .lines()
        .find(|line| line.contains("nerve_read_error"))
        .unwrap_or_else(|| panic!("no read error in the session log:\n{log}"))
        .to_owned();
    let messages = gait_core::session::messages_from_path(&log_path).expect("transcript");
    let diag_path = gait_core::session::live::diagnostic_path(tmp.path(), session);
    DeadBrain {
        line,
        log,
        transcript: format!("{messages:?}"),
        diagnostic: std::fs::read_to_string(diag_path).unwrap_or_default(),
    }
}

/// A brain that dies mid-turn names its cause: the error carries its exit
/// status, and diagnostic.jsonl keeps the end of its stderr, only the end.
#[tokio::test]
async fn dead_brain_error_names_exit_status_and_keeps_the_stderr_tail() {
    let stderr = format!("{}brain-last-words", "x".repeat(5000));
    let dead = dead_brain("tf_exit", &stderr, "7").await;
    assert!(
        dead.line.contains("brain_exit=exit status: 7"),
        "{}",
        dead.line
    );
    assert!(
        dead.diagnostic.contains("brain-last-words"),
        "the tail is kept without GAIT_DEBUG: {}",
        dead.diagnostic
    );
    assert!(
        dead.diagnostic.len() < 4500,
        "only the tail is kept: {}",
        dead.diagnostic.len()
    );
}

/// A brain ended by a signal names the signal.
#[tokio::test]
async fn dead_brain_error_names_the_signal() {
    let dead = dead_brain("tf_signal", "", "abort").await;
    assert!(dead.line.contains("brain_exit=signal: 6"), "{}", dead.line);
}

/// The session log is read back to the model, so it carries none of the
/// brain's stderr.
#[tokio::test]
async fn brain_stderr_stays_out_of_the_transcript() {
    let dead = dead_brain("tf_replay", "token=hunter2-private", "7").await;
    assert!(
        dead.transcript.contains("brain_exit=exit status: 7"),
        "the model still learns how the brain ended: {}",
        dead.transcript
    );
    for kept in [&dead.transcript, &dead.log] {
        assert!(!kept.contains("hunter2-private"), "{kept}");
        assert!(!kept.contains("stderr_tail"), "{kept}");
    }
    assert!(
        dead.diagnostic.contains("hunter2-private"),
        "{}",
        dead.diagnostic
    );
}

/// What the brain printed stays on the machine: the telemetry error names how
/// it ended and carries none of its stderr.
#[cfg(feature = "telemetry")]
#[tokio::test]
async fn brain_stderr_stays_out_of_telemetry() {
    let capture = concord_telemetry::testing::capture_exclusive();
    dead_brain("tf_private", "token=hunter2-private", "7").await;
    let errors = format!("{:?}", capture.errors_where(|_| true));
    assert!(errors.contains("brain_exit=exit status: 7"), "{errors}");
    let spans = format!("{:?}", concord_telemetry::testing::spans());
    for sent in [errors, spans] {
        assert!(!sent.contains("hunter2-private"), "{sent}");
        assert!(!sent.contains("stderr_tail"), "{sent}");
    }
}

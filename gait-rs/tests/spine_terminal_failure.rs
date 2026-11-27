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
    let ours = format!(
        "{:?}",
        capture.errors_where(|record| {
            format!("{:?}", record.fields.get("gait_session_id")).contains("tf_private")
        })
    );
    assert!(ours.contains("brain_exit=exit status: 7"), "{ours}");
    let errors = format!("{:?}", capture.errors_where(|_| true));
    let spans = format!("{:?}", concord_telemetry::testing::spans());
    for sent in [errors, spans] {
        assert!(!sent.contains("hunter2-private"), "{sent}");
        assert!(!sent.contains("stderr_tail="), "{sent}");
    }
}

/// A brain whose stderr holds `stderr` and which exits with status 7.
fn doomed_brain(stderr: &str) -> tempfile::TempDir {
    assert!(
        std::env::var_os("GAIT_DEBUG").is_none(),
        "these tests prove what is kept without GAIT_DEBUG"
    );
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("mock_stderr"), stderr).unwrap();
    std::fs::write(tmp.path().join("mock_exit"), "7").unwrap();
    tmp
}

/// A brain that dies before sending anything names how it ended, and its
/// stderr is kept only in diagnostic.jsonl.
#[tokio::test]
async fn a_brain_dead_before_its_first_signal_names_its_exit() {
    let tmp = doomed_brain("token=hunter2-early");
    run_one(tmp.path(), "tf_early", &[]).await;
    let log_path = gait_core::session::live::log_path(tmp.path(), "tf_early");
    let log = std::fs::read_to_string(log_path).expect("session log");
    assert!(log.contains("brain_exit=exit status: 7"), "{log}");
    assert!(!log.contains("hunter2-early"), "{log}");
    let diag_path = gait_core::session::live::diagnostic_path(tmp.path(), "tf_early");
    let diagnostic = std::fs::read_to_string(diag_path).unwrap_or_default();
    assert!(diagnostic.contains("hunter2-early"), "{diagnostic}");
}

/// The telemetry record of an early death names the exit and no stderr.
#[cfg(feature = "telemetry")]
#[tokio::test]
async fn brain_died_telemetry_names_the_exit_and_no_stderr() {
    let capture = concord_telemetry::testing::capture_exclusive();
    let tmp = doomed_brain("token=hunter2-early");
    run_one(tmp.path(), "tf_early_private", &[]).await;
    let ours = format!(
        "{:?}",
        capture.errors_where(|record| {
            record.code == "gait.spine.brain_died"
                && format!("{:?}", record.fields.get("gait_session_id"))
                    .contains("tf_early_private")
        })
    );
    assert!(ours.contains("brain_exit=exit status: 7"), "{ours}");
    let errors = format!("{:?}", capture.errors_where(|_| true));
    let spans = format!("{:?}", concord_telemetry::testing::spans());
    for sent in [errors, spans] {
        assert!(!sent.contains("hunter2-early"), "{sent}");
    }
}

/// A brain that ends without reading its init fails the turn and names its exit.
#[tokio::test]
async fn a_brain_gone_before_its_init_names_its_exit() {
    let tmp = doomed_brain("token=hunter2-init");
    std::fs::write(tmp.path().join("mock_exit_before_init"), "").unwrap();
    // Also writes the inference profile the turn resolves.
    write_frames(tmp.path(), &[]);
    let calls = Calls::default();
    let session = std::sync::Arc::new(gait_core::Session {
        id: "tf_init".into(),
        gait_dir: tmp.path().to_path_buf(),
        codebase_path: tmp.path().to_path_buf(),
        assigned_by: None,
        repos: mock_repos(&calls),
    });
    let mut sink = gait_core::embed::RecordingSink::default();
    let mut driver = MockDriver {
        codebase: tmp.path().to_path_buf(),
        gait_bin: std::path::PathBuf::from(env!("CARGO_BIN_EXE_mock_brain")),
    };
    // More input than a pipe holds, so writing the init has to fail.
    let input = gait_core::proto::events::user_text("x".repeat(2 << 20));
    let result = gait_core::embed::run_loop(
        gait_core::port::input::ArgSource::new(Some(input)),
        &mut sink,
        session,
        tmp.path(),
        "tf_init",
        &mut driver,
        None,
    )
    .await;
    let shown = format!("{result:?} {:?}", sink.diagnostics);
    assert!(shown.contains("brain_exit=exit status: 7"), "{shown}");
    assert!(!shown.contains("hunter2-init"), "{shown}");
    let diag_path = gait_core::session::live::diagnostic_path(tmp.path(), "tf_init");
    let diagnostic = std::fs::read_to_string(diag_path).unwrap_or_default();
    assert!(diagnostic.contains("hunter2-init"), "{diagnostic}");
}

mod common;
use common::*;
use gait_proto::gait::sandbox::v1::{signal, Diagnostic, Signal};

#[tokio::test]
async fn mock_terminated_reaches_sink_and_records_persistence() {
    let tmp = tempfile::tempdir().unwrap(); // bare temp dir — NO git init
    let (sink, calls) = run_one(tmp.path(), "smoke1", &[terminated_ok("done")]).await;
    assert_eq!(sink.results.len(), 1, "expected one TurnResult");
    match &sink.results[0] {
        gait_core::turn::TurnResult::Done { message, .. } => assert_eq!(message, "done"),
        other => panic!("expected Done, got {other:?}"),
    }
    let seq = calls.snapshot();
    assert!(
        seq.iter().any(|c| c.starts_with("create_session_worktree")),
        "{seq:?}"
    );
    assert!(
        seq.iter().any(|c| c.starts_with("commit_session")),
        "{seq:?}"
    );
}

/// Regression for the swallowed-crash bug: when the brain subprocess was
/// genuinely mid-turn (it pinged at least once — proof it got going) and then
/// its stdout pipe closes without ever sending a `Terminated` signal, the
/// relay loop must still surface a `Failed` `TurnResult` — not silently drop
/// the turn as if it succeeded. A single `Diagnostic` frame and nothing else
/// makes `mock_brain` emit one ping then exit, which is exactly a
/// mid-turn pipe-closed-without-Terminated crash (as opposed to the brain
/// never getting going at all — see `silent_brain_runs_to_process_exit_not_
/// watchdog_kill` in spine_watchdog.rs, which is deliberately NOT a failure).
#[tokio::test]
async fn brain_pipe_closed_mid_turn_surfaces_as_failure() {
    let one_ping = Signal {
        corr_id: 0,
        body: Some(signal::Body::Diagnostic(Diagnostic {
            record_json: r#"{"phase":"streaming"}"#.into(),
            phase: "streaming".into(),
            elapsed_ms: 100,
        })),
    };
    let tmp = tempfile::tempdir().unwrap();
    let (sink, _calls) = run_one(tmp.path(), "smoke2", &[one_ping]).await;
    assert_eq!(
        sink.results.len(),
        1,
        "brain crashing mid-turn must still surface a TurnResult, got {:?}",
        sink.results
    );
    match &sink.results[0] {
        gait_core::turn::TurnResult::Failed { .. } => {}
        other => panic!("expected Failed, got {other:?}"),
    }
}

/// The exact case `brain_pipe_closed_mid_turn_surfaces_as_failure` must NOT
/// regress: a brain that never sends anything at all (cold start) is not a
/// failure — see `silent_brain_runs_to_process_exit_not_watchdog_kill` in
/// spine_watchdog.rs for the canonical version of this; duplicated here in
/// miniature so the two behaviors sit side by side and can't drift apart.
#[tokio::test]
async fn brain_pipe_closed_before_ever_starting_is_not_a_failure() {
    let tmp = tempfile::tempdir().unwrap();
    let (sink, _calls) = run_one(tmp.path(), "smoke3", &[]).await;
    assert!(
        sink.results.is_empty(),
        "a brain that never got going should not synthesize a TurnResult, got {:?}",
        sink.results
    );
}

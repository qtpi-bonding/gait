mod common;
use common::*;

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

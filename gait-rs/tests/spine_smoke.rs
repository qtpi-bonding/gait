mod common;
use common::*;
use gait_proto::gait::sandbox::v1::{signal, Diagnostic, Signal};

struct UpdateOnlySource {
    body: Option<gait_core::proto::gait::log::v1::event::Body>,
}

#[async_trait::async_trait(?Send)]
impl gait_core::embed::TurnSource for UpdateOnlySource {
    async fn next(&mut self) -> Option<gait_core::embed::TurnStart> {
        self.body
            .take()
            .map(|body| gait_core::embed::TurnStart::SessionUpdate(vec![body]))
    }
}

#[tokio::test]
async fn session_update_persists_without_launching_a_brain_turn() {
    use std::sync::Arc;

    let tmp = tempfile::tempdir().unwrap();
    let calls = Calls::default();
    let repos = mock_repos(&calls);
    let session_id = "supdate1";
    let session = Arc::new(gait_core::Session {
        id: session_id.into(),
        gait_dir: tmp.path().to_path_buf(),
        codebase_path: tmp.path().to_path_buf(),
        assigned_by: None,
        repos,
    });
    let body = gait_core::proto::events::inference(gait_core::proto::gait::log::v1::Inference {
        name: "deep".into(),
        model: "openai/gpt-test".into(),
        llm_params: None,
        compaction_threshold_tokens: 128_000,
    });
    let source = UpdateOnlySource { body: Some(body) };
    let mut sink = gait_core::embed::RecordingSink::default();
    let mut driver = MockDriver {
        codebase: tmp.path().to_path_buf(),
        gait_bin: std::path::PathBuf::from("must-not-launch"),
    };

    gait_core::embed::run_loop(
        source,
        &mut sink,
        session,
        tmp.path(),
        session_id,
        &mut driver,
        None,
    )
    .await
    .unwrap();

    let events = gait_core::session::log::read_events(&gait_core::session::live::log_path(
        tmp.path(),
        session_id,
    ))
    .unwrap();
    assert!(matches!(
        events[0].body.as_ref().and_then(|body| match body {
            gait_core::proto::gait::log::v1::event::Body::Payload(payload) => {
                payload.kind.as_ref()
            }
            _ => None,
        }),
        Some(gait_core::proto::gait::log::v1::payload::Kind::Inference(_))
    ));
    let calls = calls.snapshot();
    assert_eq!(calls, vec![format!("create_session_worktree:{session_id}")]);
    assert!(
        sink.results.is_empty(),
        "session updates are not model turns"
    );
}

#[test]
fn fresh_session_update_creates_a_real_idempotent_git_worktree() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = git2::Repository::init(tmp.path()).unwrap();
    let mut config = repo.config().unwrap();
    config.set_str("user.name", "test").unwrap();
    config.set_str("user.email", "test@example.com").unwrap();
    drop(config);
    let signature = git2::Signature::now("test", "test@example.com").unwrap();
    let tree_id = repo.index().unwrap().write_tree().unwrap();
    let tree = repo.find_tree(tree_id).unwrap();
    repo.commit(Some("HEAD"), &signature, &signature, "init", &tree, &[])
        .unwrap();
    if let Ok(mut master) = repo.find_branch("master", git2::BranchType::Local) {
        master.rename("main", false).unwrap();
        repo.set_head("refs/heads/main").unwrap();
    }
    drop(tree);
    drop(repo);

    let session_id = "sfreshup";
    let repos = gait_core::git::Repos::production();
    let body = gait_core::proto::events::inference(gait_core::proto::gait::log::v1::Inference {
        name: "codex".into(),
        model: "openai-codex/gpt-test".into(),
        llm_params: None,
        compaction_threshold_tokens: 128_000,
    });
    gait_core::embed::record_session_update(&repos, tmp.path(), session_id, vec![body]).unwrap();

    let worktree = gait_core::session::live::worktree_dir(tmp.path(), session_id);
    assert!(
        worktree.join(".git").is_file(),
        "must be a linked Git worktree"
    );
    repos
        .traces
        .create_session_worktree(tmp.path(), session_id)
        .expect("the normal turn's repeated ensure must be idempotent");
}

#[tokio::test]
async fn mock_terminated_reaches_sink_and_records_persistence() {
    let tmp = tempfile::tempdir().unwrap(); // bare temp dir — NO git init
    let (sink, calls) = run_one(
        tmp.path(),
        "smoke1",
        &[append_assistant("done"), terminated_ok("done")],
    )
    .await;
    assert_eq!(sink.results.len(), 1, "expected one TurnResult");
    let outcome = sink.results[0]
        .turns
        .last()
        .and_then(|turn| turn.outcome.as_ref());
    match outcome {
        Some(gait_core::proto::gait::log::v1::turn_result::Outcome::Done(done)) => {
            match done.output.as_ref() {
                Some(gait_core::proto::gait::log::v1::turn_done::Output::Text(text)) => {
                    assert_eq!(text.message, "done")
                }
                other => panic!("expected text done, got {other:?}"),
            }
        }
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
    match sink.results[0]
        .turns
        .last()
        .and_then(|turn| turn.outcome.as_ref())
    {
        Some(gait_core::proto::gait::log::v1::turn_result::Outcome::Failed(_)) => {}
        other => panic!("expected Failed, got {other:?}"),
    }
}

/// Once RunStart and TurnStart are durable, even a cold brain exit must close
/// the lifecycle with a typed failed turn and run.
#[tokio::test]
async fn brain_pipe_closed_before_ever_starting_closes_as_failed() {
    let tmp = tempfile::tempdir().unwrap();
    let (sink, _calls) = run_one(tmp.path(), "smoke3", &[]).await;
    let outcome = sink.results[0].turns[0].outcome.as_ref();
    assert!(matches!(
        outcome,
        Some(gait_core::proto::gait::log::v1::turn_result::Outcome::Failed(failed))
            if failed.reason == "aborted"
    ));
}

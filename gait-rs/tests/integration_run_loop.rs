//! Task 2 rewrite: `run_loop` is now the sealed spine loop (spawns real `gait __brain`
//! processes). The old in-process tests that drove `run_loop` with a mock provider
//! are rewritten here to test `run_turn` directly (the primitive the brain calls).
//!
//! The behavior under test is unchanged: one mock-provider turn → user_turn appended →
//! transcript written → TurnResult::Done. Only the API changes: test at `run_turn`,
//! not at `run_loop`.
use async_stream::stream;
use gait_core::proto::gait::log::v1::{event, payload, Event, StreamItem};
use gait_core::provider::{Provider, ProviderError, StreamChunk};
use gait_core::session::log::read_events;
use gait_core::turn::run_turn;
use gait_core::types::{Message, Tool};
use std::pin::Pin;
use std::sync::Arc;
use tempfile::TempDir;

fn init_git_repo(dir: &std::path::Path) {
    let repo = git2::Repository::init(dir).unwrap();
    let mut cfg = repo.config().unwrap();
    cfg.set_str("user.name", "test").unwrap();
    cfg.set_str("user.email", "test@test.com").unwrap();
    let sig = git2::Signature::now("test", "test@test.com").unwrap();
    let tree_id = repo.index().unwrap().write_tree().unwrap();
    let tree = repo.find_tree(tree_id).unwrap();
    repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
        .unwrap();
    repo.find_branch("master", git2::BranchType::Local)
        .and_then(|mut b| {
            b.rename("main", false)?;
            Ok(())
        })
        .ok();
}

fn kind(e: &Event) -> Option<&payload::Kind> {
    match &e.body {
        Some(event::Body::Payload(p)) => p.kind.as_ref(),
        _ => None,
    }
}

/// A provider that just answers with text (one turn, no tools).
struct TextProvider;
#[async_trait::async_trait]
impl Provider for TextProvider {
    fn model_id(&self) -> &str {
        "text"
    }
    async fn stream(
        &self,
        _s: &str,
        _m: &[Message],
        _t: &[Box<dyn Tool>],
        _params: &gait_core::config::LlmParamsConfig,
        _tail_context: Option<&str>,
    ) -> Result<
        Pin<Box<dyn futures_util::Stream<Item = Result<StreamChunk, ProviderError>> + Send>>,
        ProviderError,
    > {
        Ok(Box::pin(stream! {
            yield Ok(StreamChunk::Text("answered".into()));
            yield Ok(StreamChunk::Done);
        }))
    }
}

fn make_session(ws: &std::path::Path, id: &str) -> std::sync::Arc<gait_core::session::Session> {
    let repos = gait_core::git::Repos::production();
    repos.traces.create_session_worktree(ws, id).unwrap();
    Arc::new(gait_core::session::Session {
        id: id.into(),
        gait_dir: ws.to_path_buf(),
        codebase_path: ws.to_path_buf(),
        assigned_by: None,
        repos,
    })
}

fn make_input(auto: bool) -> gait_core::turn::TurnInput {
    gait_core::turn::TurnInput {
        auto,
        max_iter: None,
        preview_policy: Default::default(),
        cancel: None,
        sandbox_on: true,
        sandbox_cfg: Default::default(),
        relay: None,
        initial_git: None,
        daemons: vec![],
    }
}

/// Equivalent of the old `run_loop_fresh_session_appends_user_turn_then_folds` test.
/// After Task 2, `run_turn` is the primitive the brain calls; the spine pre-writes
/// the user_turn before spawning. We simulate that here: pre-write the user_turn,
/// then call run_turn (Resume fold).
#[tokio::test]
async fn run_turn_prim_fresh_session_folds_after_user_turn_appended() {
    let ws = TempDir::new().unwrap();
    init_git_repo(ws.path());
    let id = "sprl0001";
    let session = make_session(ws.path(), id);

    // Simulate spine pre-writing the user_turn (Phase 4 Option A).
    let repos = gait_core::git::Repos::production();
    let head = repos
        .codebase
        .head_hash(ws.path())
        .expect("test repo must have a commit");
    let bodies = gait_core::embed::user_turn_events(&repos, ws.path(), "do the thing".into());
    let log_path = gait_core::session::live::log_path(ws.path(), id);
    std::fs::create_dir_all(log_path.parent().unwrap()).unwrap();
    for body in &bodies {
        gait_core::proto::wire::append(&log_path, body.clone()).unwrap();
    }

    // Call run_turn directly (the brain path, not run_loop).
    let caps = gait_core::profile::ResolvedProfile::for_test(
        Arc::new(TextProvider),
        vec![],
        vec![],
        vec![],
    );
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<StreamItem>();
    let emitter = gait_core::proto::Recorder::for_session(ws.path(), id, tx);

    run_turn(&session, &emitter, caps, make_input(true))
        .await
        .expect("run_turn must succeed");

    let events = read_events(&log_path).unwrap();
    let user_turns = events
        .iter()
        .filter(|e| matches!(kind(e), Some(payload::Kind::UserText(_))))
        .count();
    assert_eq!(user_turns, 1, "exactly one user_turn");
    assert!(
        events
            .iter()
            .any(|e| matches!(kind(e), Some(payload::Kind::TurnComplete(_)))),
        "turn ran (turn_complete present)"
    );
    let raw = std::fs::read_to_string(&log_path).unwrap();
    assert!(
        raw.contains(&head),
        "user_turn must carry the codebase head hash {head}"
    );
}

/// Equivalent of the old `run_loop_resume_runs_one_turn_without_appending_user_turn` test.
/// run_turn is called with no prior user_turn → bare fold → TurnComplete.
#[tokio::test]
async fn run_turn_prim_resume_runs_one_turn_without_user_turn() {
    let ws = TempDir::new().unwrap();
    init_git_repo(ws.path());
    let id = "sprl0002";
    let session = make_session(ws.path(), id);

    let caps = gait_core::profile::ResolvedProfile::for_test(
        Arc::new(TextProvider),
        vec![],
        vec![],
        vec![],
    );
    let log_path = gait_core::session::live::log_path(ws.path(), id);
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<StreamItem>();
    let emitter = gait_core::proto::Recorder::for_session(ws.path(), id, tx);

    run_turn(&session, &emitter, caps, make_input(true))
        .await
        .expect("resume turn must run");

    let events = read_events(&log_path).unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(kind(e), Some(payload::Kind::UserText(_))))
            .count(),
        0,
        "Resume must not have a user_turn"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(kind(e), Some(payload::Kind::TurnComplete(_)))),
        "the resume turn still ran"
    );
}

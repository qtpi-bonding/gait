//! Phase 4a parity: `run_loop` (the unified CLI/REPL loop) appends the input
//! `user_turn` to the log BEFORE the turn folds it, on a fresh session, and runs
//! exactly one turn per `TurnStart`. This is the end-to-end proof that `cmd_run`
//! (= ArgSource + CliDriver + run_loop) behaves like the old inline `cmd_run`.
use async_stream::stream;
use gait_core::embed::{run_loop, user_turn_events, LoopConfig, TurnDriver};
use gait_core::port::input::ArgSource;
use gait_core::port::output::{Diag, TurnSink};
use gait_core::proto::gait::log::v1::{event, payload, Event, StreamItem};
use gait_core::provider::{Provider, ProviderError, StreamChunk};
use gait_core::session::log::read_events;
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

/// Capturing sink: counts items/results so we can assert one result per turn.
struct CountSink {
    items: usize,
    results: usize,
}
impl TurnSink for CountSink {
    fn item(&mut self, _it: &StreamItem) {
        self.items += 1;
    }
    fn result(&mut self, _r: &gait_core::turn::TurnResult) {
        self.results += 1;
    }
    fn session_id(&mut self, _id: &str) {}
    fn diagnostic(&mut self, _level: Diag, _msg: &str) {}
}

/// Minimal driver: hands back a scripted profile once, never errors.
struct TestDriver {
    caps: Option<gait_core::profile::ResolvedProfile>,
    auto: bool,
}
#[async_trait::async_trait(?Send)]
impl TurnDriver for TestDriver {
    async fn resolve_profile(&mut self) -> anyhow::Result<gait_core::profile::ResolvedProfile> {
        self.caps
            .take()
            .ok_or_else(|| anyhow::anyhow!("profile already taken"))
    }
    fn turn_config(&mut self) -> LoopConfig {
        LoopConfig {
            auto: self.auto,
            max_iter: None,
            preview_policy: Default::default(),
            cancel: None,
            sandbox_on: true,
            sandbox_cfg: Default::default(),
            relay: None,
        }
    }

    fn on_turn_error(
        &mut self,
        _sink: &mut dyn TurnSink,
        e: anyhow::Error,
    ) -> std::ops::ControlFlow<()> {
        panic!("unexpected turn error: {e}");
    }
}

fn session(ws: &std::path::Path, id: &str) -> Arc<gait_core::session::Session> {
    Arc::new(gait_core::session::Session {
        id: id.into(),
        gait_dir: ws.to_path_buf(),
        codebase_path: ws.to_path_buf(),
        assigned_by: None,
        repos: gait_core::git::Repos::production(),
    })
}

#[tokio::test]
async fn run_loop_fresh_session_appends_user_turn_then_folds() {
    let ws = TempDir::new().unwrap();
    init_git_repo(ws.path());
    let id = "srl00001";

    // The CLI's primary path: --task → a user_turn body the loop appends first.
    let repos = gait_core::git::Repos::production();
    let head = repos
        .codebase
        .head_hash(ws.path())
        .expect("test repo must have a commit");
    let bodies = user_turn_events(&repos, ws.path(), "do the thing".into());
    let source = ArgSource::new(Some(bodies.into_iter().next().unwrap()));

    let caps = gait_core::profile::ResolvedProfile::for_test(
        Arc::new(TextProvider),
        vec![],
        vec![],
        vec![],
    );
    let mut driver = TestDriver {
        caps: Some(caps),
        auto: true,
    };
    let mut sink = CountSink {
        items: 0,
        results: 0,
    };

    run_loop(
        source,
        &mut sink,
        session(ws.path(), id),
        ws.path(),
        id,
        &mut driver,
    )
    .await
    .expect("run_loop must succeed on a fresh session (worktree created before append)");

    let log = ws.path().join("sessions").join(id).join("log.jsonl");
    let events = read_events(&log).unwrap();
    let user_turns = events
        .iter()
        .filter(|e| matches!(kind(e), Some(payload::Kind::UserText(_))))
        .count();
    assert_eq!(user_turns, 1, "exactly one user_turn appended");
    assert!(
        events
            .iter()
            .any(|e| matches!(kind(e), Some(payload::Kind::TurnComplete(_)))),
        "the turn ran (turn_complete)"
    );

    // The appended user_turn carried a UserTurnContext (codebase head hash).
    let raw = std::fs::read_to_string(&log).unwrap();
    assert!(
        raw.contains(&head),
        "user_turn must carry the codebase head hash {head}"
    );

    assert_eq!(sink.results, 1, "exactly one RunResult emitted");
}

#[tokio::test]
async fn run_loop_resume_runs_one_turn_without_appending_user_turn() {
    let ws = TempDir::new().unwrap();
    init_git_repo(ws.path());
    let id = "srl00002";

    // No --task → ArgSource yields Resume → one fold turn, no new user_turn.
    let source = ArgSource::new(None);
    let caps = gait_core::profile::ResolvedProfile::for_test(
        Arc::new(TextProvider),
        vec![],
        vec![],
        vec![],
    );
    let mut driver = TestDriver {
        caps: Some(caps),
        auto: true,
    };
    let mut sink = CountSink {
        items: 0,
        results: 0,
    };

    run_loop(
        source,
        &mut sink,
        session(ws.path(), id),
        ws.path(),
        id,
        &mut driver,
    )
    .await
    .expect("resume turn must run");

    let log = ws.path().join("sessions").join(id).join("log.jsonl");
    let events = read_events(&log).unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(kind(e), Some(payload::Kind::UserText(_))))
            .count(),
        0,
        "Resume must not append a user_turn"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(kind(e), Some(payload::Kind::TurnComplete(_)))),
        "the resume turn still ran"
    );
    assert_eq!(sink.results, 1);
}

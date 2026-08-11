//! Task 2 rewrite: `run_loop` is now the sealed spine loop (spawns real `gait __brain`
//! processes), so the old in-process embedding test has been rewritten to test
//! `run_turn` directly via `JsonSink` — the embedding output sink.
//!
//! This test mimics an external consumer (Serverpod/Flutter FFI-style): it
//! drives a single turn through `run_turn` and reads the output as **pbjson**
//! (canonical protobuf-JSON) via `gait_core::embed::JsonSink` — the same wire
//! format the CLI emits. The test parses the pbjson back into proto types to
//! prove the round-trip, then inspects the session's `log.jsonl`.
//!
//! Uses **only** `gait_core::embed::*` (+ tempfile/git for setup).
use async_stream::stream;
use gait_core::embed::{self, user_turn_events, JsonFrame, JsonSink, TurnSink};
use gait_core::provider::{Provider, ProviderError, StreamChunk};
use gait_core::types::{Message, Tool};
use std::cell::RefCell;
use std::pin::Pin;
use std::rc::Rc;
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

#[tokio::test]
async fn embed_drives_a_turn_and_emits_pbjson() {
    let ws = TempDir::new().unwrap();
    init_git_repo(ws.path());
    let session_id = "sembed01";

    let repos = gait_core::git::Repos::production();
    repos
        .traces
        .create_session_worktree(ws.path(), session_id)
        .unwrap();

    // Pre-write the user_turn (simulating what the spine does before spawning the brain).
    let bodies = user_turn_events(&repos, ws.path(), "embed me".into());
    let log = gait_core::session::live::log_path(ws.path(), session_id);
    std::fs::create_dir_all(log.parent().unwrap()).unwrap();
    for body in &bodies {
        gait_core::proto::wire::append(&log, body.clone()).unwrap();
    }

    let caps = embed::ResolvedProfile::for_test(Arc::new(TextProvider), vec![], vec![], vec![]);

    // Capture pbjson frames from JsonSink.
    let items: Rc<RefCell<Vec<String>>> = Default::default();
    let result: Rc<RefCell<Option<String>>> = Default::default();
    let (it, rs) = (items.clone(), result.clone());
    let mut sink = JsonSink::new(move |frame| match frame {
        JsonFrame::Item(j) => it.borrow_mut().push(j),
        JsonFrame::Result(j) => *rs.borrow_mut() = Some(j),
        JsonFrame::Diagnostic(..) => {}
    });

    let session = gait_core::session::Session {
        id: session_id.into(),
        gait_dir: ws.path().to_path_buf(),
        codebase_path: ws.path().to_path_buf(),
        assigned_by: None,
        repos: gait_core::git::Repos::production(),
    };

    let input = gait_core::turn::TurnInput {
        auto: true,
        max_iter: None,
        preview_policy: Default::default(),
        cancel: None,
        sandbox_on: true,
        sandbox_cfg: Default::default(),
        relay: None,
        initial_git: None,
        daemons: vec![],
    };

    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<embed::StreamItem>();
    let emitter = gait_core::proto::Recorder::for_session(ws.path(), session_id, tx);

    let handle =
        tokio::spawn(
            async move { gait_core::turn::run_turn(&session, &emitter, caps, input).await },
        );
    gait_core::port::output::drive(rx, &mut sink).await;

    let _brain_outcome = handle.await.unwrap().expect("run_turn must succeed");
    sink.result(&gait_core::proto::gait::log::v1::RunResult {
        run_id: "embed-run".into(),
        state: Some(
            gait_core::proto::gait::log::v1::run_result::State::Complete(Default::default()),
        ),
        turns: vec![gait_core::proto::gait::log::v1::TurnResult {
            turn_id: "embed-turn".into(),
            outcome: Some(gait_core::proto::gait::log::v1::turn_result::Outcome::Done(
                Default::default(),
            )),
            ..Default::default()
        }],
        ..Default::default()
    });

    // Parse the pbjson back (what a Dart/Serverpod consumer does): at least
    // one item decodes as a `StreamItem`, and the result decodes as a
    // `RunResult` with a complete state and a final done turn.
    let items_borrowed = items.borrow();
    assert!(
        !items_borrowed.is_empty(),
        "JsonSink must have emitted at least one item"
    );
    assert!(
        items_borrowed
            .iter()
            .any(|s| serde_json::from_str::<embed::StreamItem>(s).is_ok()),
        "at least one captured item string must parse as a pbjson StreamItem"
    );
    let result_str = result
        .borrow()
        .clone()
        .expect("JsonSink must have emitted a result");
    let v: serde_json::Value =
        serde_json::from_str(&result_str).expect("the captured result string must parse as JSON");
    assert!(
        v.get("complete").is_some()
            && v.get("turns")
                .and_then(|turns| turns.get(0))
                .and_then(|turn| turn.get("done"))
                .is_some(),
        "aggregate result pbjson must contain complete and turns[0].done (got: {v})"
    );

    // Session transcript must have the user_turn and the run_complete.
    let events = gait_core::proto::wire::read_events(&log).expect("session log must parse");
    use gait_core::proto::gait::log::v1::{event, payload, Event};
    fn kind(e: &Event) -> Option<&payload::Kind> {
        match &e.body {
            Some(event::Body::Payload(p)) => p.kind.as_ref(),
            _ => None,
        }
    }
    assert!(
        events
            .iter()
            .any(|e| matches!(kind(e), Some(payload::Kind::UserText(_)))),
        "log must contain a user_turn event (got {} events)",
        events.len()
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(kind(e), Some(payload::Kind::TurnResult(_)))),
        "brain-only run_turn must not write the spine-owned TurnResult",
    );
}

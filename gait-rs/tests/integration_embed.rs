//! Phase 5 Task 3: external-style embedding proof.
//!
//! This test mimics an external consumer (Serverpod/Flutter FFI-style): it
//! drives a single turn through `gait_core::embed::run_loop` and reads the output
//! as **pbjson** (canonical protobuf-JSON) via `gait_core::embed::JsonSink` — the
//! same wire format the CLI emits and the log is written in. The test parses
//! the pbjson back into the proto types (`StreamItem`/`RunResult`) to prove the
//! round-trip, and then inspects the session's `log.jsonl` for the expected
//! `user_turn` + `turn_complete` events.
//!
//! Uses **only** `gait_core::embed::*` (+ tempfile/git for setup, + serde_json to
//! parse pbjson). No `gait_core::cli::*`, no `gait_core::port::*` directly.
use async_stream::stream;
use async_trait::async_trait;
use gait_core::embed::{
    self, run_loop, user_turn_events, JsonFrame, JsonSink, LoopConfig, TurnDriver, TurnSink,
    TurnSource, TurnStart,
};
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

/// A provider that just answers with text (one turn, no tools) — hermetic
/// `ResolvedProfile::for_test` does not call into network/agent resolution.
struct TextProvider;
#[async_trait]
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

/// External embedder's input port: hand the loop one `Input` turn then EOF.
struct MySource {
    start: Option<TurnStart>,
}
#[async_trait(?Send)]
impl TurnSource for MySource {
    async fn next(&mut self) -> Option<TurnStart> {
        self.start.take()
    }
}

/// External embedder's driver port: hand back the resolved profile once; never error.
struct MyDriver {
    caps: Option<embed::ResolvedProfile>,
}
#[async_trait(?Send)]
impl TurnDriver for MyDriver {
    async fn resolve_profile(&mut self) -> anyhow::Result<embed::ResolvedProfile> {
        self.caps
            .take()
            .ok_or_else(|| anyhow::anyhow!("profile already taken"))
    }
    fn turn_config(&mut self) -> LoopConfig {
        LoopConfig {
            auto: true,
            max_iter: None,
            preview_policy: Default::default(),
            cancel: None,
            sandbox_on: true,
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

#[tokio::test]
async fn embed_drives_a_turn_and_emits_pbjson() {
    let ws = TempDir::new().unwrap();
    init_git_repo(ws.path());
    let session_id = "sembed01";

    // Build the one-and-only user_turn + context_status bodies (carries the
    // codebase head hash the same way the CLI/REPL do — input adapter owns
    // turn-time context).
    let repos = gait_core::git::Repos::production();
    let bodies = user_turn_events(&repos, ws.path(), "embed me".into());
    let source = MySource {
        start: Some(TurnStart::Input(bodies)),
    };

    // Hermetic profile: no api key, no tools/MCP. This is a proof of the *embedding
    // loop*, not provider resolution (resolve_profile is exercised by the REPL).
    let caps = embed::ResolvedProfile::for_test(Arc::new(TextProvider), vec![], vec![], vec![]);
    let mut driver = MyDriver { caps: Some(caps) };

    // Capture pbjson frames from JsonSink. The callback must own the String
    // (no borrowed lifetime — see embed::JsonFrame docs), so the consumer
    // copies it out via Rc<RefCell<_>>. The Rc<RefCell<_>> boxes are also
    // what keeps the closure Send-free across the await point below.
    let items: Rc<RefCell<Vec<String>>> = Default::default();
    let result: Rc<RefCell<Option<String>>> = Default::default();
    let (it, rs) = (items.clone(), result.clone());
    let mut sink = JsonSink::new(move |frame| match frame {
        JsonFrame::Item(j) => it.borrow_mut().push(j), // owned String
        JsonFrame::Result(j) => *rs.borrow_mut() = Some(j),
        JsonFrame::Diagnostic(..) => {}
    });

    // The Session the embedder is in charge of. gait_dir == codebase_path here
    // (the canonical single-root layout).
    let session = Arc::new(embed::Session {
        id: session_id.into(),
        gait_dir: ws.path().to_path_buf(),
        codebase_path: ws.path().to_path_buf(),
        assigned_by: None,
        repos: embed::Repos::production(),
    });

    run_loop(
        source,
        &mut sink,
        session,
        ws.path(),
        session_id,
        &mut driver,
    )
    .await
    .expect("embed::run_loop must succeed for the external-style embedder");

    // Parse the pbjson back (what a Dart/Serverpod consumer does): at least
    // one item decodes as a `StreamItem`, and the result decodes as a
    // `RunResult` with the expected `done` key.
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
        v.get("done").is_some(),
        "result pbjson must contain key \"done\" (got: {v})"
    );

    // Session transcript must have the user_turn (appended by the input adapter
    // pre-fold) and the turn_complete (the fold wrote it). We use the
    // proto-level `payload::Kind` match — same approach as
    // `integration_run_loop.rs` — so the check is robust to the pbjson camelCase
    // wire format ("userTurn" / "turnComplete") and to field renames.
    let log = ws
        .path()
        .join("sessions")
        .join(session_id)
        .join("log.jsonl");
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
        events
            .iter()
            .any(|e| matches!(kind(e), Some(payload::Kind::ContextStatus(_)))),
        "log must contain a context_status event (got {} events)",
        events.len()
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(kind(e), Some(payload::Kind::TurnComplete(_)))),
        "log must contain a turn_complete event (got {} events)",
        events.len(),
    );
}

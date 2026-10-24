//! REPL live-render harness (regression for "I'm not seeing the reply").
//!
//! The REPL runs its interactive session as a single open/interactive prompt
//! node through `gait_workflow::run_workflow` (spec-06 §4: the node's turn loop
//! renders through the workflow's `SinkFrame` channel, and the runner drains
//! that channel into the caller's real sink). The bug: the interactive branch
//! awaited the node to *completion* before draining, so every streamed delta sat
//! in the channel buffer until the user quit — the reply never rendered live.
//!
//! This drives the real path end-to-end with `mock_brain` (via the
//! `GAIT_BRAIN_EXE` seam) emitting one streamed delta, and asserts the caller
//! sink observes that delta *while the session is still live* — i.e. before the
//! source yields its terminating `None`. With the bug the delta only arrives
//! after the node returns, so the liveness probe reads false and the test fails.

mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use gait_core::embed::{Diag, TurnSink, TurnSource, TurnStart};
use gait_core::proto::gait::log::v1::{stream_item, StreamItem, TextDelta};
use gait_proto::gait::sandbox::v1::{signal, Signal, StreamForward};

use gait_workflow::convert;
use gait_workflow::observer::NullObserver;
use gait_workflow::runner::{run_workflow, WorkflowOutcome, WorkflowRunOpts};
use gait_workflow::schema::WorkflowDef;

use tempfile::TempDir;

const DELTA_TEXT: &str = "hello from the brain\n";

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

/// A `StreamForward` Nerve signal carrying one text delta — the spine relays it
/// straight to `sink.item(&StreamItem::Delta)`, the same call the REPL renders.
fn delta_signal(content: &str) -> Signal {
    Signal {
        corr_id: 0,
        body: Some(signal::Body::StreamForward(StreamForward {
            item: Some(StreamItem {
                item: Some(stream_item::Item::Delta(TextDelta {
                    content: content.into(),
                })),
            }),
        })),
    }
}

/// Caller sink: flips `saw_delta` the instant a text delta is replayed onto it,
/// and keeps every delta's text for a post-run content assertion.
struct WatchSink {
    saw_delta: Arc<AtomicBool>,
    deltas: Arc<Mutex<Vec<String>>>,
}

impl TurnSink for WatchSink {
    fn item(&mut self, item: &StreamItem) {
        if let Some(stream_item::Item::Delta(d)) = &item.item {
            self.deltas.lock().unwrap().push(d.content.clone());
            self.saw_delta.store(true, Ordering::SeqCst);
        }
    }
    fn result(&mut self, _r: &gait_core::proto::gait::log::v1::RunResult) {}
    fn session_id(&mut self, _id: &str) {}
    fn diagnostic(&mut self, _level: Diag, _msg: &str) {}
}

/// Live source standing in for the REPL's line reader:
///   turn 1  → one user message ("run the turn").
///   turn 2  → the *liveness probe*: yield the task until the caller sink has
///             already seen the delta, then quit (`None`). If nothing drains the
///             channel while we're parked here, `saw_delta` never flips and we
///             record the failure via `observed_live = false`.
struct ProbeSource {
    calls: usize,
    saw_delta: Arc<AtomicBool>,
    observed_live: Arc<AtomicBool>,
}

#[async_trait::async_trait(?Send)]
impl TurnSource for ProbeSource {
    async fn next(&mut self) -> Option<TurnStart> {
        self.calls += 1;
        if self.calls == 1 {
            let body = gait_core::proto::events::user_text("run the turn".into());
            return Some(TurnStart::Input {
                bodies: vec![(body, None).into()],
                on_durable: None,
            });
        }
        // Bounded park: on the buggy (await-then-drain) path nothing is draining
        // the frame channel while we sit here, so this spins its budget and gives
        // up with the flag still false. On the fixed (concurrent-drain) path the
        // very first yield lets the runner replay the buffered delta, flipping the
        // flag within a couple iterations.
        for _ in 0..2000 {
            if self.saw_delta.load(Ordering::SeqCst) {
                break;
            }
            tokio::task::yield_now().await;
        }
        self.observed_live
            .store(self.saw_delta.load(Ordering::SeqCst), Ordering::SeqCst);
        None
    }
}

/// The interactive prompt node's streamed reply must reach the caller's sink
/// while the session is still running — not batched out after it ends.
#[tokio::test]
async fn interactive_reply_renders_live_not_after_session_ends() {
    let ws = TempDir::new().unwrap();
    let gait_dir = ws.path().to_path_buf();
    init_git_repo(&gait_dir);
    std::fs::create_dir_all(gait_dir.join(".active")).unwrap();
    std::fs::create_dir_all(gait_dir.join("workflow-runs")).unwrap();

    // A model must resolve (ConfigResolver::resolve_checked); the mock brain
    // ignores it. Sandbox stays off (mock_brain doesn't self-seal).
    std::fs::create_dir_all(gait_dir.join("inferences/mock")).unwrap();
    std::fs::write(
        gait_dir.join("inferences/mock/inference.textproto"),
        "model: \"mock/model\"\ncompaction_threshold_tokens: 128000\n",
    )
    .unwrap();
    std::fs::write(gait_dir.join("gait.jsonc"), r#"{ "inference": "mock" }"#).unwrap();

    // Prebake the brain's single turn: stream one delta, then finish.
    common::write_frames(
        &gait_dir,
        &[
            common::append_assistant("done"),
            delta_signal(DELTA_TEXT),
            common::terminated_ok("done"),
        ],
    );
    std::env::set_var("GAIT_BRAIN_EXE", env!("CARGO_BIN_EXE_mock_brain"));

    let saw_delta = Arc::new(AtomicBool::new(false));
    let observed_live = Arc::new(AtomicBool::new(false));
    let deltas = Arc::new(Mutex::new(Vec::<String>::new()));

    let mut sink = WatchSink {
        saw_delta: saw_delta.clone(),
        deltas: deltas.clone(),
    };
    let source = ProbeSource {
        calls: 0,
        saw_delta: saw_delta.clone(),
        observed_live: observed_live.clone(),
    };

    // One interactive (empty-prompt) node — the REPL's degenerate workflow.
    let wf: WorkflowDef =
        serde_yaml::from_str("name: chat\nnodes:\n  - id: chat\n    prompt: \"\"\n").unwrap();

    let outcome = run_workflow(
        WorkflowRunOpts {
            workflow_def: convert::to_proto(&wf),
            yaml_path: String::new(),
            run_id: "chat-live-render".into(),
            gait_dir: gait_dir.clone(),
            codebase: gait_dir.clone(),
            args: vec![],
            sandbox_on: Some(false),
            ..Default::default()
        },
        &mut sink,
        &mut NullObserver,
        Some(Box::new(source)),
    )
    .await
    .expect("run_workflow");

    assert_eq!(
        outcome,
        WorkflowOutcome::NodeOpen,
        "interactive node must leave the run open"
    );
    assert!(
        observed_live.load(Ordering::SeqCst),
        "streamed reply must reach the caller sink WHILE the session is live \
         (before the source quits) — it was still buffered in the workflow channel"
    );
    assert!(
        deltas
            .lock()
            .unwrap()
            .iter()
            .any(|d| d.contains("hello from the brain")),
        "the streamed delta text must have been rendered: {:?}",
        deltas.lock().unwrap()
    );
    // The reply text arrived as a delta whose newline closes a REPL block — i.e.
    // the view WILL render it as a Block (see gait-repl harness unit tests for the
    // block-rendering proof; here we assert the pipe delivers renderable content).
    assert!(
        deltas
            .lock()
            .unwrap()
            .iter()
            .any(|d| d.contains("hello from the brain") && d.contains('\n')),
        "e2e: streamed reply must be renderable block content: {:?}",
        deltas.lock().unwrap()
    );
}

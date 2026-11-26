//! Script-workflow live-render harness (regression for "I'm not seeing the
//! reply").
//!
//! A workflow turn's streamed reply is rendered through the workflow's
//! `SinkFrame` channel, and the runner drains that channel into the caller's
//! real sink. The bug: the turn branch awaited the node to *completion* before
//! draining, so every streamed delta sat in the channel buffer until the run
//! ended — the reply never rendered live.
//!
//! This drives the real script-engine path end-to-end (`launch::run_named`)
//! with `mock_brain` (via the `GAIT_BRAIN_EXE` seam) emitting one streamed
//! delta. The sink writes an empty marker file the instant a delta is replayed
//! onto it, and the *next* script step (`probe`) polls for that marker. If the
//! delta only drains after the run ends, the probe times out and the run fails
//! — so a `Completed` outcome with `probe` `Done` proves the reply reached the
//! sink WHILE the run was still going, not after it ended.

mod common;

use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use gait_core::embed::{Diag, TurnSink};
use gait_core::proto::gait::log::v1::{stream_item, StreamItem, TextDelta};
use gait_proto::gait::sandbox::v1::{signal, Signal, StreamForward};

use gait_workflow::{
    launch::{run_named, LaunchEnv},
    observer::NullObserver,
    run_log::WorkflowRunLog,
    runner::WorkflowOutcome,
    state::{reconstruct_state, NodeStatus},
};

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
/// straight to `sink.item(&StreamItem::Delta)`.
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

/// Caller sink: the instant a text delta is replayed onto it, it touches the
/// marker file — the in-run liveness signal for the `probe` step — and keeps
/// every delta's text for a post-run content assertion.
struct WatchSink {
    marker: std::path::PathBuf,
    deltas: Arc<Mutex<Vec<String>>>,
}

impl WatchSink {
    fn touch_marker(&self) {
        if let Err(e) = std::fs::File::create(&self.marker) {
            eprintln!("failed to touch marker {:?}: {e}", self.marker);
        }
    }
}

impl TurnSink for WatchSink {
    fn item(&mut self, item: &StreamItem) {
        if let Some(stream_item::Item::Delta(d)) = &item.item {
            self.deltas.lock().unwrap().push(d.content.clone());
            self.touch_marker();
        }
    }
    fn result(&mut self, _r: &gait_core::proto::gait::log::v1::RunResult) {}
    fn session_id(&mut self, _id: &str) {}
    fn diagnostic(&mut self, _level: Diag, _msg: &str) {}
}

/// A workflow turn's streamed reply must reach the caller's sink WHILE the run
/// is still going — not batched out after it ends.
#[tokio::test]
async fn workflow_turn_reply_renders_live_not_after_the_run_ends() {
    let gait = TempDir::new().unwrap();
    let gait_dir = gait.path().to_path_buf();
    init_git_repo(&gait_dir);
    std::fs::create_dir_all(gait_dir.join(".active")).unwrap();
    std::fs::create_dir_all(gait_dir.join("workflow-runs")).unwrap();

    // The engine refuses a gait dir inside the codebase and runs script turns
    // with the sandbox on: use a separate codebase dir that is its own git repo.
    let codebase = TempDir::new().unwrap();
    init_git_repo(codebase.path());

    // Marker lives inside the codebase so the sandboxed `probe` step can see it.
    let marker = codebase.path().join(format!(
        "live-render-marker-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));

    // A model must resolve (ConfigResolver::resolve_checked); the mock brain
    // ignores it.
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

    // The workflow: the chat turn streams the reply (sink touches the marker),
    // then the sandboxed `probe` step polls for the marker. If nothing drains
    // the frame channel while the run is live, the marker never appears and
    // `probe` exits non-zero — failing the run.
    let script = r#"
        await gait.wf.config({ name: "live" });
        await gait.wf.turn({ id: "chat", prompt: "run the turn" });
        await gait.wf.shell({ id: "probe", script: `for i in $(seq 100)
do
  [ -f "$MARKER" ] && exit 0
  sleep 0.05
done
exit 1`, env: { MARKER: gait.wf.run.args[0] } });
    "#;
    let wf_dir = gait_dir.join("workflows");
    std::fs::create_dir_all(&wf_dir).unwrap();
    std::fs::write(wf_dir.join("live.js"), script).unwrap();

    let deltas = Arc::new(Mutex::new(Vec::<String>::new()));
    let mut sink = WatchSink {
        marker: marker.clone(),
        deltas: deltas.clone(),
    };

    let mut started = |_: &str, _: &str| {};
    let (run_id, outcome) = run_named(
        "live",
        vec![marker.to_string_lossy().into_owned()],
        LaunchEnv {
            gait_dir: gait_dir.clone(),
            codebase: codebase.path().to_path_buf(),
            started: &mut started,
            sink: &mut sink,
            observer: &mut NullObserver,
        },
    )
    .await
    .expect("run_named");

    assert_eq!(
        outcome,
        WorkflowOutcome::Completed,
        "the reply must reach the sink while the run is live — a buffered-until-\
         after-the-run drain never touches the marker, so `probe` fails the run"
    );
    let run_log = WorkflowRunLog::open(&gait_dir.join("workflow-runs"), &run_id).unwrap();
    let state = reconstruct_state(&run_log.read_events().unwrap());
    assert_eq!(
        state.node_status("probe"),
        NodeStatus::Done,
        "`probe` saw the marker inside the run: the streamed reply was replayed \
         onto the caller sink WHILE the run was still going"
    );

    // The delta content assertions: the reply text was rendered, and it arrived
    // as a delta whose newline closes a REPL block (i.e. renderable content).
    assert!(
        deltas
            .lock()
            .unwrap()
            .iter()
            .any(|d| d.contains("hello from the brain")),
        "the streamed delta text must have been rendered: {:?}",
        deltas.lock().unwrap()
    );
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

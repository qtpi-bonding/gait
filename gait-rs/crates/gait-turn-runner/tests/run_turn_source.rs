// crates/gait-turn-runner/tests/run_turn_source.rs
//
// Integration test for the shared sealed-turn runner extracted in
// gait-min-composition Task 2. Drives `run_turn_source` (via a one-turn
// source, exactly the shape a live ACP/REPL source degenerates to for a
// single turn) and `run_single_prompt` (the CLI-shaped literal-task entry
// point) against a real gait-dir + `GAIT_BRAIN_EXE=mock_brain`, and asserts
// the runner (a) emits a final, completed `TurnRunOutcome` and (b) honors
// the caller-supplied `session_id` — the field an earlier draft of this
// extraction reportedly dropped. `run_id`/`continue_session` are accepted on
// `TurnRunOpts` (Task 3's call sites always set them) but are not consumed
// by `run_loop`/`relay_brain_turn` themselves (verified against
// `src/turn/run_lifecycle.rs`, which mints its own `RunResult.run_id` via
// `uuid::Uuid::new_v4()`); this test only proves they're accepted, not that
// they flow anywhere.
//
// Requires `--features dev-harness` to build (wires up `mock_brain` and
// `env!("CARGO_BIN_EXE_mock_brain")`), same as Task 1 found for
// `gait-cli`/`gait-repl`.

use std::path::PathBuf;

use gait_core::proto::envelope;
use gait_core::proto::gait::log::v1 as pb;
use gait_core::proto::gait::sandbox::v1::{signal, Append, FinalizedTurn, Signal};
use gait_core::embed::{Diag, TurnSink};
use gait_turn_runner::{run_single_prompt, run_turn_source, OneTurnSource, TurnRunOpts};
use prost::Message;

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

// [version u8][len u32 BE][len bytes] — the mock brain's frame-blob wire
// format (see `gait-testkit/src/mock_brain.rs::read_one_frame`).
fn frame(sig: &Signal) -> Vec<u8> {
    let b = sig.encode_to_vec();
    let mut f = vec![1u8];
    f.extend_from_slice(&(b.len() as u32).to_be_bytes());
    f.extend_from_slice(&b);
    f
}

fn append_assistant(msg: &str) -> Signal {
    let event = envelope::mint(
        gait_core::proto::events::assistant_text(msg.into()),
        None,
        envelope::EventScope::Step {
            run: 0,
            turn: 0,
            step: 0,
        },
    );
    Signal {
        corr_id: 0,
        body: Some(signal::Body::Append(Append { event: Some(event) })),
    }
}

fn terminated_ok(msg: &str) -> Signal {
    use gait_core::proto::gait::sandbox::v1::{FinalizedTurnOutcome, Terminated};
    Signal {
        corr_id: 0,
        body: Some(signal::Body::Terminated(Terminated {
            turn: Some(FinalizedTurn {
                outcome: FinalizedTurnOutcome::TextDone as i32,
                message: msg.into(),
                ..Default::default()
            }),
            failed: false,
        })),
    }
}

fn setup_gait_dir() -> tempfile::TempDir {
    let ws = tempfile::tempdir().unwrap();
    let gait_dir = ws.path().to_path_buf();
    init_git_repo(&gait_dir);
    std::fs::create_dir_all(gait_dir.join(".active")).unwrap();
    std::fs::create_dir_all(gait_dir.join("inferences/mock")).unwrap();
    std::fs::write(
        gait_dir.join("inferences/mock/INFERENCE.yaml"),
        "model: mock/model\ncompaction_threshold_tokens: 128000\n",
    )
    .unwrap();
    std::fs::write(gait_dir.join("gait.jsonc"), r#"{ "inference": "mock" }"#).unwrap();

    let mut blob = Vec::new();
    for s in [append_assistant("done"), terminated_ok("done")] {
        blob.extend_from_slice(&frame(&s));
    }
    std::fs::write(gait_dir.join("mock_frames.bin"), blob).unwrap();

    let auth_home = ws.path().join("gait-home");
    std::fs::create_dir_all(&auth_home).unwrap();
    std::env::set_var("GAIT_HOME", &auth_home);
    std::env::set_var(
        "GAIT_BRAIN_EXE",
        PathBuf::from(env!("CARGO_BIN_EXE_mock_brain")),
    );

    ws
}

fn test_opts(run_id: &str, session_id: &str, gait_dir: &std::path::Path) -> TurnRunOpts {
    TurnRunOpts {
        run_id: run_id.to_string(),
        session_id: session_id.to_string(),
        continue_session: session_id.to_string(),
        gait_dir: gait_dir.to_path_buf(),
        codebase: gait_dir.to_path_buf(),
        args: vec![],
        sandbox_on: None,
        cascade_group_arg: None,
        cascade: None,
        profile: None,
        fs_delegate: None,
        allow_intree: true,
    }
}

#[derive(Default)]
struct RecordingSink {
    diagnostics: Vec<(Diag, String)>,
}

impl TurnSink for RecordingSink {
    fn item(&mut self, _item: &pb::StreamItem) {}
    fn result(&mut self, _r: &pb::RunResult) {}
    fn session_id(&mut self, _id: &str) {}
    fn diagnostic(&mut self, level: Diag, msg: &str) {
        self.diagnostics.push((level, msg.to_string()));
    }
}

#[tokio::test]
async fn single_prompt_finishes_through_the_shared_runner() {
    let ws = setup_gait_dir();
    let gait_dir = ws.path().to_path_buf();
    let mut sink = RecordingSink::default();

    let outcome = run_single_prompt(
        test_opts("run-1", "s1", &gait_dir),
        Some("hello".to_string()),
        &mut sink,
    )
    .await
    .unwrap();

    assert!(
        outcome.completed,
        "the shared runner must report the turn as completed: {:?}, diagnostics: {:?}",
        outcome.text, sink.diagnostics
    );
    assert!(!outcome.opened);
    assert_eq!(outcome.text.as_deref(), Some("done"));

    // The runner must have honored the requested session id: the turn's
    // events land under session "s1", not some other/derived id.
    let events = gait_core::session::Ingestor::from_session(&gait_dir, "s1")
        .events()
        .expect("read session events");
    let user_texts = events
        .iter()
        .filter(|e| {
            matches!(
                gait_core::proto::event_kind(e),
                Some(pb::payload::Kind::UserText(_))
            )
        })
        .count();
    assert_eq!(
        user_texts, 1,
        "exactly one user_turn event must be recorded under the requested session id: {events:?}"
    );
}

#[tokio::test]
async fn run_turn_source_finishes_a_live_one_turn_source() {
    let ws = setup_gait_dir();
    let gait_dir = ws.path().to_path_buf();
    let mut sink = RecordingSink::default();

    let repos = gait_core::embed::Repos::production();
    let bodies = gait_core::embed::user_turn_events(&repos, &gait_dir, "hello".to_string());
    let source = Box::new(OneTurnSource {
        session_updates: None,
        bodies: Some(bodies),
    });

    let outcome = run_turn_source(test_opts("run-2", "s2", &gait_dir), source, &mut sink)
        .await
        .unwrap();

    assert!(
        outcome.completed,
        "the shared runner must report the turn as completed: {:?}, diagnostics: {:?}",
        outcome.text, sink.diagnostics
    );
    assert_eq!(outcome.text.as_deref(), Some("done"));
}

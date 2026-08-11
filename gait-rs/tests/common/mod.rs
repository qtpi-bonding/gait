#![allow(dead_code)]
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use gait_core::embed::{
    run_loop, BrainLaunch, CascadeKillHandle, Diag, LoopConfig, RecordingSink, Session, TurnDriver,
    TurnSink,
};
use gait_core::git::{checkpoint::CheckpointRepo, codebase::CodebaseRepo, trace::TraceRepo, Repos};
use gait_core::port::input::ArgSource;
use gait_proto::gait::sandbox::v1::{signal, Signal, Terminated};
use prost::Message;

// ── persistence-call recorder ───────────────────────────────────────────────
#[derive(Clone, Default)]
pub struct Calls(Arc<Mutex<Vec<String>>>);
impl Calls {
    fn push(&self, s: String) {
        self.0.lock().unwrap().push(s);
    }
    pub fn snapshot(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }
}

struct RecTrace(Calls);
impl TraceRepo for RecTrace {
    fn init_workspace(&self, _g: &Path) -> anyhow::Result<()> {
        Ok(())
    }
    fn create_session_worktree(&self, gait_dir: &Path, session_id: &str) -> anyhow::Result<()> {
        // real mkdir so the FS-backed log.jsonl + SessionLock land — but no git.
        // Nested one level (see gait_core::session::live) to match the real
        // create_session_worktree's .active/<id>/sessions/<id>/ shape.
        std::fs::create_dir_all(
            gait_core::session::live::log_path(gait_dir, session_id)
                .parent()
                .unwrap(),
        )?;
        self.0.push(format!("create_session_worktree:{session_id}"));
        Ok(())
    }
    fn commit_session(&self, _g: &Path, session_id: &str, message: &str) -> anyhow::Result<()> {
        self.0
            .push(format!("commit_session:{session_id}:{message}"));
        Ok(())
    }
    fn list_active_sessions(&self, _g: &Path) -> anyhow::Result<Vec<String>> {
        Ok(vec![])
    }
    fn finish_session(&self, _g: &Path, _s: &str) -> anyhow::Result<()> {
        Ok(())
    }
    // Not exercised in a single-turn path — loud tripwire if a refactor starts
    // calling them.
    fn create_branch_worktree(&self, _: &Path, _: &str, _: &str, _: &Path) -> anyhow::Result<()> {
        unimplemented!("not used in turn path")
    }
    fn remove_branch_worktree(&self, _: &Path, _: &Path) -> anyhow::Result<()> {
        unimplemented!("not used in turn path")
    }
    fn merge_branch_ff(&self, _: &Path, _: &str, _: &str) -> anyhow::Result<()> {
        unimplemented!("not used in turn path")
    }
    fn discard_session(&self, _gait_dir: &Path, _session_id: &str) -> anyhow::Result<()> {
        Ok(())
    }
}

struct RecCheckpoint(Calls);
impl CheckpointRepo for RecCheckpoint {
    fn init(&self, _g: &Path, session_id: &str, _c: &Path) -> anyhow::Result<()> {
        self.0.push(format!("checkpoint.init:{session_id}"));
        Ok(())
    }
    fn checkpoint(
        &self,
        _g: &Path,
        session_id: &str,
        _c: &Path,
        boundary: gait_core::git::checkpoint::CheckpointBoundary,
    ) -> anyhow::Result<String> {
        self.0.push(format!("checkpoint:{session_id}:{boundary}"));
        Ok("a".repeat(40))
    }
    fn restore_to(&self, _: &Path, _: &str, _: &Path, _: &str) -> anyhow::Result<()> {
        unimplemented!("not used in turn path")
    }
    fn remove(&self, _: &Path, _: &str) -> anyhow::Result<()> {
        unimplemented!("not used in turn path")
    }
    fn diff_between(
        &self,
        _: &Path,
        _: &str,
        _: &str,
        _: &str,
    ) -> anyhow::Result<Option<gait_proto::gait::log::v1::ChangeSummary>> {
        Ok(Some(Default::default()))
    }
}

struct RecCodebase(Calls);
impl CodebaseRepo for RecCodebase {
    fn head_hash(&self, _c: &Path) -> Option<String> {
        Some("0".repeat(40))
    }
    fn dirty_files(&self, _c: &Path) -> Vec<String> {
        vec![]
    }
}

pub fn mock_repos(calls: &Calls) -> Repos {
    Repos {
        codebase: Arc::new(RecCodebase(calls.clone())),
        traces: Arc::new(RecTrace(calls.clone())),
        checkpoints: Arc::new(RecCheckpoint(calls.clone())),
    }
}

// ── frame helpers ───────────────────────────────────────────────────────────
pub fn frame(sig: &Signal) -> Vec<u8> {
    let b = sig.encode_to_vec();
    let mut f = vec![1u8];
    f.extend_from_slice(&(b.len() as u32).to_be_bytes());
    f.extend_from_slice(&b);
    f
}

pub fn terminated_ok(msg: &str) -> Signal {
    Signal {
        corr_id: 0,
        body: Some(signal::Body::Terminated(Terminated {
            turn: Some(gait_proto::gait::sandbox::v1::FinalizedTurn {
                outcome: gait_proto::gait::sandbox::v1::FinalizedTurnOutcome::TextDone as i32,
                message: msg.into(),
                ..Default::default()
            }),
            failed: false,
        })),
    }
}

pub fn append_assistant(msg: &str) -> Signal {
    let event = gait_proto::envelope::mint(
        gait_core::proto::events::assistant_text(msg.into()),
        None,
        gait_proto::envelope::EventScope::Step {
            run: 0,
            turn: 0,
            step: 0,
        },
    );
    Signal {
        corr_id: 0,
        body: Some(signal::Body::Append(
            gait_proto::gait::sandbox::v1::Append { event: Some(event) },
        )),
    }
}

pub fn terminated_fail(msg: &str) -> Signal {
    Signal {
        corr_id: 0,
        body: Some(signal::Body::Terminated(Terminated {
            turn: Some(gait_proto::gait::sandbox::v1::FinalizedTurn {
                outcome: gait_proto::gait::sandbox::v1::FinalizedTurnOutcome::Failed as i32,
                failure_reason: "mock_failure".into(),
                message: msg.into(),
                ..Default::default()
            }),
            failed: true,
        })),
    }
}

pub fn write_frames(gait_dir: &Path, signals: &[Signal]) {
    // Every mock-driven turn selects the named `mock` inference.  Keep this
    // fixture setup alongside its input frames so lower-level loop tests that
    // do not use `run_one` are valid as well.
    std::fs::create_dir_all(gait_dir.join("inferences/mock")).unwrap();
    std::fs::write(
        gait_dir.join("inferences/mock/INFERENCE.yaml"),
        "model: mock/model\n",
    )
    .unwrap();
    let mut blob = Vec::new();
    for s in signals {
        blob.extend_from_slice(&frame(s));
    }
    std::fs::write(gait_dir.join("mock_frames.bin"), blob).unwrap();
}

pub fn set_delay(gait_dir: &Path, ms: u64) {
    std::fs::write(gait_dir.join("mock_delay_ms"), ms.to_string()).unwrap();
}
/// Sleep AFTER frames are sent — arms the watchdog then goes silent.
pub fn set_hang(gait_dir: &Path, ms: u64) {
    std::fs::write(gait_dir.join("mock_hang_ms"), ms.to_string()).unwrap();
}

// ── driver + run ────────────────────────────────────────────────────────────
pub struct MockDriver {
    pub codebase: PathBuf,
    pub gait_bin: PathBuf,
}

#[async_trait::async_trait(?Send)]
impl TurnDriver for MockDriver {
    fn brain_launch(&self, _session_id: &str) -> BrainLaunch {
        BrainLaunch::new(
            self.codebase.clone(),
            self.gait_bin.clone(),
            Default::default(),
            /*sandbox_on*/ false,
            /*auto*/ true,
            /*max_iter*/ None,
            /*cascade_group_arg*/ None,
            /*agent*/ None,
            /*inference*/ Some("mock".into()),
            /*explicit_inference_override*/ false,
            /*allow*/ vec![],
            /*deny*/ vec![],
            /*result_schema*/ None,
        )
    }
    fn turn_config(&mut self) -> LoopConfig {
        // Mirror the brain's CliDriver `turn_config()` (turn/brain.rs:494) with
        // fields the spine consumes. `LoopConfig` has no Default impl.
        LoopConfig {
            auto: true,
            max_iter: None,
            cancel: None,
            preview_policy: Default::default(),
            sandbox_on: false,
            sandbox_cfg: Default::default(),
            relay: None,
            initial_git: None,
            daemons: vec![],
        }
    }
    fn on_turn_error(&mut self, sink: &mut dyn TurnSink, e: anyhow::Error) -> ControlFlow<()> {
        sink.diagnostic(Diag::Error, &format!("{e}"));
        ControlFlow::Break(())
    }
}

pub async fn run_one(
    gait_dir: &Path,
    session_id: &str,
    signals: &[Signal],
) -> (RecordingSink, Calls) {
    write_frames(gait_dir, signals);
    let mock = PathBuf::from(env!("CARGO_BIN_EXE_mock_brain"));
    let calls = Calls::default();
    let session = Arc::new(Session {
        id: session_id.into(),
        gait_dir: gait_dir.to_path_buf(),
        codebase_path: gait_dir.to_path_buf(),
        assigned_by: None,
        repos: mock_repos(&calls), // ← mock git, not Repos::production()
    });
    let mut sink = RecordingSink::default();
    let mut driver = MockDriver {
        codebase: gait_dir.to_path_buf(),
        gait_bin: mock,
    };
    let source = ArgSource::new(None); // one Resume then EOF
    let cascade = CascadeKillHandle::new();
    run_loop(
        source,
        &mut sink,
        session,
        gait_dir,
        session_id,
        &mut driver,
        Some(&cascade),
    )
    .await
    .expect("run_loop");
    (sink, calls)
}

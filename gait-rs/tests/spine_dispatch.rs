//! Task 9: `run_loop` dispatch characterization.
//!
//! Pins the routing in `embed::run_loop` (embed.rs:204-231):
//!   - `Ok(false)` (brain success) → `after_turn` fires, `on_turn_error` does NOT.
//!   - `Ok(true)`  (brain failed terminal) → `on_turn_error` fires, `after_turn` does NOT.
//!
//! `SpyDriver` wraps `MockDriver` and records which hook fired. Hooks share
//! `Rc<RefCell<bool>>` flags; the driver itself does not need `Sync`.

mod common;
use common::*;

use gait_core::embed::{BrainLaunch, LoopConfig, TurnDriver, TurnSink};
use gait_core::turn::TurnResult;
use std::cell::RefCell;
use std::ops::ControlFlow;
use std::path::PathBuf;
use std::rc::Rc;

/// Driver that records which post-turn hook `run_loop` invoked.
struct SpyDriver {
    inner: MockDriver,
    after_called: Rc<RefCell<bool>>,
    error_called: Rc<RefCell<bool>>,
}

#[async_trait::async_trait(?Send)]
impl TurnDriver for SpyDriver {
    fn brain_launch(&self, session_id: &str) -> BrainLaunch {
        self.inner.brain_launch(session_id)
    }
    fn turn_config(&mut self) -> LoopConfig {
        self.inner.turn_config()
    }
    async fn after_turn(&mut self, sink: &mut dyn TurnSink, result: &TurnResult) {
        *self.after_called.borrow_mut() = true;
        // Mirror the default impl: do nothing observable. Don't return Continue/Break;
        // `after_turn` returns `()` in the trait.
        let _ = (sink, result);
    }
    fn on_turn_error(&mut self, sink: &mut dyn TurnSink, e: anyhow::Error) -> ControlFlow<()> {
        *self.error_called.borrow_mut() = true;
        // Behave like MockDriver: emit a diagnostic and break the loop.
        sink.diagnostic(gait_core::embed::Diag::Error, &format!("{e}"));
        ControlFlow::Break(())
    }
}

/// Lower-level helper: drive `run_loop` with a caller-supplied `SpyDriver`.
/// Mirrors `run_one` but lets the test inject the driver (so the spy flags survive).
async fn run_with_spy(
    gait_dir: &std::path::Path,
    session_id: &str,
    signals: &[gait_proto::gait::sandbox::v1::Signal],
    after_called: Rc<RefCell<bool>>,
    error_called: Rc<RefCell<bool>>,
) {
    write_frames(gait_dir, signals);
    let mock = PathBuf::from(env!("CARGO_BIN_EXE_mock_brain"));
    let calls = Calls::default();
    let session = std::sync::Arc::new(gait_core::embed::Session {
        id: session_id.into(),
        gait_dir: gait_dir.to_path_buf(),
        codebase_path: gait_dir.to_path_buf(),
        assigned_by: None,
        repos: mock_repos(&calls),
    });
    let mut sink = RecordingSink::default();
    let mut driver = SpyDriver {
        inner: MockDriver {
            codebase: gait_dir.to_path_buf(),
            gait_bin: mock,
        },
        after_called,
        error_called,
    };
    let source = gait_core::port::input::ArgSource::new(None);
    let cascade = CascadeKillHandle::new();
    gait_core::embed::run_loop(
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
}

#[tokio::test]
async fn success_calls_after_turn_not_on_error() {
    // terminated_ok → relay returns Ok(false) → after_turn fires; on_turn_error must NOT.
    let tmp = tempfile::tempdir().unwrap();
    let after_called = Rc::new(RefCell::new(false));
    let error_called = Rc::new(RefCell::new(false));
    run_with_spy(
        tmp.path(),
        "d1",
        &[terminated_ok("done")],
        after_called.clone(),
        error_called.clone(),
    )
    .await;

    assert!(
        *after_called.borrow(),
        "after_turn must fire on Ok(false); error_called={}",
        *error_called.borrow()
    );
    assert!(
        !*error_called.borrow(),
        "on_turn_error must NOT fire on Ok(false); after_called={}",
        *after_called.borrow()
    );
}

#[tokio::test]
async fn brain_failure_calls_on_turn_error() {
    // terminated_fail → relay returns Ok(true) → on_turn_error fires.
    let tmp = tempfile::tempdir().unwrap();
    let after_called = Rc::new(RefCell::new(false));
    let error_called = Rc::new(RefCell::new(false));
    run_with_spy(
        tmp.path(),
        "d2",
        &[terminated_fail("boom")],
        after_called.clone(),
        error_called.clone(),
    )
    .await;

    assert!(
        *error_called.borrow(),
        "on_turn_error must fire on Ok(true); after_called={}",
        *after_called.borrow()
    );
    assert!(
        !*after_called.borrow(),
        "after_turn must NOT fire on Ok(true); error_called={}",
        *error_called.borrow()
    );
}

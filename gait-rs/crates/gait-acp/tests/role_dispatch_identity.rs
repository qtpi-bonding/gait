//! The worker spawns `current_exe() __brain`, and the brain folds signed events,
//! so the agent identity must be loaded (and the process verifier installed)
//! BEFORE role argv dispatch.

use std::process::{Command, Stdio};

#[test]
fn role_argv_loads_the_agent_identity_before_dispatching() {
    let empty = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_gait-acp"))
        .arg("__brain")
        .env("GAIT_AGENT_DIR", empty.path())
        .stdin(Stdio::null())
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "a role process with a broken identity must not start");
    assert!(
        stderr.contains("load agent identity"),
        "the identity must be loaded before the role is dispatched, got: {stderr}"
    );
}

//! A loose identity directory is refused at startup with one coded error that
//! names the path, the observed mode and the Wave D script, and a normal
//! non-zero exit (no panic, no chmod).

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};

fn assert_refused(dir: &Path, agent_dir_env: Option<&Path>, state: &Path) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_gait-acp"));
    cmd.arg("--state-dir").arg(state).env_remove("GAIT_DIR").stdin(Stdio::null());
    match agent_dir_env {
        Some(path) => cmd.env("GAIT_AGENT_DIR", path),
        None => cmd.env_remove("GAIT_AGENT_DIR"),
    };
    let output = cmd.output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(65), "config-class exit, not a panic (101): {stderr}");
    assert!(stderr.contains(&dir.display().to_string()), "path: {stderr}");
    assert!(stderr.contains("0755"), "mode: {stderr}");
    assert!(stderr.contains("run the Wave D permissions-and-replay migration script"), "{stderr}");
    assert!(!stderr.contains("panicked"), "{stderr}");
    let mode = std::fs::metadata(dir).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o755, "never chmods");
}

#[test]
fn gait_acp_refuses_a_loose_minted_agent_dir() {
    let gait = tempfile::tempdir().unwrap();
    let ctx = gait_identity::IdentityContext::mint_or_load(gait.path()).unwrap();
    let dir = ctx.agent_dir().to_path_buf();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_refused(&dir, None, gait.path());
}

#[test]
fn gait_acp_refuses_a_loose_agent_dir_named_by_the_environment() {
    let gait = tempfile::tempdir().unwrap();
    let ctx = gait_identity::IdentityContext::mint_or_load(gait.path()).unwrap();
    let dir = ctx.agent_dir().to_path_buf();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_refused(&dir, Some(&dir), gait.path());
}

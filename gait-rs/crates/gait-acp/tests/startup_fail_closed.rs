//! A loose or old-layout attestation state is refused at startup with one
//! coded, human-readable error and a normal non-zero exit (no panic, no
//! chmod, no migration).

use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};

#[test]
fn gait_acp_refuses_a_loose_attestation_dir_and_exits_once() {
    let gait = tempfile::tempdir().unwrap();
    let ctx = gait_identity::IdentityContext::mint_or_load(gait.path()).unwrap();
    let dir = gait
        .path()
        .join("attestation")
        .join(ctx.agent_id().to_string());
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(dir.join("replay.json"), br#"{"a":{"b":5}}"#).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_gait-acp"))
        .arg("--require-attestation")
        .arg("--state-dir")
        .arg(gait.path())
        .env_remove("GAIT_AGENT_DIR")
        .env_remove("GAIT_DIR")
        .stdin(Stdio::null())
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(65),
        "config-class exit, not a panic (101): {stderr}"
    );
    assert!(stderr.contains(&dir.display().to_string()), "{stderr}");
    assert!(stderr.contains("0755"), "{stderr}");
    assert!(
        stderr.contains(
            "tighten the permissions (chmod 700 for a directory, chmod 600 for a file) and retry"
        ),
        "{stderr}"
    );
    assert!(!stderr.contains("panicked"), "{stderr}");
    let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o755, "never chmods");
}

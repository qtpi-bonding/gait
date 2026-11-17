// Real-kernel check that the hand seals before any other thread exists.
//
// Landlock confines only the thread that calls `restrict_self` and the
// children that thread creates afterwards. A thread that already exists stays
// unconfined, and a shell spawned from it escapes. The hand therefore runs on
// a current-thread runtime and seals first. Run on a Landlock kernel with a
// fresh binary:
//   GAIT_BIN=<target>/debug/gait cargo test --test landlock_thread_scope -- --ignored

#[cfg(target_os = "linux")]
mod tests {
    use gait_core::proto::gait::sandbox::v1::{
        hand_response, network_scope::Mode, Feedback, HandResponse, Impulse, NetworkScope,
        OutputCaptureMode, OutputPresentation, Scope,
    };
    use prost::Message as _;
    use std::io::Write as _;
    use std::process::{Command, Stdio};

    fn gait_bin() -> std::path::PathBuf {
        std::env::var("GAIT_BIN")
            .map(Into::into)
            .unwrap_or_else(|_| "gait".into())
    }

    fn shell_impulse(readable: &[&std::path::Path], cmd: &str) -> Impulse {
        let cb = readable[0].to_string_lossy().into_owned();
        Impulse {
            tool: "shell".into(),
            args: Some(
                serde_json::from_value(serde_json::json!({ "cmd": cmd, "reason": "thread scope test" }))
                    .expect("valid shell args"),
            ),
            requested: Some(Scope {
                read_all: false,
                readable: readable.iter().map(|p| p.to_string_lossy().into_owned()).collect(),
                writable: vec![cb],
                network: Some(NetworkScope { mode: Mode::Deny as i32, hosts: vec![] }),
                local_bind: false,
                read_deny: None,
                write_deny: None,
            }),
            prefetch: None,
            shell_whitelist_patterns: vec![],
            output_capture_mode: OutputCaptureMode::Spool as i32,
            output_presentation: OutputPresentation::Unspecified as i32,
        }
    }

    /// Run the real hand and return its command output.
    fn run_hand(codebase: &std::path::Path, impulse: Impulse) -> String {
        let spool = tempfile::NamedTempFile::new_in(codebase).unwrap();
        let cb = codebase.to_str().unwrap();
        let mut child = Command::new(gait_bin())
            .args(["__run-tool", "--gait-dir", cb, "--codebase", cb, "--session", "thread-scope", "--tool", "shell"])
            .arg("--output-spool")
            .arg(spool.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn gait (set GAIT_BIN to a fresh build)");
        child.stdin.take().unwrap().write_all(&impulse.encode_to_vec()).unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "hand failed: {}", String::from_utf8_lossy(&out.stderr));
        let response = HandResponse::decode(out.stdout.as_slice()).unwrap();
        let _: Feedback = match response.outcome {
            Some(hand_response::Outcome::Feedback(f)) => f,
            other => panic!("expected Feedback, got {other:?}"),
        };
        std::fs::read_to_string(spool.path()).unwrap()
    }

    /// `cat` of a file outside the readable scope is denied, and the hand had
    /// no thread besides the one that sealed (so no pre-seal thread can spawn
    /// an unconfined shell). The control run grants the file and reads it.
    #[test]
    #[ignore = "needs a Landlock kernel and a fresh gait binary (GAIT_BIN)"]
    fn thread_scope() {
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("denied.txt");
        std::fs::write(&target, "THREAD-SCOPE-MARKER").unwrap();
        let code = tempfile::tempdir().unwrap();
        // Threads of the hand whose tid lies between the hand's pid and this
        // shell's pid were created before the shell was forked, i.e. before
        // the seal (the shell is the hand's first child after sealing).
        let cmd = format!(
            "cat {}; n=0; for t in $(ls /proc/$PPID/task); do [ $t -gt $PPID ] && [ $t -lt $$ ] && n=$((n+1)); done; echo preseal=$n",
            target.display()
        );

        let control = run_hand(code.path(), shell_impulse(&[code.path(), outside.path()], &cmd));
        assert!(control.contains("THREAD-SCOPE-MARKER"), "control must read the file: {control}");

        let sealed = run_hand(code.path(), shell_impulse(&[code.path()], &cmd));
        assert!(!sealed.contains("THREAD-SCOPE-MARKER"), "denied file was read: {sealed}");
        assert!(sealed.contains("preseal=0"), "the hand must seal before any other thread exists: {sealed}");
    }
}

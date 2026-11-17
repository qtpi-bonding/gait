// Real-kernel Landlock enforcement tests for the hand (Linux).
//
// Each test spawns `gait __run-tool` so the seal lands on the hand process,
// never on the test runner, and compares a sealed run with a control run that
// differs only in the one thing under test (a wider scope, no config, no env
// variable, no seal), so a pass cannot come from a missing file or a refused
// connection. All tests are `#[ignore]`: they need a Landlock kernel and a
// `gait` built from this tree. Run on a Landlock kernel with a fresh binary:
//   GAIT_BIN=<target>/debug/gait cargo test --test landlock_seal -- --ignored --test-threads=1
// A stale binary gives false red and false green, so rebuild first.

#[cfg(target_os = "linux")]
mod tests {
    use gait_core::proto::gait::sandbox::v1::{
        hand_response, network_scope::Mode, Feedback, HandResponse, Impulse, NetworkScope,
        OutputCaptureMode, OutputPresentation, Scope,
    };
    use prost::Message as _;
    use std::io::Write as _;
    use std::path::Path;
    use std::process::{Command, Stdio};

    fn gait_bin() -> std::path::PathBuf {
        std::env::var("GAIT_BIN")
            .map(Into::into)
            .unwrap_or_else(|_| "gait".into())
    }

    /// Spawn the real hand with its own gait dir (where `gait.jsonc` is read)
    /// and extra environment variables; return the Feedback and the output.
    fn run_hand(
        codebase: &Path,
        gait_dir: &Path,
        impulse: Impulse,
        envs: &[(&str, &Path)],
    ) -> (Feedback, String) {
        let exe = gait_bin();
        let spool = tempfile::NamedTempFile::new_in(codebase).unwrap();
        let mut command = Command::new(&exe);
        command
            .args(["__run-tool", "--gait-dir"])
            .arg(gait_dir)
            .arg("--codebase")
            .arg(codebase)
            .args(["--session", "landlock-enforce-test", "--tool", "shell", "--output-spool"])
            .arg(spool.path());
        for (key, value) in envs {
            command.env(key, value);
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|e| panic!("failed to spawn gait ({exe:?}): {e}\nSet GAIT_BIN to a fresh build"));
        child.stdin.take().unwrap().write_all(&impulse.encode_to_vec()).unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "gait __run-tool ({exe:?}) exited {}: stderr={}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
        let response = HandResponse::decode(out.stdout.as_slice()).expect("valid hand response");
        let feedback = match response.outcome {
            Some(hand_response::Outcome::Feedback(f)) => f,
            other => panic!("expected hand Feedback, got {other:?}"),
        };
        let output = std::fs::read_to_string(spool.path()).unwrap();
        (feedback, output)
    }

    /// An Impulse for `shell` with the given scope.
    fn shell_impulse(
        cmd: &str,
        read_all: bool,
        readable: &[&Path],
        writable: &[&Path],
        network: Mode,
    ) -> Impulse {
        let paths = |ps: &[&Path]| ps.iter().map(|p| p.to_string_lossy().into_owned()).collect();
        Impulse {
            tool: "shell".into(),
            args: Some(
                serde_json::from_value(serde_json::json!({ "cmd": cmd, "reason": "landlock enforcement test" }))
                    .expect("valid shell args"),
            ),
            requested: Some(Scope {
                read_all,
                readable: paths(readable),
                writable: paths(writable),
                network: Some(NetworkScope { mode: network as i32, hosts: vec![] }),
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

    fn denial_shown(output: &str, fb: &Feedback) -> bool {
        output.contains("Permission denied")
            || output.contains("Operation not permitted")
            || fb.denied.is_some()
    }

    // The confined shell still loads ld.so and libc (Linux read defaults),
    // runs `echo`, and writes to /dev/null.
    #[test]
    #[ignore = "needs a Landlock kernel and a fresh gait binary (GAIT_BIN)"]
    fn confined_exec_succeeds() {
        let code = tempfile::tempdir().unwrap();
        let impulse = shell_impulse(
            "echo hi-confined; echo x > /dev/null && echo devnull-ok",
            false,
            &[code.path()],
            &[code.path()],
            Mode::Deny,
        );
        let (_fb, out) = run_hand(code.path(), code.path(), impulse, &[]);
        assert!(out.contains("hi-confined"), "echo must run under the seal — got: {out}");
        assert!(out.contains("devnull-ok"), "/dev/null must be writable — got: {out}");
    }

    // A file outside the readable scope is denied; the control run grants the
    // same file and must read it.
    #[test]
    #[ignore = "needs a Landlock kernel and a fresh gait binary (GAIT_BIN)"]
    fn confined_out_of_scope_read_denied() {
        let code = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let marker = "0ut-0f-sc0pe-marker";
        let file = outside.path().join("secret.txt");
        std::fs::write(&file, marker).unwrap();
        let cat = format!("cat {}", file.display());

        let wide = shell_impulse(&cat, false, &[code.path(), outside.path()], &[code.path()], Mode::Deny);
        let (_fb, control) = run_hand(code.path(), code.path(), wide, &[]);
        assert!(control.contains(marker), "control: a granted file must be readable — got: {control}");

        let narrow = shell_impulse(&cat, false, &[code.path()], &[code.path()], Mode::Deny);
        let (fb, denied) = run_hand(code.path(), code.path(), narrow, &[]);
        assert!(!denied.contains(marker), "out-of-scope read must not return the content — got: {denied}");
        assert!(denial_shown(&denied, &fb), "the denial must show up — got: {denied}, denied={:?}", fb.denied);
    }

    // A write outside the writable scope is denied while one inside works; the
    // control run makes the outside directory writable.
    #[test]
    #[ignore = "needs a Landlock kernel and a fresh gait binary (GAIT_BIN)"]
    fn confined_write_outside_writable_denied() {
        let code = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        // A gait dir inside the codebase would put the default deny paths
        // under it and take create rights from the codebase (documented), so
        // the hand gets its own gait dir, as in real use.
        let gait_dir = tempfile::tempdir().unwrap();
        let target = outside.path().join("written.txt");
        let cmd = format!("echo inside > {}/in.txt; echo outside > {}", code.path().display(), target.display());

        let wide = shell_impulse(&cmd, true, &[], &[code.path(), outside.path()], Mode::Deny);
        let (_fb, _) = run_hand(code.path(), gait_dir.path(), wide, &[]);
        assert!(target.exists(), "control: a writable directory must accept the write");
        std::fs::remove_file(&target).unwrap();
        std::fs::remove_file(code.path().join("in.txt")).unwrap();

        let narrow = shell_impulse(&cmd, true, &[], &[code.path()], Mode::Deny);
        let (fb, out) = run_hand(code.path(), gait_dir.path(), narrow, &[]);
        assert!(code.path().join("in.txt").exists(), "the in-scope write must succeed — got: {out}");
        assert!(!target.exists(), "the out-of-scope write must not create the file");
        assert!(denial_shown(&out, &fb), "the denial must show up — got: {out}, denied={:?}", fb.denied);
    }

    // The machine identity dir is denied under a blanket-read scope, where only
    // the deny rule stands between the shell and the folder. The variable is
    // read by the identity-keyfile library the hand uses; the control run
    // leaves it unset and must read the same file.
    #[test]
    #[ignore = "needs a Landlock kernel and a fresh gait binary (GAIT_BIN)"]
    fn machine_dir_denied_under_real_seal() {
        let home = std::env::var("HOME").expect("HOME must be set");
        let machine = tempfile::Builder::new()
            .prefix(".gait-machine-dir-test-")
            .tempdir_in(&home)
            .expect("scratch machine dir under HOME");
        let marker = "m4ch1ne-dir-marker";
        let file = machine.path().join("scratch.txt");
        std::fs::write(&file, marker).unwrap();
        let code = tempfile::tempdir().unwrap();
        let cat = format!("cat {}", file.display());
        let impulse = || shell_impulse(&cat, true, &[], &[code.path()], Mode::Deny);

        let (_fb, control) = run_hand(code.path(), code.path(), impulse(), &[]);
        assert!(control.contains(marker), "control: without MACHINE_IDENTITY_DIR the file must be readable — got: {control}");

        let (fb, denied) = run_hand(code.path(), code.path(), impulse(), &[("MACHINE_IDENTITY_DIR", machine.path())]);
        assert!(!denied.contains(marker), "the machine dir must not be readable through the shell — got: {denied}");
        assert!(denial_shown(&denied, &fb), "the denial must show up — got: {denied}, denied={:?}", fb.denied);
    }

    // A folder listed under `sandbox.deny.read` in gait.jsonc is denied by the
    // kernel. The probe is `shell cat`, never the `read` tool: the app-level
    // gate would deny the read tool first and the test would pass without a seal.
    #[test]
    #[ignore = "needs a Landlock kernel and a fresh gait binary (GAIT_BIN)"]
    fn configured_deny_path_denied_under_real_seal() {
        let home = std::env::var("HOME").expect("HOME must be set");
        let keys = tempfile::Builder::new()
            .prefix(".gait-keys-dir-test-")
            .tempdir_in(&home)
            .expect("scratch keys dir under HOME");
        let marker = "k3ys-dir-marker";
        let file = keys.path().join("scratch.txt");
        std::fs::write(&file, marker).unwrap();

        let plain = tempfile::tempdir().unwrap();
        let configured = tempfile::tempdir().unwrap();
        std::fs::write(
            configured.path().join("gait.jsonc"),
            serde_json::json!({ "sandbox": { "deny": { "read": [keys.path()] } } }).to_string(),
        )
        .unwrap();
        let code = tempfile::tempdir().unwrap();
        let cat = format!("cat {}", file.display());
        let run = |gait_dir: &Path| {
            let impulse = shell_impulse(&cat, true, &[], &[code.path()], Mode::Deny);
            run_hand(code.path(), gait_dir, impulse, &[])
        };

        let (_fb, control) = run(plain.path());
        assert!(control.contains(marker), "control: without the config the file must be readable — got: {control}");

        let (fb, denied) = run(configured.path());
        assert!(!denied.contains(marker), "a configured deny path must not be readable through the shell — got: {denied}");
        assert!(denial_shown(&denied, &fb), "the denial must show up — got: {denied}, denied={:?}", fb.denied);
    }

    // With the network denied, a TCP connect to a live local listener fails;
    // the control run (network All) connects to the same listener, so a
    // refused connection cannot pass for denied. bash's /dev/tcp is a builtin,
    // so no helper binary is involved. Needs Landlock ABI 4 (kernel 6.7+).
    #[test]
    #[ignore = "needs a Landlock kernel and a fresh gait binary (GAIT_BIN)"]
    fn network_deny_blocks_tcp_connect() {
        // SAFETY: a pure query; null attr and zero size with the VERSION flag.
        let abi = unsafe {
            libc::syscall(libc::SYS_landlock_create_ruleset, std::ptr::null::<libc::c_void>(), 0usize, 1u32)
        };
        if abi < 4 {
            eprintln!("SKIP network_deny_blocks_tcp_connect: Landlock ABI {abi} < 4 (TCP rules need kernel 6.7+)");
            return;
        }
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let code = tempfile::tempdir().unwrap();
        let cmd = format!("bash -c 'exec 3<>/dev/tcp/127.0.0.1/{port} && echo CONNECTED-OK'");
        let impulse = |mode| shell_impulse(&cmd, false, &[code.path()], &[code.path()], mode);

        let (_fb, control) = run_hand(code.path(), code.path(), impulse(Mode::All), &[]);
        assert!(control.contains("CONNECTED-OK"), "control: network All must connect to the live listener — got: {control}");

        let (fb, denied) = run_hand(code.path(), code.path(), impulse(Mode::Deny), &[]);
        assert!(!denied.contains("CONNECTED-OK"), "network Deny must block the connect — got: {denied}");
        assert!(denial_shown(&denied, &fb), "the failure must be a denial, not a refusal — got: {denied}");
        drop(listener);
    }

    // Landlock confines only the thread that seals and what it spawns later.
    // The hand runs on a current-thread runtime and seals first, so no thread
    // can predate the seal: the shell counts the hand's threads older than
    // itself and must see none, and a denied file stays denied.
    #[test]
    #[ignore = "needs a Landlock kernel and a fresh gait binary (GAIT_BIN)"]
    fn hand_has_no_thread_older_than_the_seal() {
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
        let wide = shell_impulse(&cmd, false, &[code.path(), outside.path()], &[code.path()], Mode::Deny);
        let (_fb, control) = run_hand(code.path(), code.path(), wide, &[]);
        assert!(control.contains("THREAD-SCOPE-MARKER"), "control must read the file: {control}");

        let narrow = shell_impulse(&cmd, false, &[code.path()], &[code.path()], Mode::Deny);
        let (_fb, sealed) = run_hand(code.path(), code.path(), narrow, &[]);
        assert!(!sealed.contains("THREAD-SCOPE-MARKER"), "denied file was read: {sealed}");
        assert!(sealed.contains("preseal=0"), "the hand must seal before any other thread exists: {sealed}");
    }

    // Why the order matters, demonstrated on the kernel itself: a thread that
    // exists before the seal stays unconfined and can still read a denied
    // file, while a process spawned from the sealing thread cannot. The hand
    // cannot be handed a pre-seal thread from outside, so this runs the real
    // `LandlockSandbox` in a re-executed copy of this test binary. If the
    // kernel or the applier ever confines existing threads too, the escape
    // assertion flips and the hand's ordering rule can be revisited.
    #[test]
    #[ignore = "needs a Landlock kernel; seals a child copy of the test binary"]
    fn pre_seal_thread_escapes_but_sealing_thread_does_not() {
        use gait_core::sandbox::{platform_sandbox, NetworkPolicy, SandboxScope, SandboxStatus};
        if let Ok(denied_file) = std::env::var("GAIT_SEAL_CHILD_FILE") {
            let code = std::env::var("GAIT_SEAL_CHILD_CODE").unwrap();
            let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
            let (res_tx, res_rx) = std::sync::mpsc::channel::<bool>();
            let (up_tx, up_rx) = std::sync::mpsc::channel::<()>();
            let f = denied_file.clone();
            let old = std::thread::spawn(move || {
                up_tx.send(()).unwrap();
                go_rx.recv().unwrap();
                let ok = Command::new("cat").arg(&f).output().map(|o| o.status.success()).unwrap_or(false);
                res_tx.send(ok).unwrap();
            });
            up_rx.recv().unwrap(); // the thread exists before the seal
            let scope = SandboxScope {
                readable: vec![code.into()],
                writable: vec![],
                read_all: false,
                network: NetworkPolicy::All,
                ..Default::default()
            };
            let status = platform_sandbox().apply(&scope).unwrap();
            assert!(
                matches!(status, SandboxStatus::Active | SandboxStatus::ActiveWithNotes { .. }),
                "{status:?}"
            );
            let sealing = Command::new("cat").arg(&denied_file).output().map(|o| o.status.success()).unwrap_or(false);
            go_tx.send(()).unwrap();
            let pre_seal = res_rx.recv().unwrap();
            old.join().unwrap();
            println!("RESULT sealing_thread_read={sealing} pre_seal_thread_read={pre_seal}");
            return;
        }

        let outside = tempfile::tempdir().unwrap();
        let file = outside.path().join("denied.txt");
        std::fs::write(&file, "x").unwrap();
        let code = tempfile::tempdir().unwrap();
        let out = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tests::pre_seal_thread_escapes_but_sealing_thread_does_not",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("GAIT_SEAL_CHILD_FILE", &file)
            .env("GAIT_SEAL_CHILD_CODE", code.path())
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(out.status.success(), "child failed: {stdout} {}", String::from_utf8_lossy(&out.stderr));
        assert!(
            stdout.contains("RESULT sealing_thread_read=false pre_seal_thread_read=true"),
            "sealing thread must be denied and a pre-seal thread unconfined — got: {stdout}"
        );
    }
}

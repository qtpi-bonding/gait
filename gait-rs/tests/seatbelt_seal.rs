// Tests Seatbelt enforcement — must run as a single isolated test binary.
// Each test function seals the process and cannot be undone; use `--test-threads=1`
// and mark as `#[ignore]` to prevent accidental sealing of the test suite process.
// Run manually: cargo test -p gait-core --test seatbelt_seal -- --ignored --test-threads=1

#[cfg(target_os = "macos")]
mod tests {
    use gait_core::sandbox::{platform_sandbox, NetworkPolicy, SandboxScope, SandboxStatus};
    #[test]
    #[ignore = "seals the process — run with: cargo test --test seatbelt_seal -- --ignored --test-threads=1"]
    fn seatbelt_apply_returns_active_and_enforces_writes() {
        let tmp = tempfile::tempdir().unwrap();
        let allowed = tmp.path().join("allowed");
        std::fs::create_dir_all(&allowed).unwrap();

        let scope = SandboxScope {
            writable: vec![allowed.clone()],
            read_all: true,
            network: NetworkPolicy::Deny,
            ..Default::default()
        };

        // Apply the seal
        let status = platform_sandbox().apply(&scope).unwrap();
        assert!(
            matches!(status, SandboxStatus::Active),
            "Expected Active, got: {status:?}"
        );

        // In-scope write succeeds
        std::fs::write(allowed.join("ok.txt"), b"hello").expect("in-scope write must succeed");

        // Out-of-scope write denied
        let probe = std::env::temp_dir().join(format!("gait-seal-probe-{}", std::process::id()));
        let result = std::fs::write(&probe, b"should fail");
        assert!(
            result.is_err(),
            "out-of-scope write must be denied by Seatbelt"
        );
    }
}

// ── Confined-read enforcement tests ──────────────────────────────────────────
//
// These tests spawn `gait __run-tool` as a subprocess so the Seatbelt seal is
// applied to the hand process (not the test runner). The hand receives an Impulse
// with `read_all=false` and a confined `readable` list. We assert that:
//   (a) exec still works — dyld can load libc etc. under the platform read-set
//   (b) in-scope reads succeed
//   (c) out-of-scope reads are denied (EACCES/EPERM from the kernel)
//   (d) DNS resolves — resolver files are in the platform read-set
//
// The tests are `#[ignore]` so CI skips them; they require the `gait` binary on
// PATH (installed via `make install`) and macOS Seatbelt.
//
// Wire encoding: binary protobuf via prost (same as SubprocessSynapse in production).

#[cfg(target_os = "macos")]
mod confined_read_tests {
    use gait_core::proto::gait::sandbox::v1::{
        network_scope::Mode, Feedback, Impulse, NetworkScope, Scope,
    };
    use pbjson_types::value::Kind;
    use prost::Message as _;
    use std::io::Write as _;
    use std::process::{Command, Stdio};

    /// Resolve the path to the `gait` binary for hand-spawning tests.
    ///
    /// Resolution order:
    /// 1. `GAIT_BIN` env var — explicit override (useful in CI / custom installs)
    /// 2. `<CARGO_MANIFEST_DIR>/../target/debug/gait` and the custom target-dir
    ///    `/home/user/develop/cargo-target/debug/gait` — built by `cargo build -p gait-bin`
    /// 3. `gait` on PATH — last resort (the `make install` path)
    ///
    /// The binary MUST understand `__run-tool`; an old installed binary (from a
    /// release build before Phase 2a) will exit non-zero with "unknown command".
    fn gait_bin() -> std::path::PathBuf {
        if let Ok(p) = std::env::var("GAIT_BIN") {
            return std::path::PathBuf::from(p);
        }
        // Try workspace-relative target/debug first.
        let manifest = env!("CARGO_MANIFEST_DIR");
        let candidates = [
            // cargo target-dir override (detected from ~/.cargo/config.toml)
            std::path::PathBuf::from("/home/user/develop/cargo-target/debug/gait"),
            // standard target dir (no config override)
            std::path::PathBuf::from(manifest).join("target/debug/gait"),
        ];
        for c in &candidates {
            if c.exists() {
                return c.clone();
            }
        }
        // Fall back to PATH
        std::path::PathBuf::from("gait")
    }

    /// Spawn `gait __run-tool` with the given Impulse, return decoded Feedback.
    /// Panics if the process exits non-zero or stdout is not a valid Feedback.
    fn run_hand(codebase: &str, tool: &str, impulse: Impulse) -> Feedback {
        let exe = gait_bin();
        let impulse_bytes = impulse.encode_to_vec();

        let mut child = Command::new(&exe)
            .args([
                "__run-tool",
                "--gait-dir",
                codebase,
                "--codebase",
                codebase,
                "--session",
                "seatbelt-enforce-test",
                "--tool",
                tool,
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|e| panic!("failed to spawn gait ({exe:?}): {e}\nSet GAIT_BIN or run `cargo build -p gait-bin`"));

        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(&impulse_bytes).unwrap();
        drop(stdin);

        let out = child.wait_with_output().unwrap();
        if !out.status.success() {
            panic!(
                "gait __run-tool ({exe:?}) exited {}: stderr={}",
                out.status,
                String::from_utf8_lossy(&out.stderr)
            );
        }
        Feedback::decode(out.stdout.as_slice()).expect("valid Feedback from hand")
    }

    /// Build an Impulse for `shell` with confined reads (read_all=false).
    /// `readable` contains the codebase path. `network_mode` selects network policy.
    fn shell_impulse(codebase: &str, cmd: &str, network_mode: Mode) -> Impulse {
        Impulse {
            tool: "shell".into(),
            args: Some(
                serde_json::from_value(
                    serde_json::json!({ "cmd": cmd, "reason": "confined-read enforcement test" }),
                )
                .expect("valid shell args struct"),
            ),
            requested: Some(Scope {
                read_all: false,
                readable: vec![codebase.into()],
                writable: vec![codebase.into()],
                network: Some(NetworkScope {
                    mode: network_mode as i32,
                    hosts: vec![],
                }),
                local_bind: false,
                read_deny: None,
                write_deny: None,
            }),
            prefetch: None,
            shell_whitelist_patterns: vec![],
        }
    }

    /// Extract the string value from a Feedback result, panicking if absent.
    fn feedback_string(fb: &Feedback) -> String {
        let val = fb.result.as_ref().expect("Feedback.result must be set");
        match &val.kind {
            Some(Kind::StringValue(s)) => s.clone(),
            Some(Kind::StructValue(s)) => serde_json::to_value(s)
                .map(|v| v.to_string())
                .unwrap_or_default(),
            other => panic!("unexpected result kind: {other:?}"),
        }
    }

    /// Use the gait-rs source tree itself as a stable codebase dir for these tests.
    fn codebase() -> String {
        // manifest_dir is gait-rs/ (the gait-core package root)
        env!("CARGO_MANIFEST_DIR").to_string()
    }

    // ── (a) exec works ────────────────────────────────────────────────────────
    //
    // A `shell echo hi` hand process is sealed with read_all=false. dyld must
    // be able to load /usr/lib, /usr/share/dyld, and the shared cache under
    // /private/var/db/dyld — all listed in platform_read_defaults().
    // If exec breaks, the hand exits non-zero and the test panics in run_hand().
    #[test]
    #[cfg(target_os = "macos")]
    #[ignore = "requires gait binary and macOS Seatbelt"]
    fn confined_exec_succeeds() {
        let cb = codebase();
        let impulse = shell_impulse(&cb, "echo hi", Mode::Deny);
        let fb = run_hand(&cb, "shell", impulse);
        let result = feedback_string(&fb);
        // The shell tool returns a JSON object; the output field should contain "hi"
        assert!(
            result.contains("hi"),
            "exec under confined reads must succeed — got: {result}"
        );
    }

    // ── (b) in-scope read works ───────────────────────────────────────────────
    //
    // A `read` tool call for a file that IS in `readable` must succeed and return
    // the file content. We read Cargo.toml from the gait-rs package root.
    #[test]
    #[cfg(target_os = "macos")]
    #[ignore = "requires gait binary and macOS Seatbelt"]
    fn confined_in_scope_read_succeeds() {
        let cb = codebase();
        let impulse = Impulse {
            tool: "read".into(),
            args: Some(
                serde_json::from_value(serde_json::json!({ "file_path": "Cargo.toml" }))
                    .expect("valid read args"),
            ),
            requested: Some(Scope {
                read_all: false,
                readable: vec![cb.clone()],
                writable: vec![],
                network: Some(NetworkScope {
                    mode: Mode::Deny as i32,
                    hosts: vec![],
                }),
                local_bind: false,
                read_deny: None,
                write_deny: None,
            }),
            prefetch: None,
            shell_whitelist_patterns: vec![],
        };
        let fb = run_hand(&cb, "read", impulse);
        let result = feedback_string(&fb);
        assert!(
            result.contains("gait-core"),
            "in-scope read must return file content — got: {result}"
        );
    }

    // ── (c) out-of-scope read denied ─────────────────────────────────────────
    //
    // Write a secret file to a home-directory path BEFORE spawning the hand.
    // The hand's Impulse has `readable=[codebase]` only. The HOME dir is NOT in
    // platform_read_defaults(), so the kernel must deny the read (EACCES/EPERM).
    //
    // We deliberately avoid /tmp because platform_read_defaults() includes /private
    // (full subtree), which resolves to /private/tmp via the /tmp symlink — so
    // /tmp paths are actually readable in confined-read mode. The home directory
    // (/Users/<user>) is truly out-of-scope.
    #[test]
    #[cfg(target_os = "macos")]
    #[ignore = "requires gait binary and macOS Seatbelt"]
    fn confined_out_of_scope_read_denied() {
        // Write the secret BEFORE the hand is sealed.
        // Use the home dir — it's outside all platform_read_defaults() subtrees.
        let home = std::env::var("HOME").unwrap_or_else(|_| "/home/user".to_string());
        let secret_path = format!("{home}/.gait-secret-test-{}", std::process::id());
        std::fs::write(&secret_path, b"s3cr3t").expect("pre-test secret write must succeed");

        let cb = codebase();
        // Use `shell cat` so we also exercise the denied-detection in run_tool.rs
        let impulse = shell_impulse(&cb, &format!("cat {secret_path}"), Mode::Deny);
        let fb = run_hand(&cb, "shell", impulse);

        // Clean up AFTER the hand exits (the hand can't write here but we can)
        let _ = std::fs::remove_file(&secret_path);

        let result = feedback_string(&fb);
        assert!(
            !result.contains("s3cr3t"),
            "out-of-scope read must NOT return the secret content — got: {result}"
        );
        // Seatbelt denies the read; the shell output should contain "Permission denied"
        // or "Operation not permitted"; the fb.denied field may also be set.
        let denied_in_output = result.contains("Permission denied")
            || result.contains("Operation not permitted")
            || result.contains("error");
        assert!(
            denied_in_output || fb.denied.is_some(),
            "out-of-scope read must signal denial — got: {result}, denied={:?}",
            fb.denied
        );
    }

    // ── (d) DNS still works ───────────────────────────────────────────────────
    //
    // Under confined-read + network-all scope, DNS resolution must work. The
    // platform_read_defaults() list includes /etc, /private/etc, /private/var/db/timezone,
    // and the mDNSResponder mach-lookup stanza is emitted for network-allow scopes.
    // We use `nslookup example.com` (always available on macOS) as the DNS probe.
    #[test]
    #[cfg(target_os = "macos")]
    #[ignore = "requires gait binary and macOS Seatbelt"]
    fn confined_dns_still_works() {
        let cb = codebase();
        // nslookup will exit 0 and print the resolved IP when DNS works
        let impulse = shell_impulse(&cb, "nslookup example.com", Mode::All);
        let fb = run_hand(&cb, "shell", impulse);
        let result = feedback_string(&fb);
        assert!(
            result.contains("example.com") || result.contains("93.184"),
            "DNS resolution must succeed under confined reads + network-all — got: {result}"
        );
        assert!(
            !result.contains("SERVFAIL") && !result.contains("connection timed out"),
            "DNS must not fail — got: {result}"
        );
    }
}

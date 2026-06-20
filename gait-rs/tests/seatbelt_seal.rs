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

//! Shared arg/path helpers + `run` entrypoint.
//! Pulled out of gait-bin so the bin can become a thin dispatcher.

pub fn extract_path_arg(args: &[String], flag: &str) -> Option<std::path::PathBuf> {
    args.windows(2)
        .find(|w| w[0] == flag)
        .map(|w| std::path::PathBuf::from(&w[1]))
}

/// Resolve gait dir: `--gait-dir`/`$GAIT_DIR` > walk up for `.gait/` or `.gaitref`
/// > error. See `gait_core::gait_dir`.
pub fn resolve_gait_dir(args: &[String]) -> anyhow::Result<std::path::PathBuf> {
    let explicit = extract_path_arg(args, "--gait-dir");
    resolve_gait_dir_from(explicit)
}

/// Resolve a parsed gait-dir option through the shared state policy.
pub fn resolve_gait_dir_from(
    explicit: Option<std::path::PathBuf>,
) -> anyhow::Result<std::path::PathBuf> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    gait_core::gait_dir::resolve(explicit, &cwd)
}

/// Resolve codebase: --codebase arg > current working directory.
///
/// Canonicalized here, once, so every downstream consumer (tool
/// implementations' own `Path::exists()`/read/write calls via
/// `ToolCtx.codebase_path`, AND the sandbox scope computed separately in
/// `compute_scope_with_home`) sees the same resolved path. Before this fix,
/// only the sandbox-scope side canonicalized its own copy (to handle
/// symlinks like macOS's `/tmp` -> `/private/tmp`); tools kept using this
/// raw, un-resolved value, so a symlinked `--codebase` made the kernel-level
/// sandbox grant (canonical path) and the tools' own filesystem checks (raw
/// path) diverge — a `glob`/`ls` on the raw path could get silently denied
/// at a symlink component the grant didn't cover, surfacing as a false
/// "Directory not found" for a directory that genuinely exists. Falls back
/// to the raw path if canonicalize fails (e.g. genuinely doesn't exist yet),
/// same pattern used elsewhere (`gait_dir_inside_codebase` in embed.rs,
/// `compute_scope_with_home` in gait-sandbox).
pub fn resolve_codebase(args: &[String]) -> std::path::PathBuf {
    let raw = extract_path_arg(args, "--codebase").unwrap_or_else(|| {
        std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
    });
    raw.canonicalize().unwrap_or(raw)
}

// cmd_brain deleted (Task 5): dispatch now via gait_core::__dispatch_role.
// `gait run` routes through cmd_run -> gait_turn_runner::{run_single_prompt,
// run_turn_source} directly (Task 3) -- no `gait-workflow`/`run_workflow`
// involved for an ordinary turn.

pub async fn cmd_run(argv: &[String]) {
    let gait_dir = resolve_gait_dir(argv).unwrap_or_else(|e| {
        eprintln!("[gait] {e}");
        std::process::exit(1);
    });
    let codebase = resolve_codebase(argv);
    crate::cli::run::cmd_run(argv, gait_dir, codebase).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_path_arg_finds_value() {
        let args = vec![
            "run".to_string(),
            "--gait-dir".to_string(),
            "/foo/bar".to_string(),
        ];
        assert_eq!(
            extract_path_arg(&args, "--gait-dir"),
            Some(std::path::PathBuf::from("/foo/bar"))
        );
    }

    #[test]
    fn extract_path_arg_returns_none_when_missing() {
        let args = vec!["run".to_string(), "--task".to_string(), "hi".to_string()];
        assert_eq!(extract_path_arg(&args, "--gait-dir"), None);
    }
    #[test]
    fn extract_path_arg_returns_none_for_empty() {
        assert_eq!(extract_path_arg(&[], "--gait-dir"), None);
    }

    mod manifest_tests {
        use super::*;
        use serial_test::serial;

        #[test]
        #[serial]
        fn resolve_gait_dir_uses_env_var_when_no_arg() {
            std::env::set_var("GAIT_DIR", "/tmp/test-gait");
            let result = resolve_gait_dir(&[]);
            std::env::remove_var("GAIT_DIR");
            assert_eq!(result.unwrap(), std::path::PathBuf::from("/tmp/test-gait"));
        }

        #[test]
        #[serial]
        fn resolve_gait_dir_arg_overrides_env_var() {
            std::env::set_var("GAIT_DIR", "/tmp/env-gait");
            let args = vec!["--gait-dir".to_string(), "/tmp/arg-gait".to_string()];
            let result = resolve_gait_dir(&args);
            std::env::remove_var("GAIT_DIR");
            assert_eq!(result.unwrap(), std::path::PathBuf::from("/tmp/arg-gait"));
        }

        #[test]
        fn resolve_codebase_defaults_to_pwd() {
            let result = resolve_codebase(&[]);
            let cwd = std::env::current_dir().unwrap();
            assert_eq!(result, cwd.canonicalize().unwrap_or(cwd));
        }

        #[test]
        fn resolve_codebase_uses_arg() {
            // A non-existent path: canonicalize() fails, falls back to raw —
            // exercises the fallback branch specifically.
            let args = vec!["--codebase".to_string(), "/tmp/mycode-does-not-exist".to_string()];
            let result = resolve_codebase(&args);
            assert_eq!(result, std::path::PathBuf::from("/tmp/mycode-does-not-exist"));
        }

        // Regression test for the archeglyph-eb report (2026-09-08): a
        // symlinked --codebase (macOS's /tmp -> /private/tmp is the common
        // real-world trigger) must resolve to the SAME canonical path tools
        // and the sandbox scope both end up using, not the raw symlinked one.
        #[cfg(unix)]
        #[test]
        fn resolve_codebase_resolves_a_symlinked_path_to_its_real_target() {
            let t = tempfile::TempDir::new().unwrap();
            let real_dir = t.path().join("real-codebase");
            std::fs::create_dir_all(&real_dir).unwrap();
            let link = t.path().join("linked-codebase");
            std::os::unix::fs::symlink(&real_dir, &link).unwrap();

            let args = vec!["--codebase".to_string(), link.to_string_lossy().to_string()];
            let result = resolve_codebase(&args);
            assert_eq!(
                result,
                real_dir.canonicalize().unwrap(),
                "must resolve through the symlink to the real directory, not stay at the symlink path"
            );
        }
    }
}

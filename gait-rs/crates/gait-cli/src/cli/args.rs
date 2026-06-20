//! Shared arg/path helpers + `run` entrypoint.
//! Pulled out of gait-bin so the bin can become a thin dispatcher.

pub fn extract_path_arg(args: &[String], flag: &str) -> Option<std::path::PathBuf> {
    args.windows(2)
        .find(|w| w[0] == flag)
        .map(|w| std::path::PathBuf::from(&w[1]))
}

/// The single source of truth for gait-dir precedence: `--gait-dir` arg >
/// `$GAIT_DIR` env var > `None`. Callers pick the no-value policy (error vs.
/// fall back to pwd) — see `resolve_gait_dir` and `cmd_chat`.
pub fn gait_dir_or_env(args: &[String]) -> Option<std::path::PathBuf> {
    if let Some(p) = extract_path_arg(args, "--gait-dir") {
        return Some(p);
    }
    std::env::var("GAIT_DIR")
        .ok()
        .filter(|v| !v.is_empty())
        .map(std::path::PathBuf::from)
}

/// Resolve gait dir: --gait-dir arg > $GAIT_DIR env var > error.
pub fn resolve_gait_dir(args: &[String]) -> anyhow::Result<std::path::PathBuf> {
    gait_dir_or_env(args)
        .ok_or_else(|| anyhow::anyhow!("no gait dir: pass --gait-dir <path> or set $GAIT_DIR"))
}

/// Resolve codebase: --codebase arg > current working directory.
pub fn resolve_codebase(args: &[String]) -> std::path::PathBuf {
    extract_path_arg(args, "--codebase").unwrap_or_else(|| {
        std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
    })
}

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
        #[serial]
        fn resolve_gait_dir_errors_when_nothing_set() {
            std::env::remove_var("GAIT_DIR");
            let result = resolve_gait_dir(&[]);
            assert!(
                result.is_err(),
                "should error when no --gait-dir and no $GAIT_DIR"
            );
        }

        #[test]
        fn resolve_codebase_defaults_to_pwd() {
            let result = resolve_codebase(&[]);
            assert_eq!(result, std::env::current_dir().unwrap());
        }

        #[test]
        fn resolve_codebase_uses_arg() {
            let args = vec!["--codebase".to_string(), "/tmp/mycode".to_string()];
            let result = resolve_codebase(&args);
            assert_eq!(result, std::path::PathBuf::from("/tmp/mycode"));
        }
    }
}

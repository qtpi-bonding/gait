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
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    gait_core::gait_dir::resolve(explicit, &cwd)
}

/// Resolve codebase: --codebase arg > current working directory.
pub fn resolve_codebase(args: &[String]) -> std::path::PathBuf {
    extract_path_arg(args, "--codebase").unwrap_or_else(|| {
        std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
    })
}

// cmd_brain deleted (Task 5): dispatch now via gait_core::__dispatch_role.
// `gait run` routes through cmd_run → single_prompt_run → run_workflow (spec-06 §3).

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

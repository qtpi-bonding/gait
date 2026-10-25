//! `gait link <gait-dir>` — write a `.gaitref` pointer in cwd naming an existing
//! gait workspace, so `gait_core::gait_dir::resolve` finds it by walk-up.
#![allow(clippy::print_stdout, clippy::print_stderr)]

use crate::cli::error::{CliError, CliResult};
use std::path::{Path, PathBuf};

pub fn pointer_body(gait_dir: &Path) -> String {
    format!("gaitdir: {}\n", gait_dir.display())
}

/// Point the current directory at an existing gait workspace.
///
/// Writes a `.gaitref` pointer file, so `gait_core::gait_dir::resolve` finds
/// the workspace by walk-up without needing `--gait-dir` on every invocation.
// Mirrors `InitArgs`'s derive exactly (`#[derive(Parser)]` +
// `#[command(no_binary_name = true)]`, per init.rs:6-11) — a plain comment
// so it doesn't leak into clap's `--help`/subcommand-about text the way a
// `///` doc comment does.
#[derive(clap::Parser, Debug)]
#[command(no_binary_name = true)]
pub struct LinkArgs {
    /// Path to an existing gait workspace to point at.
    pub gait_dir: PathBuf,
    #[arg(long)]
    pub pretty: bool,
}

pub fn cmd_link(args: LinkArgs) -> CliResult {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let target = if args.gait_dir.is_absolute() {
        args.gait_dir.clone()
    } else {
        cwd.join(&args.gait_dir)
    };
    match gait_ops::workspace::link(&cwd, &target) {
        Ok(out) => {
            println!(
                "{}",
                gait_ops::render_json(
                    &serde_json::json!({
                        "gaitref": out.gaitref.display().to_string(),
                        "target": out.target.display().to_string(),
                    }),
                    args.pretty
                )
            );
            Ok(())
        }
        Err(e) if matches!(gait_ops::OpError::find(&e), Some(gait_ops::OpError::NotFound(_))) => {
            Err(CliError::not_found(format!("[gait] {e}")))
        }
        Err(e) => Err(CliError::failed(format!("[gait] {e}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// The typed variant a command failed with. Panics on an uncoded error.
    fn cli_error(err: &anyhow::Error) -> &CliError {
        coded_error::downcast_ref::<CliError>(err).expect("error is a CliError")
    }

    /// Run `cmd_link` with the process cwd set to `cwd`, then restore it. The
    /// result is returned un-asserted so the cwd is restored even on failure.
    fn link_from(cwd: &Path, target: &Path) -> CliResult {
        let saved = std::env::current_dir().unwrap();
        std::env::set_current_dir(cwd).unwrap();
        let result = cmd_link(LinkArgs {
            gait_dir: target.to_path_buf(),
            pretty: false,
        });
        std::env::set_current_dir(saved).unwrap();
        result
    }

    #[test]
    fn pointer_body_prefixed_absolute() {
        assert_eq!(
            pointer_body(&PathBuf::from("/opt/home")),
            "gaitdir: /opt/home\n"
        );
    }

    #[test]
    #[serial_test::serial]
    fn round_trips_through_core_resolver() {
        // `resolve(None, ...)` honors $GAIT_DIR before walk-up (by design), so a
        // dev shell with $GAIT_DIR exported would otherwise short-circuit this
        // test before it ever reaches the .gaitref walk-up being verified here.
        let saved = std::env::var("GAIT_DIR").ok();
        std::env::remove_var("GAIT_DIR");
        let t = tempfile::TempDir::new().unwrap();
        let repo = t.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let gait_dir = t.path().join("home");
        std::fs::write(repo.join(".gaitref"), pointer_body(&gait_dir)).unwrap();
        let result = gait_core::gait_dir::resolve(None, &repo);
        if let Some(v) = saved {
            std::env::set_var("GAIT_DIR", v);
        }
        assert_eq!(result.unwrap(), gait_dir);
    }

    #[test]
    #[serial_test::serial]
    fn link_to_a_missing_path_is_not_found() {
        let t = tempfile::TempDir::new().unwrap();
        let cwd = t.path().join("cwd");
        std::fs::create_dir_all(&cwd).unwrap();
        let err = link_from(&cwd, &t.path().join("nowhere")).unwrap_err();
        assert!(matches!(cli_error(&err), CliError::NotFound(_)));
        assert!(format!("{err:#}").contains("is not a gait workspace"));
    }

    #[test]
    #[serial_test::serial]
    fn link_to_a_directory_without_git_is_not_found() {
        let t = tempfile::TempDir::new().unwrap();
        let cwd = t.path().join("cwd");
        let plain = t.path().join("plain");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(&plain).unwrap();
        let err = link_from(&cwd, &plain).unwrap_err();
        assert!(matches!(cli_error(&err), CliError::NotFound(_)));
    }

    #[test]
    #[serial_test::serial]
    fn link_when_the_pointer_cannot_be_written_is_failed() {
        let t = tempfile::TempDir::new().unwrap();
        let cwd = t.path().join("cwd");
        let ws = t.path().join("ws");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(ws.join(".git")).unwrap();
        // A directory where the `.gaitref` file should go makes the write fail.
        std::fs::create_dir_all(cwd.join(".gaitref")).unwrap();
        let err = link_from(&cwd, &ws).unwrap_err();
        assert!(matches!(cli_error(&err), CliError::Failed(_)));
    }
}

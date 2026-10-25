//! `gait daemons` subcommand: list and stop tracked background daemons.

use crate::cli::error::{CliError, CliResult};
use clap::Parser;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(no_binary_name = true)]
pub struct DaemonsArgs {
    #[arg(long, env = "GAIT_DIR")]
    pub gait_dir: Option<PathBuf>,
    /// Subcommand: list (default) | stop
    pub action: Option<String>,
    /// Daemon name (for `stop`)
    pub name: Option<String>,
    #[arg(long)]
    pub pretty: bool,
    /// `list` only: also show daemons that have finished (done/killed),
    /// not just ones currently running.
    #[arg(long)]
    pub all: bool,
}

pub fn cmd_daemons(args: DaemonsArgs) -> CliResult {
    let gait_dir = crate::cli::args::resolve_gait_dir_from(args.gait_dir)
        .ok()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    match args.action.as_deref() {
        Some("stop") => {
            let Some(name) = args.name.as_deref() else {
                return Err(CliError::usage(
                    "[gait daemons] usage: gait daemons stop <name>",
                ));
            };
            cmd_daemons_stop(&gait_dir, name, args.pretty)
        }
        Some("list") | None => cmd_daemons_list(&gait_dir, args.all, args.pretty),
        Some(other) => Err(CliError::usage(format!(
            "[gait daemons] unknown subcommand: {}",
            other
        ))),
    }
}

fn cmd_daemons_list(gait_dir: &std::path::Path, all: bool, pretty: bool) -> CliResult {
    match gait_ops::daemon::list(gait_dir, all) {
        Ok(outcome) => {
            println!("{}", outcome.to_json(pretty));
            Ok(())
        }
        Err(e) => Err(CliError::failed(format!("[gait daemons] error: {}", e))),
    }
}

fn cmd_daemons_stop(gait_dir: &std::path::Path, name: &str, pretty: bool) -> CliResult {
    match gait_ops::daemon::stop(gait_dir, name) {
        Ok(outcome) => {
            println!("{}", outcome.to_json(pretty));
            Ok(())
        }
        Err(e) if matches!(gait_ops::OpError::find(&e), Some(gait_ops::OpError::NotFound(_))) => {
            Err(CliError::not_found(format!("[gait daemons] error: {}", e)))
        }
        Err(e) => Err(CliError::failed(format!("[gait daemons] error: {}", e))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The typed variant a command failed with. Panics on an uncoded error.
    fn cli_error(err: &anyhow::Error) -> &CliError {
        coded_error::downcast_ref::<CliError>(err).expect("error is a CliError")
    }

    fn args(action: Option<&str>, name: Option<&str>, gait_dir: &std::path::Path) -> DaemonsArgs {
        DaemonsArgs {
            gait_dir: Some(gait_dir.to_path_buf()),
            action: action.map(str::to_string),
            name: name.map(str::to_string),
            pretty: false,
            all: false,
        }
    }

    #[test]
    fn stop_without_a_name_is_usage() {
        let dir = tempfile::TempDir::new().unwrap();
        let err = cmd_daemons(args(Some("stop"), None, dir.path())).unwrap_err();
        assert!(matches!(cli_error(&err), CliError::Usage(_)));
        assert_eq!(
            format!("{err:#}"),
            "[gait daemons] usage: gait daemons stop <name>"
        );
    }

    #[test]
    fn unknown_subcommand_is_usage() {
        let dir = tempfile::TempDir::new().unwrap();
        let err = cmd_daemons(args(Some("restart"), None, dir.path())).unwrap_err();
        assert!(matches!(cli_error(&err), CliError::Usage(_)));
        assert_eq!(
            format!("{err:#}"),
            "[gait daemons] unknown subcommand: restart"
        );
    }

    #[test]
    fn stop_of_an_unknown_daemon_is_not_found() {
        let dir = tempfile::TempDir::new().unwrap();
        let err = cmd_daemons(args(Some("stop"), Some("ghost"), dir.path())).unwrap_err();
        assert!(matches!(cli_error(&err), CliError::NotFound(_)));
        assert!(format!("{err:#}").contains("no daemon named 'ghost'"));
    }
}

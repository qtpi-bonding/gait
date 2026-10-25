//! `gait init` — adapter: build `gait_ops::workspace::InitParams`, call the op,
//! render `InitOutcome` and error. The logic lives in `gait_ops::workspace`.
#![allow(clippy::print_stdout, clippy::print_stderr)]

use crate::cli::error::{CliError, CliResult};
use clap::Parser;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(no_binary_name = true)]
pub struct InitArgs {
    /// Create the workspace even if it would live inside a codebase git repo.
    /// (In-tree is discouraged: running such a workspace also needs
    /// `--allow-intree`, since the agent could otherwise write to its own config.
    /// Prefer out-of-tree: `gait init ~/gait-home && gait link ~/gait-home`.)
    #[arg(long)]
    pub force: bool,
    /// Target gait-dir (default: current directory).
    pub path: Option<PathBuf>,
    #[arg(long)]
    pub pretty: bool,
}

/// The scaffolded files table `gait init` always creates — static
/// documentation data, not per-run state, but carried in the JSON result so
/// the new-workspace layout is discoverable without reading source.
fn scaffold_files() -> serde_json::Value {
    serde_json::json!([
        {"path": "gait.jsonc", "purpose": "model + tool-permission config", "git": "committed"},
        {"path": "agents/default/AGENT.md", "purpose": "the agent's role and instructions", "git": "committed"},
        {"path": "USER.md", "purpose": "your personal instructions (not shared)", "git": "ignored"},
        {"path": "skills/", "purpose": "installed skills (skills/<n>/SKILL.md)", "git": "committed (empty dir; tracked once you add skills)"},
        {"path": "sessions/", "purpose": "turn history", "git": "ignored"},
    ])
}

pub fn cmd_init(args: InitArgs) -> CliResult {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let raw = args.path.clone().unwrap_or_else(|| cwd.clone());
    let gait_dir = if raw.is_absolute() {
        raw
    } else {
        cwd.join(raw)
    };

    let params = gait_ops::workspace::InitParams {
        path: gait_dir,
        force: args.force,
    };
    match gait_ops::workspace::init(params) {
        Ok(outcome) => {
            if outcome.in_tree {
                eprintln!(
                    "[gait] warning: this workspace is inside your code tree; running it will \
                     require --allow-intree, and an agent could then write to its own config."
                );
            }
            println!(
                "{}",
                gait_ops::render_json(
                    &serde_json::json!({
                        "gait_dir": outcome.gait_dir.display().to_string(),
                        "pre_existing_workspace": outcome.pre_existing_workspace,
                        "in_tree": outcome.in_tree,
                        "gitignored": outcome.gitignored,
                        "gitignore_line": outcome.gitignore_line,
                        "gitignore_path": outcome.gitignore_path.map(|p| p.display().to_string()),
                        "files": scaffold_files(),
                    }),
                    args.pretty
                )
            );
            Ok(())
        }
        Err(e) => Err(match gait_ops::OpError::find(&e) {
            // The refusal names the path the user passed; that input is what is wrong.
            Some(gait_ops::OpError::Conflict(msg)) => {
                CliError::invalid_input(format!("[gait] {msg}"))
            }
            _ => CliError::failed_after("[gait]", e),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The typed variant a command failed with. Panics on an uncoded error.
    fn cli_error(err: &anyhow::Error) -> &CliError {
        coded_error::downcast_ref::<CliError>(err).expect("error is a CliError")
    }

    fn init_args(path: PathBuf) -> InitArgs {
        InitArgs {
            force: false,
            path: Some(path),
            pretty: false,
        }
    }

    #[test]
    fn init_inside_a_git_repo_without_force_is_invalid_input() {
        let repo = tempfile::TempDir::new().unwrap();
        git2::Repository::init(repo.path()).unwrap();
        let err = cmd_init(init_args(repo.path().join("gait-home"))).unwrap_err();
        assert!(matches!(cli_error(&err), CliError::InvalidInput(_)));
        let text = format!("{err:#}");
        assert!(text.starts_with("[gait] "), "{text}");
        assert!(text.contains("would live inside git repo"), "{text}");
    }

    #[test]
    fn init_at_a_path_that_is_a_file_is_failed() {
        let dir = tempfile::TempDir::new().unwrap();
        let file = dir.path().join("not-a-dir");
        std::fs::write(&file, b"occupied").unwrap();
        let err = cmd_init(init_args(file)).unwrap_err();
        assert!(matches!(cli_error(&err), CliError::Failed(_)));
        assert!(format!("{err:#}").starts_with("[gait] "));
    }

    #[test]
    fn init_below_a_regular_file_is_failed() {
        let dir = tempfile::TempDir::new().unwrap();
        let file = dir.path().join("occupied");
        std::fs::write(&file, b"occupied").unwrap();
        let err = cmd_init(init_args(file.join("child"))).unwrap_err();
        assert!(matches!(cli_error(&err), CliError::Failed(_)));
    }
}

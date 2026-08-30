//! `gait init` — adapter: build `gait_ops::workspace::InitParams`, call the op,
//! render `InitOutcome` and error. The logic lives in `gait_ops::workspace`.
#![allow(clippy::print_stdout, clippy::print_stderr, clippy::exit)]

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

pub fn cmd_init(args: InitArgs) {
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
        }
        Err(gait_ops::OpError::Conflict(msg)) => {
            eprintln!("[gait] {msg}");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("[gait] {e}");
            std::process::exit(1);
        }
    }
}

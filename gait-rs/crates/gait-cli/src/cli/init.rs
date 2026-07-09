//! `gait init` — adapter: build `gait_ops::workspace::InitParams`, call the op,
//! render `InitOutcome` and error. The logic lives in `gait_ops::workspace`.
#![allow(clippy::print_stdout, clippy::print_stderr, clippy::exit)]

use clap::Parser;
use std::path::{Path, PathBuf};

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
}

fn print_init_summary(gait_dir: &Path) {
    let display = gait_dir.display();
    eprintln!("[gait] initialized gait-dir at {display}");
    eprintln!();
    eprintln!("  {:<32} {:<46} git", "file", "purpose");
    eprintln!(
        "  {:<32} {:<46} committed",
        "gait.jsonc", "model + tool-permission config"
    );
    eprintln!(
        "  {:<32} {:<46} committed",
        "agents/default/AGENT.md", "the agent's role and instructions"
    );
    eprintln!(
        "  {:<32} {:<46} ignored",
        "USER.md", "your personal instructions (not shared)"
    );
    eprintln!(
        "  {:<32} {:<46} committed*",
        "skills/", "installed skills  (skills/<n>/SKILL.md)"
    );
    eprintln!("  {:<32} {:<46} ignored", "sessions/", "turn history");
    eprintln!();
    eprintln!("  * empty dir; git-tracked when you add skills");
    eprintln!();
    eprintln!("  next: gait run --task \"...\"");
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
            if outcome.gitignored {
                println!(
                    "[gait] added {} to {}",
                    outcome.gitignore_line.as_deref().unwrap_or(""),
                    outcome
                        .gitignore_path
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default()
                );
            }
            if outcome.in_tree {
                eprintln!(
                    "[gait] warning: this workspace is inside your code tree; running it will \
                     require --allow-intree, and an agent could then write to its own config."
                );
            }
            print_init_summary(&outcome.gait_dir);
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

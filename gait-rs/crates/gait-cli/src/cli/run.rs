//! `RunArgs` — shared arg parser for `gait run` / `gait __brain` dispatch.
//!
//! `cmd_run` and `CliDriver` have been deleted (Task 2): the in-process run path
//! is dead code; `gait run` routes through `cmd_spine` → `SpineDriver` → `run_loop`
//! → `relay_brain_turn` (sealed brain). `RunArgs` is kept because `cmd_spine` and
//! `cmd_brain` both parse it.
use clap::Parser;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(no_binary_name = true)]
pub struct RunArgs {
    #[arg(long = "session")]
    pub session_id: Option<String>,
    #[arg(long)]
    pub task: Option<String>,
    #[arg(long)]
    pub auto: bool,
    #[arg(long = "model")]
    pub model_override: Option<String>,
    #[arg(long = "agent")]
    pub agent_name: Option<String>,
    #[arg(long = "result-schema")]
    pub result_schema_path: Option<String>,
    #[arg(long, action = clap::ArgAction::Append)]
    pub allow: Vec<String>,
    #[arg(long, action = clap::ArgAction::Append)]
    pub deny: Vec<String>,
    #[arg(long)]
    pub max_iter: Option<usize>,
    // Task 7: sandbox engagement flags. `--sandbox` forces on, `--no-sandbox` forces off.
    // When neither is passed, the config default (`config.sandbox.on`) applies.
    // `--sandbox-locked` is set by a parent spine when spawning a child; it forces
    // sandbox on and makes `--no-sandbox` a no-op so the child cannot escape.
    #[arg(long)]
    pub sandbox: bool,
    #[arg(long = "no-sandbox")]
    pub no_sandbox: bool,
    #[arg(long = "sandbox-locked", hide = true)]
    pub sandbox_locked: bool,
    /// Cascade group handle passed by a parent spine to a blocking subagent.
    /// `None` for the top-level turn; a parent's pgid string for subagents (Plan 4).
    #[arg(long = "cascade-group", hide = true)]
    pub cascade_group: Option<String>,
    // Consumed by caller (args.rs); included to prevent "unexpected argument" errors.
    #[arg(long, hide = true)]
    gait_dir: Option<PathBuf>,
    #[arg(long, hide = true)]
    codebase: Option<PathBuf>,
    // Accepted to prevent "unexpected argument" errors. Container spawning is
    // handled externally — gait never self-spawns into a container.
    #[arg(long = "container", hide = true)]
    container: bool,
}

// ── Deleted: cmd_run + CliDriver (Task 2) ─────────────────────────────────────
// These were the in-process run path. Deleted as dead code — `args::cmd_run` has
// no caller in main.rs. `gait run` routes through `cmd_spine` → `SpineDriver` →
// `run_loop` → `relay_brain_turn` (sealed brain). The sealed path is the only path.

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parse_extracts_task_and_flags() {
        let argv: Vec<String> = [
            "--task",
            "hi",
            "--auto",
            "--model",
            "x/y",
            "--max-iter",
            "7",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let a = RunArgs::parse_from(&argv);
        assert_eq!(a.task.as_deref(), Some("hi"));
        assert!(a.auto);
        assert_eq!(a.model_override.as_deref(), Some("x/y"));
        assert_eq!(a.max_iter, Some(7));
    }
    #[test]
    fn parse_no_task_is_none() {
        assert!(RunArgs::parse_from(&["--auto".to_string()]).task.is_none());
    }
    #[test]
    fn sandbox_locked_parsed() {
        let a = RunArgs::parse_from(&["--sandbox-locked".to_string()]);
        assert!(a.sandbox_locked);
        assert!(!a.no_sandbox);
    }
}

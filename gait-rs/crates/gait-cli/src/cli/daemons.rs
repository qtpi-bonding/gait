//! `gait daemons` subcommand: list and stop tracked background daemons.

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
    pub json: bool,
}

pub fn cmd_daemons(args: DaemonsArgs) {
    let gait_dir = args
        .gait_dir
        .or_else(|| {
            std::env::var("GAIT_DIR")
                .ok()
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        })
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    match args.action.as_deref() {
        Some("stop") => {
            let name = match &args.name {
                Some(n) => n.clone(),
                None => {
                    eprintln!("[gait daemons] usage: gait daemons stop <name>");
                    return;
                }
            };
            cmd_daemons_stop(&gait_dir, &name);
        }
        Some("list") | None => cmd_daemons_list(&gait_dir),
        Some(other) => eprintln!("[gait daemons] unknown subcommand: {}", other),
    }
}

fn cmd_daemons_list(gait_dir: &std::path::Path) {
    match gait_ops::daemon::list(gait_dir) {
        Ok(outcome) => {
            if outcome.rows.is_empty() {
                println!("No daemons.");
                return;
            }
            for row in &outcome.rows {
                println!(
                    "{} (pid {}) — {} — log: {}",
                    row.name, row.pid, row.status, row.log_path
                );
            }
        }
        Err(e) => eprintln!("[gait daemons] error: {}", e),
    }
}

fn cmd_daemons_stop(gait_dir: &std::path::Path, name: &str) {
    match gait_ops::daemon::stop(gait_dir, name) {
        Ok(outcome) => {
            if outcome.was_alive {
                println!(
                    "[gait daemons] stopped {} (pid {})",
                    outcome.id, outcome.pid
                );
            } else {
                println!("[gait daemons] {} is not running", outcome.id);
            }
        }
        Err(e) => eprintln!("[gait daemons] error: {}", e),
    }
}

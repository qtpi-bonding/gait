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
    use gait_core::session::daemons::{active, is_alive};
    match active(gait_dir) {
        Ok(recs) => {
            if recs.is_empty() {
                println!("No daemons.");
                return;
            }
            for r in &recs {
                let status = if is_alive(r) { "running" } else { "stopped" };
                println!(
                    "{} (pid {}) — {} — log: {}",
                    r.name, r.pid, status, r.log_path
                );
            }
        }
        Err(e) => eprintln!("[gait daemons] error: {}", e),
    }
}

fn cmd_daemons_stop(gait_dir: &std::path::Path, name: &str) {
    use gait_core::session::daemons::{active, append_stop, is_alive};
    let recs = match active(gait_dir) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[gait daemons] error: {}", e);
            return;
        }
    };
    match recs.iter().find(|r| r.name == name) {
        None => eprintln!("[gait daemons] no daemon named '{}'", name),
        Some(rec) => {
            if is_alive(rec) {
                // SAFETY: is_alive performed the identity check (proc_start_time comparison).
                // killpg sends SIGTERM to the daemon's process group.
                unsafe { libc::killpg(rec.pgid, libc::SIGTERM) };
                println!("[gait daemons] stopped {} (pid {})", name, rec.pid);
            } else {
                println!("[gait daemons] {} is not running", name);
            }
            let _ = append_stop(gait_dir, name);
        }
    }
}

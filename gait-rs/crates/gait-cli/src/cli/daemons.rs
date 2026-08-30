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
    pub pretty: bool,
    /// `list` only: also show daemons that have finished (done/killed),
    /// not just ones currently running.
    #[arg(long)]
    pub all: bool,
}

pub fn cmd_daemons(args: DaemonsArgs) {
    let gait_dir = crate::cli::args::resolve_gait_dir_from(args.gait_dir)
        .ok()
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
            cmd_daemons_stop(&gait_dir, &name, args.pretty);
        }
        Some("list") | None => cmd_daemons_list(&gait_dir, args.all, args.pretty),
        Some(other) => eprintln!("[gait daemons] unknown subcommand: {}", other),
    }
}

fn cmd_daemons_list(gait_dir: &std::path::Path, all: bool, pretty: bool) {
    match gait_ops::daemon::list(gait_dir, all) {
        Ok(outcome) => println!("{}", outcome.to_json(pretty)),
        Err(e) => eprintln!("[gait daemons] error: {}", e),
    }
}

fn cmd_daemons_stop(gait_dir: &std::path::Path, name: &str, pretty: bool) {
    match gait_ops::daemon::stop(gait_dir, name) {
        Ok(outcome) => println!("{}", outcome.to_json(pretty)),
        Err(e) => eprintln!("[gait daemons] error: {}", e),
    }
}

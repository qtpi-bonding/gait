//! Hidden `__daemon-exit` subcommand handler.
//!
//! Invoked by the shell wrapper (Task 6) when a detached daemon command exits.
//! Records the daemon's stop in the registry (truthful status), then calls
//! `notify_parent` to surface the completion to the owning session — waking
//! it if idle, or letting the live turn poll surface it (Part 2).
//!
//! Argv: `gait __daemon-exit --gait-dir <dir> --session <parent> --name <name> --exit <code>`

use std::path::PathBuf;

use crate::session::completion::{notify_parent, Completion};
use crate::session::daemons;

/// Args parsed from `argv[1..]` (with `no_binary_name`) when `argv[0]` is `__daemon-exit`.
#[derive(clap::Parser, Debug)]
pub struct DaemonExitArgs {
    #[arg(long)]
    pub gait_dir: PathBuf,

    #[arg(long)]
    pub session: String,

    #[arg(long)]
    pub name: String,

    #[arg(long)]
    pub exit: i32,
}

/// Record the daemon's stop and surface the completion to the parent session.
///
/// Always records the stop (the registry is the durable fallback for Part 2's
/// live-turn surfacing). The notify call is best-effort: missing parent,
/// unexpected I/O, etc. all degrade silently — the caller (the wrapper) has
/// already finished the user command and the only effect of failure is that
/// the parent will not be woken now (it will pick the completion up on the
/// next turn via `<status>`).
pub fn cmd_daemon_exit(args: DaemonExitArgs) -> anyhow::Result<()> {
    // 1. Record the stop always (truthful registry status).
    let _ = daemons::append_stop(&args.gait_dir, &args.name, args.exit);

    // 2. Build the completion + notify the parent.
    let status = if args.exit == 124 || args.exit < 0 {
        "killed"
    } else {
        "done"
    };
    let log_path = args
        .gait_dir
        .join("daemons")
        .join(format!("{}.log", args.name));
    let c = Completion::Daemon {
        name: args.name.clone(),
        exit_code: args.exit,
        status: status.to_string(),
        log_path: log_path.to_string_lossy().into_owned(),
    };
    let repos = crate::git::Repos::production();
    let _ = notify_parent(&args.gait_dir, &repos, &args.session, &c);

    Ok(())
}

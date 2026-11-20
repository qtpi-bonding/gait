//! Daemon ops: the background processes a session started. Thin over
//! `gait_ops::daemon`.
use super::{infra, GaitContext};
use op_registry::{CallOptions, OpParams, Ref};

output! {
    /// A tracked background daemon.
    pub struct DaemonItem {
        pub name: String,
        pub pid: i32,
        /// `running`, `done` or `killed`.
        pub status: String,
        /// Where its output is logged.
        pub log_path: String,
    }
}

output! {
    /// What `daemons stop` did.
    pub struct DaemonStopped {
        /// The daemon name.
        pub id: String,
        pub pid: i32,
        /// Whether it was still running when asked to stop.
        pub was_alive: bool,
        /// Whether it was sent SIGTERM; true only when it was running.
        pub signalled: bool,
    }
}

#[derive(Debug, Clone, OpParams)]
pub struct DaemonListParams {
    /// Widen the set: for `daemons list`, also finished daemons; for `persist` and `discard`, every idle session that is not running (needs `--yes`).
    pub all: bool,
}

#[derive(Debug, Clone, OpParams)]
pub struct DaemonStopParams {
    /// The object to act on: a session id, or the name of an inference, mode, extension, provider, config key or daemon.
    #[op(subject = "daemon")]
    pub id: Ref,
}

/// List the running background daemons, or all of them with `--all`.
pub async fn list(
    ctx: &GaitContext,
    params: DaemonListParams,
    _opts: CallOptions,
) -> anyhow::Result<Vec<DaemonItem>> {
    let out = gait_ops::daemon::list(&ctx.gait_dir()?, params.all).map_err(infra)?;
    Ok(out
        .rows
        .into_iter()
        .map(|row| DaemonItem { name: row.name, pid: row.pid, status: row.status, log_path: row.log_path })
        .collect())
}

/// Stop a running daemon by name and record the stop.
pub async fn stop(
    ctx: &GaitContext,
    params: DaemonStopParams,
    _opts: CallOptions,
) -> anyhow::Result<DaemonStopped> {
    let out = gait_ops::daemon::stop(&ctx.gait_dir()?, params.id.as_raw()).map_err(infra)?;
    Ok(DaemonStopped { id: out.id, pid: out.pid, was_alive: out.was_alive, signalled: out.signalled })
}

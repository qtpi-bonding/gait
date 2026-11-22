//! Process ops: the background processes a session started. Thin over
//! `gait_ops::process`.
use super::{infra, GaitContext};
use op_registry::{CallOptions, OpParams, Ref};

output! {
    /// A tracked background process.
    pub struct TrackedProcessItem {
        pub name: String,
        pub pid: i32,
        /// `running`, `done` or `killed`.
        pub status: String,
        /// Where its output is logged.
        pub log_path: String,
    }
}

output! {
    /// What `processes stop` did.
    pub struct ProcessStopped {
        /// The process name.
        pub id: String,
        pub pid: i32,
        /// Whether it was still running when asked to stop.
        pub was_alive: bool,
        /// Whether it was sent SIGTERM; true only when it was running.
        pub signalled: bool,
    }
}

#[derive(Debug, Clone, OpParams)]
pub struct ProcessListParams {
    /// Widen the set: for `processes list`, also finished processes; for `persist` and `discard`, every idle session that is not running (needs `--yes`).
    pub all: bool,
}

#[derive(Debug, Clone, OpParams)]
pub struct ProcessStopParams {
    /// The object to act on: a session id, or the name of an inference, mode, extension, provider, config key or process.
    #[op(subject = "process")]
    pub id: Ref,
}

/// List the running background processes, or all of them with `--all`.
pub async fn list(
    ctx: &GaitContext,
    params: ProcessListParams,
    _opts: CallOptions,
) -> anyhow::Result<Vec<TrackedProcessItem>> {
    let out = gait_ops::processes::list(&ctx.gait_dir()?, params.all).map_err(infra)?;
    Ok(out
        .rows
        .into_iter()
        .map(|row| TrackedProcessItem {
            name: row.name,
            pid: row.pid,
            status: row.status,
            log_path: row.log_path,
        })
        .collect())
}

/// Stop a running process by name and record the stop.
pub async fn stop(
    ctx: &GaitContext,
    params: ProcessStopParams,
    _opts: CallOptions,
) -> anyhow::Result<ProcessStopped> {
    let out = gait_ops::processes::stop(&ctx.gait_dir()?, params.id.as_raw()).map_err(infra)?;
    Ok(ProcessStopped {
        id: out.id,
        pid: out.pid,
        was_alive: out.was_alive,
        signalled: out.signalled,
    })
}

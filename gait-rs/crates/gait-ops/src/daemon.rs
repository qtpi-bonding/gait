//! Daemon-registry verbs: `list`, `stop`.
//!
//! Sources: `crates/gait-cli/src/cli/daemons.rs`. The registry is an append-only
//! log of daemon lifecycle events (starts and stops) stored in
//! `<gait_dir>/daemons.jsonl`. `list` wraps `active` + `is_alive` to report
//! live daemons; `stop` resolves the target, signals it (if alive), and records
//! the stop via `append_stop`.

use crate::OpError;
use std::io;
use std::path::Path;

//-----------------------------------------------------------------------------
// list
// -----------------------------------------------------------------------------

/// One daemon entry in the list output.
#[derive(Debug, Clone)]
pub struct DaemonRow {
    pub name: String,
    pub pid: i32,
    pub status: String, // "running" or "stopped"
    pub log_path: String,
}

/// Outcome of `list`: active daemons and their status.
#[derive(Debug)]
pub struct DaemonListOutcome {
    pub rows: Vec<DaemonRow>,
}

/// Query the daemon registry and return all active daemons with their liveness
/// status. Read-only; no mutations.
pub fn list(gait_dir: &Path) -> Result<DaemonListOutcome, OpError> {
    use gait_state::session::daemons::{active, is_alive};

    let recs = active(gait_dir).map_err(|e| OpError::Other(anyhow::Error::from(e)))?;
    let rows = recs
        .iter()
        .map(|rec| {
            let status = if is_alive(rec) {
                "running".to_string()
            } else {
                "stopped".to_string()
            };
            DaemonRow {
                name: rec.name.clone(),
                pid: rec.pid,
                status,
                log_path: rec.log_path.clone(),
            }
        })
        .collect();

    Ok(DaemonListOutcome { rows })
}

// -----------------------------------------------------------------------------
// stop
// -----------------------------------------------------------------------------

/// Outcome of `stop`: what happened when stopping a daemon.
#[derive(Debug)]
pub struct StopOutcome {
    /// The daemon name.
    pub id: String,
    /// The process ID of the daemon.
    pub pid: i32,
    /// Whether the daemon was reported alive before we signalled it.
    pub was_alive: bool,
    /// Whether we sent a SIGTERM to the process group (true only if was_alive).
    pub signalled: bool,
}

/// Send SIGTERM to a process group, wrapping the unsafe `libc::killpg` call
/// behind a seam so it's injectable/testable.
fn signal_pgid(pgid: i32) -> io::Result<()> {
    // SAFETY: killpg is safe to call in this context (we've already validated
    // the pgid belongs to a live daemon via is_alive).
    let result = unsafe { libc::killpg(pgid, libc::SIGTERM) };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Stop a running daemon by name. Resolves the daemon from the registry,
/// signals it if alive, and records the stop in the append-only log.
///
/// If the daemon is not found in the registry, returns `OpError::NotFound`.
/// If the daemon is already dead, skips the signal and records the stop cleanly.
pub fn stop(gait_dir: &Path, name: &str) -> Result<StopOutcome, OpError> {
    use gait_state::session::daemons::{active, append_stop, is_alive};

    let recs = active(gait_dir).map_err(|e| OpError::Other(anyhow::Error::from(e)))?;
    let rec = recs
        .iter()
        .find(|r| r.name == name)
        .ok_or_else(|| OpError::NotFound(format!("no daemon named '{}'", name)))?;

    let was_alive = is_alive(rec);
    let pid = rec.pid;

    if was_alive {
        // SAFETY: is_alive performed the identity check (proc_start_time comparison).
        signal_pgid(rec.pgid).map_err(|e| OpError::Other(anyhow::Error::from(e)))?;
    }

    // Record the stop in the append-only log. CLI stop is SIGTERM (graceful).
    // We don't observe the exit code; record 0 to mean "cleanly stopped until
    // the daemon-exit hook writes the real one.
    append_stop(gait_dir, name, 0).map_err(|e| OpError::Other(anyhow::Error::from(e)))?;

    Ok(StopOutcome {
        id: name.to_string(),
        pid,
        was_alive,
        signalled: was_alive,
    })
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use gait_state::proto::gait::log::v1 as pb;
    use gait_state::session::daemons::append_start;

    fn make_rec(name: &str, pid: i32) -> pb::DaemonStart {
        pb::DaemonStart {
            name: name.to_owned(),
            pid,
            pgid: pid,
            started_at: 12345,
            cmd: "echo hello".to_owned(),
            cwd: "/tmp".to_owned(),
            log_path: format!("/tmp/{}.log", name),
            session: String::new(),
        }
    }

    #[test]
    fn list_returns_daemons_with_status() {
        let dir = tempfile::TempDir::new().unwrap();
        let rec1 = make_rec("daemon-a", 9999);
        let rec2 = make_rec("daemon-b", 8888);
        append_start(dir.path(), &rec1).unwrap();
        append_start(dir.path(), &rec2).unwrap();

        let outcome = list(dir.path()).unwrap();
        assert_eq!(outcome.rows.len(), 2);

        // Check that we have both daemons (order may vary).
        let names: Vec<&str> = outcome.rows.iter().map(|r| r.name.as_str()).collect();
        assert!(names.contains(&"daemon-a"));
        assert!(names.contains(&"daemon-b"));

        // Both should report status (either "running" or "stopped" — we can't
        // guarantee "running" since the pids are fake, but they should be populated).
        for row in &outcome.rows {
            assert!(row.status == "running" || row.status == "stopped");
            assert!(!row.log_path.is_empty());
        }
    }

    #[test]
    fn list_empty_when_no_daemons() {
        let dir = tempfile::TempDir::new().unwrap();
        let outcome = list(dir.path()).unwrap();
        assert!(outcome.rows.is_empty());
    }

    #[test]
    fn stop_on_dead_daemon_skips_signal_but_records() {
        let dir = tempfile::TempDir::new().unwrap();
        // Create a daemon record with a pid that's definitely not running.
        let rec = pb::DaemonStart {
            name: "dead-daemon".to_owned(),
            pid: 999999,
            pgid: 999999,
            started_at: u64::MAX, // Invalid start time ensures is_alive returns false
            cmd: "sleep 999".to_owned(),
            cwd: "/tmp".to_owned(),
            log_path: "/tmp/dead.log".to_owned(),
            session: String::new(),
        };
        append_start(dir.path(), &rec).unwrap();

        // Stop the daemon. Since is_alive returns false, no signal is sent,
        // but the stop is still recorded.
        let outcome = stop(dir.path(), "dead-daemon").unwrap();

        assert_eq!(outcome.id, "dead-daemon");
        assert!(!outcome.was_alive); // Should detect it's dead
        assert!(!outcome.signalled); // Should not have sent a signal

        // Verify the stop was recorded in the registry.
        let recs = gait_state::session::daemons::active(dir.path()).unwrap();
        assert!(
            recs.iter().all(|r| r.name != "dead-daemon"),
            "stopped daemon should not appear in active()"
        );
    }

    #[test]
    fn stop_not_found_returns_error() {
        let dir = tempfile::TempDir::new().unwrap();
        let rec = make_rec("existing-daemon", 7777);
        append_start(dir.path(), &rec).unwrap();

        let err = stop(dir.path(), "nonexistent").unwrap_err();
        assert!(matches!(err, OpError::NotFound(_)));
    }
}

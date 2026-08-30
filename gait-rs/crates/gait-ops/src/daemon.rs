//! Daemon-registry verbs: `list`, `stop`.
//!
//! Sources: `crates/gait-cli/src/cli/daemons.rs`. The registry is an append-only
//! log of daemon lifecycle events (starts and stops) stored in
//! `<gait_dir>/daemons.jsonl`. `list` wraps `history`/`active` + `is_alive` to
//! report daemons (running-only by default, everything under `--all`); `stop`
//! resolves the target, signals it (if alive), and records the stop via
//! `append_stop`.

use crate::OpError;
use std::io;
use std::path::Path;

//-----------------------------------------------------------------------------
// list
// -----------------------------------------------------------------------------

/// One daemon in the list output.
#[derive(Debug, Clone, serde::Serialize)]
pub struct DaemonRow {
    pub name: String,
    pub pid: i32,
    /// `"running"`, `"done"`, or `"killed"` — see
    /// `gait_state::session::daemons::classify_exit`.
    pub status: String,
    pub log_path: String,
}

/// Outcome of `list`.
#[derive(Debug, serde::Serialize)]
pub struct DaemonListOutcome {
    pub rows: Vec<DaemonRow>,
}

impl DaemonListOutcome {
    /// The CLI's only rendering — no plain-text form exists.
    pub fn to_json(&self, pretty: bool) -> String {
        crate::render_json(&self.rows, pretty)
    }
}

/// Query the daemon registry.
///
/// Default (`all = false`): only daemons that are actually running right
/// now. A dead daemon is never worth surfacing by default — "your daemon
/// finished/died" already has its own channel
/// (`turn::completion_poll::pending_daemon_completions` posts a one-time
/// event into the *owning* session's own log). Re-reporting the same dead
/// entry to every caller of `list` — including unrelated, later sessions
/// just asking "what's running" — is clutter, not signal.
///
/// `all = true`: every daemon that has ever been registered, each labeled
/// `"running"` / `"done"` / `"killed"` (see `classify_exit`) — for when you
/// actually want the history, e.g. debugging whether a daemon ran at all.
///
/// Self-healing (both modes): a `Start` record can be left active forever if
/// the daemon dies without going through either normal `Stop`-appending path
/// (`__daemon-exit` on clean exit, `gait daemons stop` on manual stop) —
/// SIGKILL, OOM, or the owning session/turn being torn down mid-flight all
/// leave no one to append the `Stop`. Rather than adding a reaper process
/// (gait is daemonless by design — no daemons watching daemons), `list`
/// appends the missing `Stop` itself the moment it observes the death (exit
/// code -9, classified `"killed"`). That keeps the registry an append-only
/// log of true observations: the entry is healed on the very call that
/// notices it, so it never needs re-discovering by a later session.
pub fn list(gait_dir: &Path, all: bool) -> Result<DaemonListOutcome, OpError> {
    use gait_state::session::daemons::{append_stop, classify_exit, history, is_alive};

    let recs = history(gait_dir).map_err(|e| OpError::Other(anyhow::Error::from(e)))?;
    let rows = recs
        .into_iter()
        .filter_map(|(rec, stop_exit_code)| {
            let status = match stop_exit_code {
                Some(code) => classify_exit(code).to_string(),
                None if is_alive(&rec) => "running".to_string(),
                None => {
                    // Observed dead with no recorded Stop: heal the registry
                    // now so this doesn't linger and clutter every future
                    // listing. Best-effort — a failure here just means we'll
                    // notice again next time, no worse than the status quo.
                    let _ = append_stop(gait_dir, &rec.name, -9);
                    classify_exit(-9).to_string()
                }
            };
            if !all && status != "running" {
                return None;
            }
            Some(DaemonRow {
                name: rec.name.clone(),
                pid: rec.pid,
                status,
                log_path: rec.log_path.clone(),
            })
        })
        .collect();

    Ok(DaemonListOutcome { rows })
}

// -----------------------------------------------------------------------------
// stop
// -----------------------------------------------------------------------------

/// Outcome of `stop`: what happened when stopping a daemon.
#[derive(Debug, serde::Serialize)]
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

impl StopOutcome {
    /// The CLI's only rendering — no plain-text form exists.
    pub fn to_json(&self, pretty: bool) -> String {
        crate::render_json(self, pretty)
    }
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

    /// `list` must report a genuinely-running daemon, and never surface a
    /// dead one — even on its very first observation. Uses a real spawned
    /// process for the "alive" side (fake pids are indistinguishable from
    /// dead ones, which is exactly the case this behavior depends on).
    #[test]
    fn list_shows_only_alive_daemons() {
        use std::os::unix::process::CommandExt;
        use std::process::Command;

        let dir = tempfile::TempDir::new().unwrap();

        let mut child = unsafe {
            Command::new("sleep")
                .arg("999")
                .pre_exec(|| {
                    libc::setsid();
                    Ok(())
                })
                .spawn()
                .expect("spawn sleep 999")
        };
        let pid = child.id() as i32;
        let started_at =
            gait_state::process::proc_start_time(pid).expect("proc_start_time for child");
        let alive = pb::DaemonStart {
            name: "alive-daemon".to_owned(),
            pid,
            pgid: pid,
            started_at,
            cmd: "sleep 999".to_owned(),
            cwd: "/tmp".to_owned(),
            log_path: "/tmp/alive-daemon.log".to_owned(),
            session: String::new(),
        };
        append_start(dir.path(), &alive).unwrap();

        // A dead entry with no recorded Stop (fake pid, bogus started_at).
        let dead = pb::DaemonStart {
            name: "dead-daemon".to_owned(),
            pid: 999999,
            pgid: 999999,
            started_at: u64::MAX,
            cmd: "sleep 999".to_owned(),
            cwd: "/tmp".to_owned(),
            log_path: "/tmp/dead-daemon.log".to_owned(),
            session: String::new(),
        };
        append_start(dir.path(), &dead).unwrap();

        let outcome = list(dir.path(), false).unwrap();
        assert_eq!(outcome.rows.len(), 1, "only the alive daemon is shown");
        assert_eq!(outcome.rows[0].name, "alive-daemon");
        assert_eq!(outcome.rows[0].status, "running");

        // `--all` surfaces both: the running one, and the dead one healed
        // into a `"killed"` entry (it has no recorded Stop, so it's the
        // unclean-death case, not a clean `"done"`).
        let all = list(dir.path(), true).unwrap();
        let mut names_statuses: Vec<(&str, &str)> = all
            .rows
            .iter()
            .map(|r| (r.name.as_str(), r.status.as_str()))
            .collect();
        names_statuses.sort();
        assert_eq!(
            names_statuses,
            vec![("alive-daemon", "running"), ("dead-daemon", "killed")]
        );

        let _ = child.kill();
        let _ = child.wait();
    }

    /// A daemon that went through the normal clean-exit path (`Stop` with
    /// exit code 0, as `__daemon-exit` records) must classify as `"done"`,
    /// not `"killed"` — and must not show up in the default (non-`--all`)
    /// view at all.
    #[test]
    fn list_all_classifies_clean_stop_as_done() {
        let dir = tempfile::TempDir::new().unwrap();
        let rec = make_rec("finished-daemon", 12345);
        append_start(dir.path(), &rec).unwrap();
        gait_state::session::daemons::append_stop(dir.path(), "finished-daemon", 0).unwrap();

        let default_view = list(dir.path(), false).unwrap();
        assert!(
            default_view.rows.is_empty(),
            "done daemons are hidden by default"
        );

        let all = list(dir.path(), true).unwrap();
        assert_eq!(all.rows.len(), 1);
        assert_eq!(all.rows[0].status, "done");
    }

    /// A `Start` record with no `Stop` whose pid is provably dead (fake pid,
    /// bogus started_at) must be self-healed: the `list()` call that
    /// observes it as dead appends the missing `Stop`, so a subsequent read
    /// of the registry (or another `list()` call, from an unrelated later
    /// session) no longer sees it at all. This is what keeps a shared
    /// gait-dir's daemon registry from accumulating zombie entries that
    /// clutter every future session's view.
    #[test]
    fn list_heals_dead_entry_so_it_disappears_next_time() {
        let dir = tempfile::TempDir::new().unwrap();
        let rec = pb::DaemonStart {
            name: "zombie".to_owned(),
            pid: 999999,
            pgid: 999999,
            started_at: u64::MAX, // guarantees is_alive() == false
            cmd: "sleep 999".to_owned(),
            cwd: "/tmp".to_owned(),
            log_path: "/tmp/zombie.log".to_owned(),
            session: "some-old-session".to_owned(),
        };
        append_start(dir.path(), &rec).unwrap();

        // First observation: never shown, even though it's the first call.
        let first = list(dir.path(), false).unwrap();
        assert!(first.rows.is_empty());

        // Healed: a fresh, unrelated read of the registry no longer carries it.
        let recs = gait_state::session::daemons::active(dir.path()).unwrap();
        assert!(
            recs.iter().all(|r| r.name != "zombie"),
            "healed dead entry must not remain active() for later sessions"
        );
    }

    #[test]
    fn list_empty_when_no_daemons() {
        let dir = tempfile::TempDir::new().unwrap();
        let outcome = list(dir.path(), false).unwrap();
        assert!(outcome.rows.is_empty());
        let outcome_all = list(dir.path(), true).unwrap();
        assert!(outcome_all.rows.is_empty());
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

    // -- JSON rendering (golden) --

    #[test]
    fn daemon_list_outcome_to_json_golden() {
        let out = DaemonListOutcome {
            rows: vec![DaemonRow {
                name: "build".to_string(),
                pid: 1234,
                status: "running".to_string(),
                log_path: "/tmp/build.log".to_string(),
            }],
        };
        assert_eq!(
            out.to_json(false),
            "[{\"name\":\"build\",\"pid\":1234,\"status\":\"running\",\"log_path\":\"/tmp/build.log\"}]"
        );
    }

    #[test]
    fn daemon_list_outcome_to_json_empty() {
        let out = DaemonListOutcome { rows: vec![] };
        assert_eq!(out.to_json(false), "[]");
    }

    #[test]
    fn stop_outcome_to_json_golden() {
        let out = StopOutcome {
            id: "build".to_string(),
            pid: 1234,
            was_alive: true,
            signalled: true,
        };
        assert_eq!(
            out.to_json(false),
            "{\"id\":\"build\",\"pid\":1234,\"was_alive\":true,\"signalled\":true}"
        );
    }
}

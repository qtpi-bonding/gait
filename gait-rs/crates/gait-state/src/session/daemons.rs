//! Global daemon registry: `<gait_dir>/daemons.jsonl`.
//!
//! The registry is a flat append-only file shared by **concurrent** `gait`
//! processes.  Each line is one JSON-encoded [`Line`] (either a `Start` or a
//! `Stop` marker).  Writes are protected by an exclusive `flock` held only
//! for the duration of the append.
//!
//! `active()` folds the full file: Start inserts by name, Stop removes.
//! `is_alive()` double-checks liveness *and* identity (start-time) to avoid
//! sending signals to a pid-recycled stranger.

use std::fs::OpenOptions;
use std::io::{self, BufRead, BufReader, Write};
use std::path::Path;

use fs2::FileExt;

use crate::proto::gait::log::v1 as pb;

// ---------------------------------------------------------------------------
// Internal wire type (what we actually write to the file).
// The variant bodies are pbjson-serialized proto messages (they get serde
// impls from pbjson); the outer enum is internally tagged as `kind` so the
// line on disk looks like `{"kind":"start",...}` / `{"kind":"stop",...}`.
// ---------------------------------------------------------------------------
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Line {
    Start(pb::DaemonStart),
    Stop(pb::DaemonStop),
}

// ---------------------------------------------------------------------------
// Registry API
// ---------------------------------------------------------------------------

/// Append a `DaemonStart` entry to `<gait_dir>/daemons.jsonl`.
///
/// Returns `Err(AlreadyExists)` if a live daemon with the same name is already
/// registered in the gait-dir registry.  Note: this check is advisory (TOCTOU
/// window between the read and the write); for a CLI tool that is acceptable.
pub fn append_start(gait_dir: &Path, rec: &pb::DaemonStart) -> io::Result<()> {
    // Check for duplicate name among currently-active records.
    let existing = active(gait_dir)?;
    if existing.iter().any(|r| r.name == rec.name) {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "daemon '{}' is already registered (use a different name or stop it first)",
                rec.name
            ),
        ));
    }
    let path = gait_dir.join("daemons.jsonl");
    let mut f = OpenOptions::new().create(true).append(true).open(&path)?;
    f.lock_exclusive()?;
    let line = serde_json::to_string(&Line::Start(rec.clone()))
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    writeln!(f, "{}", line)?;
    f.unlock()?;
    Ok(())
}

/// Append a `DaemonStop` entry to `<gait_dir>/daemons.jsonl`.
///
/// `exit_code` is the exit status of the daemon command: 0 for normal exit,
/// 124 for timeout-kill, negative for signal-terminated processes.
pub fn append_stop(gait_dir: &Path, name: &str, exit_code: i32) -> io::Result<()> {
    let path = gait_dir.join("daemons.jsonl");
    let mut f = OpenOptions::new().create(true).append(true).open(&path)?;
    f.lock_exclusive()?;
    let line = serde_json::to_string(&Line::Stop(pb::DaemonStop {
        name: name.to_owned(),
        exit_code,
    }))
    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    writeln!(f, "{}", line)?;
    f.unlock()?;
    Ok(())
}

/// Read `<gait_dir>/daemons.jsonl` and return the last-known record for every
/// daemon that has a `Start` with no subsequent `Stop`.
pub fn active(gait_dir: &Path) -> io::Result<Vec<pb::DaemonStart>> {
    let path = gait_dir.join("daemons.jsonl");
    if !path.exists() {
        return Ok(vec![]);
    }
    let f = OpenOptions::new().read(true).open(&path)?;
    f.lock_shared()?;
    let reader = BufReader::new(&f);
    let mut records: Vec<pb::DaemonStart> = Vec::new();
    for line in reader.lines() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<Line>(trimmed) {
            Ok(Line::Start(rec)) => {
                if let Some(existing) = records.iter_mut().find(|r| r.name == rec.name) {
                    *existing = rec;
                } else {
                    records.push(rec);
                }
            }
            Ok(Line::Stop(s)) => {
                records.retain(|r| r.name != s.name);
            }
            Err(_) => {
                // Tolerate malformed lines — skip silently.
            }
        }
    }
    f.unlock()?;
    Ok(records)
}

// ---------------------------------------------------------------------------
// Liveness + identity check
// ---------------------------------------------------------------------------

/// Names + exit codes of daemons owned by `session` that have a `Stop`
/// record. `active()` drops stopped entries, so this surfaces them for
/// completion polling. Reads the registry directly with a shared lock; an
/// absent or unreadable file is not an error.
pub fn stopped_for_session(gait_dir: &Path, session: &str) -> Vec<(String, i32)> {
    use std::collections::HashMap;
    let path = gait_dir.join("daemons.jsonl");
    if !path.exists() {
        return vec![];
    }
    let Ok(f) = OpenOptions::new().read(true).open(&path) else {
        return vec![];
    };
    let _ = f.lock_shared();
    let reader = BufReader::new(&f);
    // name -> owning session, name -> recorded exit_code
    let mut starts: HashMap<String, String> = HashMap::new();
    let mut stops: HashMap<String, i32> = HashMap::new();
    for line in reader.lines().map_while(Result::ok) {
        match serde_json::from_str::<Line>(line.trim()) {
            Ok(Line::Start(s)) => {
                starts.insert(s.name.clone(), s.session.clone());
            }
            Ok(Line::Stop(s)) => {
                stops.insert(s.name.clone(), s.exit_code);
            }
            _ => {}
        }
    }
    let _ = f.unlock();
    stops
        .into_iter()
        .filter(|(name, _)| starts.get(name).map(|s| s == session).unwrap_or(false))
        .collect()
}

/// Returns `true` iff the process described by `rec` is still running **and**
/// has the same start-time (guards against pid reuse).
///
/// Fail-closed: any read failure → `false`.
pub fn is_alive(rec: &pb::DaemonStart) -> bool {
    // 1. Is the pid present in the kernel's process table?
    if unsafe { libc::kill(rec.pid, 0) } != 0 {
        return false;
    }
    // 2. Identity check: start-time must match what we recorded.
    match crate::process::proc_start_time(rec.pid) {
        Some(t) => t == rec.started_at,
        None => false, // fail-closed
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::CommandExt;
    use std::sync::Arc;
    use tempfile::TempDir;

    fn make_rec(name: &str, pid: i32) -> pb::DaemonStart {
        pb::DaemonStart {
            name: name.to_owned(),
            pid,
            pgid: pid,
            started_at: 12345,
            cmd: "echo hello".to_owned(),
            cwd: "/tmp".to_owned(),
            log_path: "/tmp/daemon.log".to_owned(),
            session: String::new(),
        }
    }

    #[test]
    fn round_trip_append_and_active() {
        let dir = TempDir::new().unwrap();
        let rec = make_rec("svc-a", 9999);
        append_start(dir.path(), &rec).unwrap();
        let active = active(dir.path()).unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].name, "svc-a");
        assert_eq!(active[0].pid, 9999);
    }

    #[test]
    fn stop_removes_entry() {
        let dir = TempDir::new().unwrap();
        append_start(dir.path(), &make_rec("svc-b", 1111)).unwrap();
        append_start(dir.path(), &make_rec("svc-c", 2222)).unwrap();
        append_stop(dir.path(), "svc-b", 0).unwrap();
        let active = active(dir.path()).unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].name, "svc-c");
    }

    /// `append_stop` must persist the exit code on disk so consumers can read
    /// it back from the raw jsonl. (active() drops stopped entries, so we
    /// inspect the file directly.)
    #[test]
    fn stop_records_exit_code() {
        let dir = TempDir::new().unwrap();
        append_start(dir.path(), &make_rec("svc-x", 4242)).unwrap();
        append_stop(dir.path(), "svc-x", 124).unwrap();
        let raw = std::fs::read_to_string(dir.path().join("daemons.jsonl")).unwrap();
        assert!(raw.contains("\"kind\":\"stop\""), "got: {raw}");
        // pbjson serializes proto fields as camelCase by default; tolerate either.
        assert!(
            raw.contains("\"exit_code\":124") || raw.contains("\"exitCode\":124"),
            "got: {raw}"
        );
    }

    #[test]
    fn is_alive_true_for_self() {
        let my_pid = std::process::id() as i32;
        let start = crate::process::proc_start_time(my_pid)
            .expect("proc_start_time should work for own pid");
        let rec = pb::DaemonStart {
            name: "self".to_owned(),
            pid: my_pid,
            pgid: my_pid,
            started_at: start,
            cmd: "test".to_owned(),
            cwd: "".to_owned(),
            log_path: "".to_owned(),
            session: String::new(),
        };
        assert!(is_alive(&rec), "current process should be alive");
    }

    #[test]
    fn is_alive_false_for_dead_pid() {
        // Use pid 1 with a deliberately wrong start-time so identity check fails.
        let rec = pb::DaemonStart {
            name: "dead".to_owned(),
            pid: 1,
            pgid: 1,
            started_at: u64::MAX, // will never match
            cmd: "init".to_owned(),
            cwd: "".to_owned(),
            log_path: "".to_owned(),
            session: String::new(),
        };
        assert!(
            !is_alive(&rec),
            "wrong start-time should fail identity check"
        );
    }

    // -----------------------------------------------------------------------
    // Integration tests: live process + per-turn sweep + stop
    // -----------------------------------------------------------------------

    /// Spawn a real `sleep 999` in its own session (setsid), register it, then
    /// verify that `active()` + `is_alive()` reports it alive.
    #[test]
    fn active_sweep_reports_live_process_as_alive() {
        use std::process::Command;

        let dir = TempDir::new().unwrap();

        // Spawn sleep 999 as a child; it gets a new session via pre_exec setsid.
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
        let pgid = unsafe { libc::getpgid(pid) };
        assert!(pgid > 0, "getpgid should succeed");

        let started_at = crate::process::proc_start_time(pid).expect("proc_start_time for child");

        let rec = pb::DaemonStart {
            name: "test-sleep-alive".to_owned(),
            pid,
            pgid,
            started_at,
            cmd: "sleep 999".to_owned(),
            cwd: "/tmp".to_owned(),
            log_path: "/tmp/test-sleep.log".to_owned(),
            session: String::new(),
        };

        append_start(dir.path(), &rec).unwrap();

        let recs = active(dir.path()).unwrap();
        assert_eq!(recs.len(), 1, "one active daemon expected");
        let found = &recs[0];
        assert_eq!(found.name, "test-sleep-alive");
        assert!(is_alive(found), "live sleep process should be alive");

        // Cleanup: kill the child before the test exits.
        let _ = child.kill();
        let _ = child.wait();
    }

    /// After stopping a live daemon (killpg + append_stop), `active()` must no
    /// longer list it and `is_alive()` must return false.
    #[test]
    fn stop_removes_live_daemon_and_is_alive_flips_false() {
        use std::process::Command;

        let dir = TempDir::new().unwrap();

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
        let pgid = unsafe { libc::getpgid(pid) };
        assert!(pgid > 0);

        let started_at = crate::process::proc_start_time(pid).expect("proc_start_time for child");

        let rec = pb::DaemonStart {
            name: "test-sleep-stop".to_owned(),
            pid,
            pgid,
            started_at,
            cmd: "sleep 999".to_owned(),
            cwd: "/tmp".to_owned(),
            log_path: "/tmp/test-sleep-stop.log".to_owned(),
            session: String::new(),
        };

        append_start(dir.path(), &rec).unwrap();

        // Replicate cmd_daemons_stop logic: identity-checked kill + append_stop.
        assert!(is_alive(&rec), "must be alive before stop");
        unsafe { libc::killpg(pgid, libc::SIGTERM) };
        // Give the kernel a moment to reap.
        let _ = child.wait();

        append_stop(dir.path(), "test-sleep-stop", 0).unwrap();

        // 1. active() must not list it any more.
        let recs = active(dir.path()).unwrap();
        assert!(
            recs.iter().all(|r| r.name != "test-sleep-stop"),
            "stopped daemon must not appear in active()"
        );

        // 2. is_alive must return false.
        assert!(!is_alive(&rec), "is_alive must be false after stop");
    }

    #[test]
    fn concurrent_appends_do_not_corrupt() {
        let dir = Arc::new(TempDir::new().unwrap());
        let n = 20usize;
        let handles: Vec<_> = (0..n)
            .map(|i| {
                let d = Arc::clone(&dir);
                std::thread::spawn(move || {
                    let rec = make_rec(&format!("svc-{}", i), (1000 + i) as i32);
                    append_start(d.path(), &rec).unwrap();
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let active = active(dir.path()).unwrap();
        assert_eq!(active.len(), n, "all {} records should be present", n);
    }
}

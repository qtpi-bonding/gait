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
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonRecord {
    pub name: String,
    pub pid: i32,
    pub pgid: i32,
    pub started_at: u64,
    pub cmd: String,
    pub cwd: String,
    pub log_path: String,
}

// ---------------------------------------------------------------------------
// Internal wire type (what we actually write to the file)
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Line {
    Start(DaemonRecord),
    Stop { name: String },
}

// ---------------------------------------------------------------------------
// Registry API
// ---------------------------------------------------------------------------

/// Append a `DaemonStart` entry to `<gait_dir>/daemons.jsonl`.
///
/// Returns `Err(AlreadyExists)` if a live daemon with the same name is already
/// registered in the gait-dir registry.  Note: this check is advisory (TOCTOU
/// window between the read and the write); for a CLI tool that is acceptable.
pub fn append_start(gait_dir: &Path, rec: &DaemonRecord) -> io::Result<()> {
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
pub fn append_stop(gait_dir: &Path, name: &str) -> io::Result<()> {
    let path = gait_dir.join("daemons.jsonl");
    let mut f = OpenOptions::new().create(true).append(true).open(&path)?;
    f.lock_exclusive()?;
    let line = serde_json::to_string(&Line::Stop {
        name: name.to_owned(),
    })
    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    writeln!(f, "{}", line)?;
    f.unlock()?;
    Ok(())
}

/// Read `<gait_dir>/daemons.jsonl` and return the last-known record for every
/// daemon that has a `Start` with no subsequent `Stop`.
pub fn active(gait_dir: &Path) -> io::Result<Vec<DaemonRecord>> {
    let path = gait_dir.join("daemons.jsonl");
    if !path.exists() {
        return Ok(vec![]);
    }
    let f = OpenOptions::new().read(true).open(&path)?;
    f.lock_shared()?;
    let reader = BufReader::new(&f);
    // Maintain insertion order: Vec of (name, record); update/remove in-place.
    let mut records: Vec<DaemonRecord> = Vec::new();
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
            Ok(Line::Stop { name }) => {
                records.retain(|r| r.name != name);
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

/// Returns `true` iff the process described by `rec` is still running **and**
/// has the same start-time (guards against pid reuse).
///
/// Fail-closed: any read failure → `false`.
pub fn is_alive(rec: &DaemonRecord) -> bool {
    // 1. Is the pid present in the kernel's process table?
    if unsafe { libc::kill(rec.pid, 0) } != 0 {
        return false;
    }
    // 2. Identity check: start-time must match what we recorded.
    match proc_start_time(rec.pid) {
        Some(t) => t == rec.started_at,
        None => false, // fail-closed
    }
}

/// Return the kernel-reported process start time (seconds since boot epoch)
/// for the given pid.  Returns `None` on any error.
///
/// `pub(crate)` so Task 2's `spawn_detached_to_file` can call it directly.
pub(crate) fn proc_start_time(pid: i32) -> Option<u64> {
    #[cfg(target_os = "macos")]
    {
        proc_start_time_macos(pid)
    }
    #[cfg(target_os = "linux")]
    {
        proc_start_time_linux(pid)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = pid;
        None
    }
}

// ---------------------------------------------------------------------------
// Platform implementations
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
fn proc_start_time_macos(pid: i32) -> Option<u64> {
    // Define only the fields of extern_proc we need.
    // On macOS (64-bit), p_un.__p_starttime is the FIRST field in extern_proc
    // (it is inside a union p_un at offset 0).
    // timeval on Darwin 64-bit: { tv_sec: i64, tv_usec: i32 } = 12 bytes, padded to 16.
    // We read KERN_PROC / KERN_PROC_PID into a raw buffer large enough for
    // kinfo_proc (~648 bytes on arm64/x86_64) and extract bytes [0..8] as tv_sec.
    const KINFO_PROC_SIZE: usize = 648;
    let mut buf = [0u8; KINFO_PROC_SIZE];
    let mut size: libc::size_t = KINFO_PROC_SIZE;
    let mut mib: [libc::c_int; 4] = [libc::CTL_KERN, libc::KERN_PROC, libc::KERN_PROC_PID, pid];
    let ret = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            4,
            buf.as_mut_ptr() as *mut libc::c_void,
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if ret != 0 || size < 8 {
        return None;
    }
    // extern_proc.p_un.__p_starttime.tv_sec is at offset 0 of extern_proc,
    // and extern_proc is the first field of kinfo_proc (kp_proc).
    // On 64-bit Darwin tv_sec is a long (8 bytes, little-endian).
    let tv_sec = i64::from_ne_bytes(buf[0..8].try_into().ok()?);
    if tv_sec <= 0 {
        None
    } else {
        Some(tv_sec as u64)
    }
}

#[cfg(target_os = "linux")]
fn proc_start_time_linux(pid: i32) -> Option<u64> {
    use std::fs;
    // /proc/<pid>/stat — field 22 (1-indexed) is starttime (clock ticks since boot).
    // We return it as raw ticks; callers must compare with a value obtained the
    // same way, so units cancel out.
    let content = fs::read_to_string(format!("/proc/{}/stat", pid)).ok()?;
    // The second field is the comm wrapped in parens and may contain spaces.
    // Find the last ')' to skip the comm field safely.
    let after_comm = content.rfind(')')?.checked_add(1)?;
    let rest = content[after_comm..].trim_start();
    // Remaining fields are space-separated; starttime is at 0-indexed position 19
    // (fields 3..21 counting from after comm, 1-indexed fields 3–22 in the man page).
    let fields: Vec<&str> = rest.split_whitespace().collect();
    // field 22 (1-indexed) = index 19 in the post-comm slice (fields start at 3).
    let starttime_str = fields.get(19)?;
    starttime_str.parse::<u64>().ok()
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

    fn make_rec(name: &str, pid: i32) -> DaemonRecord {
        DaemonRecord {
            name: name.to_owned(),
            pid,
            pgid: pid,
            started_at: 12345,
            cmd: "echo hello".to_owned(),
            cwd: "/tmp".to_owned(),
            log_path: "/tmp/daemon.log".to_owned(),
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
        append_stop(dir.path(), "svc-b").unwrap();
        let active = active(dir.path()).unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].name, "svc-c");
    }

    #[test]
    fn is_alive_true_for_self() {
        let my_pid = std::process::id() as i32;
        let start = proc_start_time(my_pid).expect("proc_start_time should work for own pid");
        let rec = DaemonRecord {
            name: "self".to_owned(),
            pid: my_pid,
            pgid: my_pid,
            started_at: start,
            cmd: "test".to_owned(),
            cwd: "".to_owned(),
            log_path: "".to_owned(),
        };
        assert!(is_alive(&rec), "current process should be alive");
    }

    #[test]
    fn is_alive_false_for_dead_pid() {
        // Use pid 1 with a deliberately wrong start-time so identity check fails.
        let rec = DaemonRecord {
            name: "dead".to_owned(),
            pid: 1,
            pgid: 1,
            started_at: u64::MAX, // will never match
            cmd: "init".to_owned(),
            cwd: "".to_owned(),
            log_path: "".to_owned(),
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

        let started_at = proc_start_time(pid).expect("proc_start_time for child");

        let rec = DaemonRecord {
            name: "test-sleep-alive".to_owned(),
            pid,
            pgid,
            started_at,
            cmd: "sleep 999".to_owned(),
            cwd: "/tmp".to_owned(),
            log_path: "/tmp/test-sleep.log".to_owned(),
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

        let started_at = proc_start_time(pid).expect("proc_start_time for child");

        let rec = DaemonRecord {
            name: "test-sleep-stop".to_owned(),
            pid,
            pgid,
            started_at,
            cmd: "sleep 999".to_owned(),
            cwd: "/tmp".to_owned(),
            log_path: "/tmp/test-sleep-stop.log".to_owned(),
        };

        append_start(dir.path(), &rec).unwrap();

        // Replicate cmd_daemons_stop logic: identity-checked kill + append_stop.
        assert!(is_alive(&rec), "must be alive before stop");
        unsafe { libc::killpg(pgid, libc::SIGTERM) };
        // Give the kernel a moment to reap.
        let _ = child.wait();

        append_stop(dir.path(), "test-sleep-stop").unwrap();

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

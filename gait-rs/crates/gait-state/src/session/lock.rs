//! Per-session RAII PID-file lock.
//!
//! A pure path-based file lock: no git handle, no `Repos`. Both the state layer
//! (session log, completion) and the turn engine reach it here — defining it in
//! `session/` keeps the dependency direction state←engine.

use std::path::{Path, PathBuf};

// ── Session lock (RAII PID file) ─────────────────────────────────────────────

/// Another process holds the session's lock. [`SessionLock::acquire`] returns it
/// inside its `anyhow::Error`; a caller that treats "busy" differently from an
/// I/O failure checks for it with `downcast_ref`.
#[derive(Debug, thiserror::Error)]
#[error("session {dir} is already running (PID {pid})")]
pub struct AlreadyRunning {
    pub dir: String,
    pub pid: String,
}

pub struct SessionLock {
    path: PathBuf,
}

/// Read-only: does a live process currently hold this session's lock?
/// No side effects — never writes or removes the lock file.
fn pid_alive(pid: u32) -> bool {
    if std::path::Path::new(&format!("/proc/{}", pid)).exists() {
        return true;
    }
    // SAFETY: kill(pid, 0) sends no signal; it only checks if the process exists.
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

/// Start time of `pid` in clock ticks since boot (field 22 of `/proc/<pid>/stat`).
/// `None` where `/proc` is unavailable (macOS) or the process is gone.
fn proc_starttime(pid: u32) -> Option<u64> {
    starttime_from_stat(&std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
}

/// The command name may contain spaces and parentheses; fields resume after
/// the last ')'. After it, `state` is field 3, so field 22 is index 19.
fn starttime_from_stat(stat: &str) -> Option<u64> {
    stat.rsplit_once(')')?.1.split_whitespace().nth(19)?.parse().ok()
}

/// Is the process recorded in a lock file still the one that wrote it? A pid
/// alone is not enough: the lock lives in a workspace that outlives its
/// container, and pids restart from the same numbers, so a lock left by a
/// killed process can name a pid that a different live process (even this
/// one) now holds. The recorded start time tells them apart.
fn holder_is_live(
    raw: &str,
    alive: impl Fn(u32) -> bool,
    starttime: impl Fn(u32) -> Option<u64>,
) -> bool {
    let mut fields = raw.split_whitespace();
    let Some(pid) = fields.next().and_then(|p| p.parse::<u32>().ok()) else {
        return false;
    };
    if !alive(pid) {
        return false;
    }
    match (fields.next().and_then(|s| s.parse::<u64>().ok()), starttime(pid)) {
        (Some(recorded), Some(current)) => recorded == current,
        _ => true,
    }
}

fn lock_is_held(raw: &str) -> bool {
    holder_is_live(raw, pid_alive, proc_starttime)
}

impl SessionLock {
    /// Read-only peek used by bulk classification. No side effects.
    pub fn is_running(session_dir: &std::path::Path) -> bool {
        let path = session_dir.join("lock");
        std::fs::read_to_string(&path).is_ok_and(|raw| lock_is_held(&raw))
    }

    pub fn acquire(session_dir: &Path) -> anyhow::Result<Self> {
        let path = session_dir.join("lock");
        if path.exists() {
            let raw = std::fs::read_to_string(&path).unwrap_or_default();
            if lock_is_held(&raw) {
                return Err(AlreadyRunning {
                    dir: session_dir.display().to_string(),
                    pid: raw.split_whitespace().next().unwrap_or("?").to_owned(),
                }
                .into());
            }
        }
        let pid = std::process::id();
        let contents = match proc_starttime(pid) {
            Some(start) => format!("{pid} {start}"),
            None => pid.to_string(),
        };
        std::fs::write(&path, contents)?;
        Ok(Self { path })
    }
}

impl Drop for SessionLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::SessionLock;
    use tempfile::TempDir;

    #[test]
    fn is_running_false_when_no_lock() {
        let dir = TempDir::new().unwrap();
        assert!(!SessionLock::is_running(dir.path()));
    }

    #[test]
    fn is_running_false_on_stale_pid() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("lock"), "999999999").unwrap();
        assert!(!SessionLock::is_running(dir.path()));
    }

    #[test]
    fn acquiring_a_held_lock_fails_with_the_typed_already_running_error() {
        let dir = TempDir::new().unwrap();
        let _held = SessionLock::acquire(dir.path()).unwrap();
        let error = SessionLock::acquire(dir.path()).err().expect("second acquire fails");
        let busy = error.downcast_ref::<super::AlreadyRunning>().expect("typed busy error");
        assert_eq!(busy.pid, std::process::id().to_string());
    }

    #[test]
    fn is_running_true_for_self() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("lock"), std::process::id().to_string()).unwrap();
        assert!(SessionLock::is_running(dir.path()));
    }

    #[test]
    fn a_lock_naming_a_reused_pid_is_stale() {
        // Same pid, different start time: a different process now holds the number.
        assert!(!super::holder_is_live("7 100", |_| true, |_| Some(200)));
        assert!(super::holder_is_live("7 100", |_| true, |_| Some(100)));
    }

    #[test]
    fn a_lock_without_a_start_time_falls_back_to_the_pid() {
        assert!(super::holder_is_live("7", |_| true, |_| Some(200)));
        assert!(super::holder_is_live("7 100", |_| true, |_| None));
        assert!(!super::holder_is_live("7 100", |_| false, |_| Some(100)));
    }

    #[test]
    fn starttime_parses_a_command_name_with_spaces_and_parens() {
        // After "(name)": state, then fields 4..=21 (18 numbers), then starttime.
        let stat = format!("42 (a b) c) S {} 777 23", (4..=21).map(|n| n.to_string()).collect::<Vec<_>>().join(" "));
        assert_eq!(super::starttime_from_stat(&stat), Some(777));
        assert_eq!(super::starttime_from_stat("42 (short) S 1"), None);
    }

    #[test]
    fn is_running_false_on_garbage() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("lock"), "not-a-pid").unwrap();
        assert!(!SessionLock::is_running(dir.path()));
    }
}

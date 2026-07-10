//! Per-session RAII PID-file lock.
//!
//! A pure path-based file lock: no git handle, no `Repos`. Both the state layer
//! (session log, completion) and the turn engine reach it here — defining it in
//! `session/` keeps the dependency direction state←engine.

use std::path::{Path, PathBuf};

// ── Session lock (RAII PID file) ─────────────────────────────────────────────

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

impl SessionLock {
    /// Read-only peek used by bulk classification. No side effects.
    pub fn is_running(session_dir: &std::path::Path) -> bool {
        let path = session_dir.join("lock");
        let Ok(raw) = std::fs::read_to_string(&path) else {
            return false;
        };
        match raw.trim().parse::<u32>() {
            Ok(pid) => pid_alive(pid),
            Err(_) => false,
        }
    }

    pub fn acquire(session_dir: &Path) -> anyhow::Result<Self> {
        let path = session_dir.join("lock");
        if path.exists() {
            let raw = std::fs::read_to_string(&path).unwrap_or_default();
            if let Ok(pid) = raw.trim().parse::<u32>() {
                if pid_alive(pid) {
                    anyhow::bail!(
                        "session {} is already running (PID {})",
                        session_dir.display(),
                        pid
                    );
                }
            }
        }
        std::fs::write(&path, std::process::id().to_string())?;
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
    fn is_running_true_for_self() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("lock"), std::process::id().to_string()).unwrap();
        assert!(SessionLock::is_running(dir.path()));
    }

    #[test]
    fn is_running_false_on_garbage() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("lock"), "not-a-pid").unwrap();
        assert!(!SessionLock::is_running(dir.path()));
    }
}

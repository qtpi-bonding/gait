//! A command spawned into its OWN process group, so the shell-tool timeout can
//! SIGKILL the whole subtree (sh → flutter → dart) without touching the cascade
//! group (brain + other hands).
//!
//! Process-group membership is orthogonal to cgroup membership: on Linux the
//! child stays in the work cgroup it inherited, so the spine's recursive
//! `cgroup.kill` still sweeps it on teardown. macOS (and the Linux pgid-degrade
//! path) rely on the hand's SIGTERM guard (`kill_active`) for teardown cleanup.

use std::sync::atomic::{AtomicI32, Ordering};

/// Configure `cmd` so the spawned child leads a NEW process group (pgid == pid).
/// Uses tokio's inherent `process_group(0)` (translates to `setpgid(pid, 0)`),
/// matching the std→`From` pattern at `spine.rs`. Unix only; no-op else.
/// Call before `spawn`.
pub fn configure_own_group(cmd: &mut tokio::process::Command) {
    #[cfg(unix)]
    {
        cmd.process_group(0);
    }
    #[cfg(not(unix))]
    {
        let _ = cmd;
    }
}

/// A kill handle for a process subtree, named by its process-group id.
#[derive(Clone, Copy)]
pub struct Scope {
    pgid: i32,
}

impl Scope {
    /// A child spawned via `configure_own_group` leads its own group, so its
    /// pid IS the pgid.
    pub fn from_leader_pid(pid: i32) -> Self {
        Self { pgid: pid }
    }

    /// SIGKILL the whole group. Best-effort and idempotent (a second call after
    /// the group is gone is a harmless ESRCH).
    pub fn kill(&self) {
        #[cfg(unix)]
        if self.pgid > 0 {
            unsafe {
                libc::killpg(self.pgid, libc::SIGKILL);
            }
        }
    }

    pub fn pgid(&self) -> i32 {
        self.pgid
    }
}

// ── Active-scope registry (for the hand's SIGTERM guard, Task 6) ──────────────
// A hand runs exactly one tool at a time, so a single slot suffices.
static ACTIVE_PGID: AtomicI32 = AtomicI32::new(0);

/// Register the in-flight command's pgid so a teardown SIGTERM can forward a
/// SIGKILL to its subtree before the hand exits.
pub fn set_active(pgid: i32) {
    ACTIVE_PGID.store(pgid, Ordering::SeqCst);
}

/// Clear the registry once the command has completed (or been killed).
pub fn clear_active() {
    ACTIVE_PGID.store(0, Ordering::SeqCst);
}

/// SIGKILL the currently-registered subtree, if any. Called by the SIGTERM guard.
pub fn kill_active() {
    let pgid = ACTIVE_PGID.load(Ordering::SeqCst);
    #[cfg(unix)]
    if pgid > 0 {
        unsafe {
            libc::killpg(pgid, libc::SIGKILL);
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Poll up to ~2s for the group to disappear. Orphaned (SIGKILLed) members are
    /// reparented to init and reaped asynchronously, so `killpg(pgid, 0)` may
    /// briefly still see a zombie member — retry instead of a fixed sleep.
    async fn await_group_gone(pgid: i32) -> bool {
        for _ in 0..40 {
            if unsafe { libc::killpg(pgid, 0) } == -1 {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        false
    }

    #[tokio::test]
    async fn kill_terminates_whole_group() {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c").arg("sleep 30 & sleep 30 & wait");
        configure_own_group(&mut cmd);
        let mut child = cmd.spawn().expect("spawn");
        let pid = child.id().expect("pid") as i32;
        // Child leads its own group → the group exists.
        assert_eq!(
            unsafe { libc::killpg(pid, 0) },
            0,
            "group should exist pre-kill"
        );

        Scope::from_leader_pid(pid).kill();
        let _ = child.wait().await; // reap leader so no zombie keeps the group "alive"

        assert!(
            await_group_gone(pid).await,
            "group should be gone after kill"
        );
    }

    #[tokio::test]
    async fn kill_is_idempotent_on_dead_group() {
        let s = Scope::from_leader_pid(2_000_000_000); // almost-certainly nonexistent
        s.kill();
        s.kill(); // no panic
    }
}

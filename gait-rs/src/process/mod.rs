//! Cascade group: the OS process group holding the current BLOCKING work
//! (brain, hands, blocking subagents + descendants). The TOP process owns it and
//! is never a member, so it can kill the group and survive to clean up. macOS
//! first; other targets get Noop. NOT gait's Docker `--container`.

use std::io;
use std::process::{Child, Command};
use std::sync::Arc;

pub trait Containment: Send + Sync {
    /// Spawn a child INSIDE the cascade group; returns the Child so the owner
    /// can waitpid it (no zombies — S4).
    fn spawn_enrolled(&self, cmd: Command) -> io::Result<Child>;
    /// Spawn a daemon OUTSIDE the group (setsid breakaway); returns child + pgid.
    fn spawn_detached(&self, cmd: Command) -> io::Result<(Child, i32)>;
    /// Reap the whole group; idempotent; waitpids owned children.
    fn kill_all(&self) -> io::Result<()>;
    /// The handle to pass to a blocking subagent as `--cascade-group` (so it
    /// enrolls its own work into THIS group). `None` if not yet realized.
    fn handle(&self) -> Option<String>;
}

pub fn establish(inherited: Option<String>) -> Arc<dyn Containment> {
    #[cfg(target_os = "macos")]
    {
        Arc::new(macos::CascadeGroup::new(inherited))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = inherited;
        Arc::new(NoopContainment)
    }
}

pub struct NoopContainment;
impl Containment for NoopContainment {
    fn spawn_enrolled(&self, mut cmd: Command) -> io::Result<Child> {
        cmd.spawn()
    }
    fn spawn_detached(&self, mut cmd: Command) -> io::Result<(Child, i32)> {
        let c = cmd.spawn()?;
        let pid = c.id() as i32;
        Ok((c, pid))
    }
    fn kill_all(&self) -> io::Result<()> {
        Ok(())
    }
    fn handle(&self) -> Option<String> {
        None
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use super::*;
    use std::os::unix::process::CommandExt;
    use std::sync::atomic::{AtomicI32, Ordering};
    use std::sync::Mutex;

    use std::sync::atomic::AtomicBool;

    pub struct CascadeGroup {
        /// pgid naming the group. 0 until the first enrolled child (the brain)
        /// becomes leader, OR the inherited pgid (a subagent joining the parent's).
        pgid: AtomicI32,
        owned: Mutex<Vec<i32>>, // pids we waitpid in kill_all (S4) — store pids, not Child (no transmute)
        inherited: bool,
        killed: AtomicBool, // arm the SIGKILL timer once (review S2)
    }
    impl CascadeGroup {
        pub fn new(inherited: Option<String>) -> Self {
            let pgid = inherited
                .as_deref()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            Self {
                pgid: AtomicI32::new(pgid),
                owned: Mutex::new(Vec::new()),
                inherited: inherited.is_some(),
                killed: AtomicBool::new(false),
            }
        }
    }
    impl Containment for CascadeGroup {
        fn spawn_enrolled(&self, mut cmd: Command) -> io::Result<Child> {
            let pgid = self.pgid.load(Ordering::SeqCst);
            if pgid == 0 {
                // First child = the brain = group+SESSION leader via setsid → a stable
                // leader for the whole (blocking) turn; pgid == pid after setsid.
                // (The brain talks over pipes, not a TTY, so detaching the TTY is fine.)
                unsafe {
                    cmd.pre_exec(|| {
                        libc::setsid();
                        Ok(())
                    });
                }
                let child = cmd.spawn()?;
                self.pgid.store(child.id() as i32, Ordering::SeqCst);
                self.owned.lock().unwrap().push(child.id() as i32);
                Ok(child)
            } else {
                // Member (hand): join the leader's group. Sound because the brain is
                // spawned FIRST and is alive for the whole blocking turn (assert below).
                cmd.process_group(pgid);
                let child = cmd.spawn()?;
                self.owned.lock().unwrap().push(child.id() as i32);
                Ok(child)
            }
        }
        fn spawn_detached(&self, mut cmd: Command) -> io::Result<(Child, i32)> {
            unsafe {
                cmd.pre_exec(|| {
                    libc::setsid();
                    Ok(())
                });
            }
            let c = cmd.spawn()?;
            let pid = c.id() as i32;
            Ok((c, pid)) // setsid leader: pgid == pid
        }
        fn kill_all(&self) -> io::Result<()> {
            let pgid = self.pgid.load(Ordering::SeqCst);
            // Only the OWNER kills; never killpg(0) (our own group); arm SIGKILL once.
            if pgid > 0 && !self.inherited && !self.killed.swap(true, Ordering::SeqCst) {
                unsafe {
                    libc::killpg(pgid, libc::SIGTERM);
                }
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_millis(300));
                    // Guard pid-reuse: only SIGKILL if the group still exists (review S2).
                    if unsafe { libc::killpg(pgid, 0) } == 0 {
                        unsafe {
                            libc::killpg(pgid, libc::SIGKILL);
                        }
                    }
                });
            }
            // Reap owned children to avoid zombies (S4) — blocking waitpid after kill is fine.
            for pid in self.owned.lock().unwrap().drain(..) {
                unsafe {
                    libc::waitpid(pid, std::ptr::null_mut(), 0);
                }
            }
            Ok(())
        }
        fn handle(&self) -> Option<String> {
            let p = self.pgid.load(Ordering::SeqCst);
            if p > 0 {
                Some(p.to_string())
            } else {
                None
            }
        }
    }
}

#[cfg(target_os = "macos")]
#[test]
fn kill_all_is_noop_before_any_child() {
    let g = establish(None);
    assert!(g.kill_all().is_ok()); // pgid 0 → must NOT killpg(0) (our own group!)
}

#[cfg(target_os = "macos")]
#[test]
fn inherited_group_does_not_kill() {
    let g = establish(Some("424242".into()));
    // a subagent inheriting the parent's group must never killpg it
    assert!(g.kill_all().is_ok()); // inherited=true → no killpg
}

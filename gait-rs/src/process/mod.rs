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
    /// Configure `cmd` to join the cascade group and record the resulting pid
    /// (called by `enroll_tokio` after the tokio spawn). Distinct from
    /// `spawn_enrolled` because the brain uses `tokio::process::Command` so it
    /// can drive async I/O; hands use `std::process::Command` (sync wait).
    ///
    /// On the FIRST call the brain becomes the group leader (setsid pre_exec).
    /// On subsequent calls the hand joins via `process_group(pgid)`.
    ///
    /// Returns the pid to register after spawn.
    fn configure_enrolled(&self, cmd: &mut Command) -> io::Result<()>;
    /// Register a pid that was spawned after `configure_enrolled`. Must be
    /// called exactly once per `configure_enrolled` call so kill_all can reap.
    fn register_pid(&self, pid: i32);
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
    fn configure_enrolled(&self, _cmd: &mut Command) -> io::Result<()> {
        Ok(())
    }
    fn register_pid(&self, _pid: i32) {}
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
                // First child = the brain = new process group leader (setpgid(0,0)).
                // Stays in the spine's session so hands spawned by the spine can join.
                unsafe {
                    cmd.pre_exec(|| {
                        libc::setpgid(0, 0);
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
        fn configure_enrolled(&self, cmd: &mut Command) -> io::Result<()> {
            let pgid = self.pgid.load(Ordering::SeqCst);
            if pgid == 0 {
                // First child (brain): new group leader via setpgid(0,0) — stays in
                // spine's session so hands from the spine can join the group.
                unsafe {
                    cmd.pre_exec(|| {
                        libc::setpgid(0, 0);
                        Ok(())
                    });
                }
            } else {
                // Subsequent children (hands): join the brain's group.
                debug_assert!(
                    self.pgid.load(Ordering::SeqCst) != 0,
                    "hand enrolled before brain: pgid must be set (brain must be spawned first)"
                );
                cmd.process_group(pgid);
            }
            Ok(())
        }
        fn register_pid(&self, pid: i32) {
            if pid <= 0 {
                return;
            }
            // If this is the first enrollment, the brain's pid IS the pgid.
            let prev = self
                .pgid
                .compare_exchange(0, pid, Ordering::SeqCst, Ordering::SeqCst);
            if prev.is_err() {
                // pgid already set (either inherited or brain already registered);
                // just track the pid for reaping.
            }
            self.owned.lock().unwrap().push(pid);
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
            // Reap owned children to avoid zombies (S4) — release mutex before blocking waitpid.
            let pids: Vec<i32> = self.owned.lock().unwrap().drain(..).collect();
            for pid in pids {
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

/// Integration test: brain becomes group leader; hand joins the same group.
/// Asserts `getpgid(hand_pid) == getpgid(brain_pid)` via `libc::getpgid`.
#[cfg(target_os = "macos")]
#[test]
fn brain_and_hand_share_process_group() {
    let g = establish(None);

    // Spawn "brain" (first enrolled = group leader via setpgid(0,0)).
    let mut brain_cmd = std::process::Command::new("sleep");
    brain_cmd.arg("5");
    g.configure_enrolled(&mut brain_cmd)
        .expect("configure brain");
    let mut brain = brain_cmd.spawn().expect("spawn brain");
    let brain_pid = brain.id() as i32;
    g.register_pid(brain_pid);

    // Spawn "hand" (joins brain's group).
    let mut hand_cmd = std::process::Command::new("sleep");
    hand_cmd.arg("5");
    g.configure_enrolled(&mut hand_cmd).expect("configure hand");
    let mut hand = hand_cmd.spawn().expect("spawn hand");
    let hand_pid = hand.id() as i32;
    g.register_pid(hand_pid);

    // Verify both are in the same process group.
    let brain_pgid = unsafe { libc::getpgid(brain_pid) };
    let hand_pgid = unsafe { libc::getpgid(hand_pid) };
    assert!(brain_pgid > 0, "brain_pgid must be positive");
    assert_eq!(
        brain_pgid, hand_pgid,
        "brain and hand must share a process group"
    );

    // Clean up.
    g.kill_all().expect("kill_all");
    let _ = brain.wait();
    let _ = hand.wait();
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

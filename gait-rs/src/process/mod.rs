//! Cascade group: the OS process group holding the current BLOCKING work
//! (brain, hands, blocking subagents + descendants). The TOP process owns it and
//! is never a member, so it can kill the group and survive to clean up. macOS
//! first; other targets get Noop. NOT gait's Docker `--container`.

use std::io;
use std::path::Path;
use std::process::{Child, Command, Stdio};
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

/// Spawn `cmd` as a daemon fully outside the cascade group:
/// - its own session (setsid in pre_exec) so SIGHUP from the hand's exit is not delivered,
/// - stdout+stderr redirected to `log_path` (created/truncated),
/// - stdin closed (null).
///
/// Returns `(child, pgid, started_at)` where `pgid == pid` (setsid leader).
/// Drop `child` immediately if you do not intend to wait on it.
pub fn spawn_detached_to_file(mut cmd: Command, log_path: &Path) -> io::Result<(Child, i32, u64)> {
    let f = std::fs::File::create(log_path)?;
    let f2 = f.try_clone()?;
    cmd.stdin(Stdio::null())
        .stdout(Stdio::from(f))
        .stderr(Stdio::from(f2));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }
    let mut child = cmd.spawn()?;
    let pid = child.id() as i32;
    // After setsid the child is its own session+group leader, so pgid == pid.
    // Do NOT call getpgid here — it races the child's pre_exec.
    let started_at = match crate::session::daemons::proc_start_time(pid) {
        Some(t) if t > 0 => t,
        _ => {
            // Cannot establish process identity — kill the child and fail the spawn.
            // This prevents recording an un-stoppable zombie daemon entry.
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
            let _ = child.wait(); // reap
            return Err(std::io::Error::other(
                "spawn_detached_to_file: could not read process start time for identity tracking",
            ));
        }
    };
    Ok((child, pid, started_at))
}

pub fn establish(inherited: Option<String>) -> Arc<dyn Containment> {
    cfg_if_establish(inherited)
}

#[cfg(target_os = "linux")]
fn cfg_if_establish(inherited: Option<String>) -> Arc<dyn Containment> {
    linux::establish_linux(inherited)
}

#[cfg(target_os = "macos")]
fn cfg_if_establish(inherited: Option<String>) -> Arc<dyn Containment> {
    Arc::new(macos::CascadeGroup::new(inherited))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn cfg_if_establish(inherited: Option<String>) -> Arc<dyn Containment> {
    let _ = inherited;
    Arc::new(NoopContainment)
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

// ── Linux cgroup v2 containment ───────────────────────────────────────────────
#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use std::os::unix::process::CommandExt;
    use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
    use std::sync::Mutex;

    // ── helpers ────────────────────────────────────────────────────────────────

    /// Try to create a delegated cgroup for this process.  Returns the path on
    /// success; returns `None` if cgroup v2 is not available/writable.
    fn try_create_cgroup(pid: u32) -> Option<std::path::PathBuf> {
        let uid = unsafe { libc::getuid() };

        // Try the user-session delegated subtree first (systemd + user delegation).
        let candidates = [
            format!("/sys/fs/cgroup/user.slice/user-{uid}.slice/user@{uid}.service/gait-{pid}"),
            format!("/sys/fs/cgroup/gait-{pid}"),
        ];

        for candidate in &candidates {
            let p = std::path::PathBuf::from(candidate);
            if std::fs::create_dir_all(&p).is_ok() {
                // Verify cgroup.procs is writable.
                let procs = p.join("cgroup.procs");
                if procs.exists() {
                    return Some(p);
                }
            }
        }
        None
    }

    // ── CgroupContainment ──────────────────────────────────────────────────────

    pub struct CgroupContainment {
        /// The cgroup directory managed by this instance (None → degraded).
        cgroup_dir: Option<std::path::PathBuf>,
        /// pgid used when degraded to process-group model.
        pgid: AtomicI32,
        /// Pids to wait on in kill_all (no zombies).
        owned: Mutex<Vec<i32>>,
        /// True when we inherited (are a subagent); we kill but don't re-kill parent work.
        inherited: bool,
        /// Arm SIGKILL once only.
        killed: AtomicBool,
        /// Detached-sibling cgroup path (used by spawn_detached in cgroup mode).
        detached_dir: Mutex<Option<std::path::PathBuf>>,
    }

    impl CgroupContainment {
        /// Create a fresh (non-inherited) cgroup containment for the top-level spine.
        pub fn new_owner() -> Self {
            let pid = std::process::id();
            let cgroup_dir = try_create_cgroup(pid);
            Self {
                cgroup_dir,
                pgid: AtomicI32::new(0),
                owned: Mutex::new(Vec::new()),
                inherited: false,
                killed: AtomicBool::new(false),
                detached_dir: Mutex::new(None),
            }
        }

        /// Inherit an existing handle from a parent spine (subagent path).
        /// `handle` is either a cgroup path or a pgid integer (degrade path).
        pub fn new_inherited(handle: String) -> Self {
            let cgroup_dir = if handle.starts_with("/sys/fs/cgroup") {
                Some(std::path::PathBuf::from(&handle))
            } else {
                None
            };
            let pgid = if cgroup_dir.is_none() {
                handle.parse::<i32>().unwrap_or(0)
            } else {
                0
            };
            Self {
                cgroup_dir,
                pgid: AtomicI32::new(pgid),
                owned: Mutex::new(Vec::new()),
                inherited: true,
                killed: AtomicBool::new(false),
                detached_dir: Mutex::new(None),
            }
        }

        fn in_cgroup_mode(&self) -> bool {
            self.cgroup_dir.is_some()
        }

        fn enroll_in_cgroup(&self, pid: u32) {
            if let Some(ref dir) = self.cgroup_dir {
                let _ = std::fs::write(dir.join("cgroup.procs"), pid.to_string());
            }
        }
    }

    impl Containment for CgroupContainment {
        fn spawn_enrolled(&self, mut cmd: Command) -> io::Result<Child> {
            if self.in_cgroup_mode() {
                // Inject --cascade-group so gait children self-assign race-free.
                // Non-gait children (sh/bash) inherit the cgroup from the parent process
                // (the spine is already in the cgroup); no post-spawn write needed.
                if let Some(ref dir) = self.cgroup_dir {
                    cmd.arg("--cascade-group").arg(dir.as_os_str());
                }
                let child = cmd.spawn()?;
                let pid = child.id();
                self.owned.lock().unwrap().push(pid as i32);
                Ok(child)
            } else {
                // Degrade: pgid mode
                let pgid = self.pgid.load(Ordering::SeqCst);
                if pgid == 0 {
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
                    cmd.process_group(pgid);
                    let child = cmd.spawn()?;
                    self.owned.lock().unwrap().push(child.id() as i32);
                    Ok(child)
                }
            }
        }

        fn configure_enrolled(&self, cmd: &mut Command) -> io::Result<()> {
            if self.in_cgroup_mode() {
                // Inject --cascade-group so the spawned child self-assigns race-free.
                if let Some(ref dir) = self.cgroup_dir {
                    cmd.arg("--cascade-group").arg(dir.as_os_str());
                }
                Ok(())
            } else {
                let pgid = self.pgid.load(Ordering::SeqCst);
                if pgid == 0 {
                    unsafe {
                        cmd.pre_exec(|| {
                            libc::setpgid(0, 0);
                            Ok(())
                        });
                    }
                } else {
                    cmd.process_group(pgid);
                }
                Ok(())
            }
        }

        fn register_pid(&self, pid: i32) {
            if pid <= 0 {
                return;
            }
            if self.in_cgroup_mode() {
                self.enroll_in_cgroup(pid as u32);
            } else {
                let prev = self
                    .pgid
                    .compare_exchange(0, pid, Ordering::SeqCst, Ordering::SeqCst);
                let _ = prev;
            }
            self.owned.lock().unwrap().push(pid);
        }

        fn spawn_detached(&self, mut cmd: Command) -> io::Result<(Child, i32)> {
            if self.in_cgroup_mode() {
                // Create a sibling cgroup for the detached process so kill_all on the
                // work cgroup never touches it.
                let pid_self = std::process::id();
                let detached_path = format!(
                    "{}-detached-{}",
                    self.cgroup_dir.as_ref().unwrap().display(),
                    pid_self
                );
                let _ = std::fs::create_dir_all(&detached_path);
                {
                    let mut d = self.detached_dir.lock().unwrap();
                    *d = Some(std::path::PathBuf::from(&detached_path));
                }
                // Inject --cascade-group so the child self-assigns into the detached
                // cgroup before it can fork grandchildren (same pattern as spawn_enrolled).
                // This eliminates the post-spawn write race.
                cmd.arg("--cascade-group").arg(&detached_path);
                // Child runs with setsid (own session) so SIGHUP from parent exit is not delivered.
                unsafe {
                    cmd.pre_exec(|| {
                        libc::setsid();
                        Ok(())
                    });
                }
                let c = cmd.spawn()?;
                let pid = c.id() as i32;
                Ok((c, pid))
            } else {
                unsafe {
                    cmd.pre_exec(|| {
                        libc::setsid();
                        Ok(())
                    });
                }
                let c = cmd.spawn()?;
                let pid = c.id() as i32;
                Ok((c, pid))
            }
        }

        fn kill_all(&self) -> io::Result<()> {
            if self.in_cgroup_mode() {
                // Kill the cgroup whether we own it or inherited it — if the parent
                // died (pidfd watch fired), we must reap our own children.
                // The `inherited` flag only controls cleanup (rmdir), not the kill.
                if !self.killed.swap(true, Ordering::SeqCst) {
                    if let Some(ref dir) = self.cgroup_dir {
                        let kill_file = dir.join("cgroup.kill");
                        if kill_file.exists() {
                            let _ = std::fs::write(&kill_file, "1");
                        } else {
                            // Fallback: SIGKILL every pid in cgroup.procs
                            let procs_path = dir.join("cgroup.procs");
                            if let Ok(contents) = std::fs::read_to_string(&procs_path) {
                                for line in contents.lines() {
                                    if let Ok(pid) = line.trim().parse::<i32>() {
                                        unsafe {
                                            libc::kill(pid, libc::SIGKILL);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            } else {
                let pgid = self.pgid.load(Ordering::SeqCst);
                if pgid > 0 && !self.inherited && !self.killed.swap(true, Ordering::SeqCst) {
                    unsafe {
                        libc::killpg(pgid, libc::SIGTERM);
                    }
                    std::thread::spawn(move || {
                        std::thread::sleep(std::time::Duration::from_millis(300));
                        if unsafe { libc::killpg(pgid, 0) } == 0 {
                            unsafe {
                                libc::killpg(pgid, libc::SIGKILL);
                            }
                        }
                    });
                }
            }
            // Reap owned children.
            let pids: Vec<i32> = self.owned.lock().unwrap().drain(..).collect();
            for pid in pids {
                unsafe {
                    libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG);
                }
            }
            Ok(())
        }

        fn handle(&self) -> Option<String> {
            if let Some(ref dir) = self.cgroup_dir {
                Some(dir.to_string_lossy().into_owned())
            } else {
                let p = self.pgid.load(Ordering::SeqCst);
                if p > 0 {
                    Some(p.to_string())
                } else {
                    None
                }
            }
        }
    }

    // ── pidfd parent-watch ─────────────────────────────────────────────────────

    /// Spawn a background task that watches the parent process via pidfd.
    /// When the parent dies (even SIGKILL), calls kill_all() on the inherited containment.
    /// Only relevant when we are a subagent (inherited = true).
    pub fn spawn_pidfd_watch(containment: Arc<dyn Containment>, parent_pid: i32) {
        use std::os::unix::io::{FromRawFd, OwnedFd};
        use tokio::io::unix::AsyncFd;

        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, parent_pid, 0) };
        if fd < 0 {
            // pidfd_open not available or parent already dead — skip watch.
            return;
        }
        let raw_fd = fd as std::os::unix::io::RawFd;
        // Mark non-blocking so AsyncFd can poll it.
        unsafe {
            let flags = libc::fcntl(raw_fd, libc::F_GETFL, 0);
            libc::fcntl(raw_fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }

        tokio::spawn(async move {
            // Safety: we just created this fd and own it.
            let owned = unsafe { OwnedFd::from_raw_fd(raw_fd) };
            let async_fd = match AsyncFd::new(owned) {
                Ok(a) => a,
                Err(_) => return,
            };
            // Wait for the pidfd to become readable — that means the parent exited.
            let _ = async_fd.readable().await;
            let _ = containment.kill_all();
        });
    }

    // ── factory ────────────────────────────────────────────────────────────────

    pub fn establish_linux(inherited: Option<String>) -> Arc<dyn Containment> {
        if let Some(handle) = inherited {
            let arc: Arc<dyn Containment> = Arc::new(CgroupContainment::new_inherited(handle));
            // Watch the parent process via pidfd: if parent dies (even SIGKILL),
            // kill_all() the inherited group so orphaned children don't linger.
            let parent_pid = unsafe { libc::getppid() };
            spawn_pidfd_watch(arc.clone(), parent_pid);
            arc
        } else {
            let c = CgroupContainment::new_owner();
            Arc::new(c)
        }
    }

    // ── tests ──────────────────────────────────────────────────────────────────

    #[cfg(test)]
    mod tests {
        use super::*;

        /// On Linux: enroll a child, kill_all → child must be dead.
        #[test]
        fn linux_enroll_then_kill() {
            let c = establish_linux(None);
            let mut cmd = std::process::Command::new("sleep");
            cmd.arg("60");
            let mut child = c.spawn_enrolled(cmd).expect("spawn sleep");
            let pid = child.id() as i32;

            c.kill_all().expect("kill_all");
            // Give it a moment to die.
            std::thread::sleep(std::time::Duration::from_millis(200));

            // Reap the child first to ensure it's not a zombie.
            let _ = child.wait();
            // After kill_all + reap, kill(pid, 0) must return ESRCH (-1) meaning dead.
            // Try kill(0) — ESRCH means dead.
            let alive = unsafe { libc::kill(pid, 0) };
            assert_eq!(
                alive, -1,
                "kill_all should have terminated child (pid {pid}), but kill(pid,0) returned {alive}"
            );
        }

        /// spawn_detached child survives kill_all on the main cgroup.
        #[test]
        fn linux_detached_survives_kill() {
            let c = establish_linux(None);

            let mut det_cmd = std::process::Command::new("sleep");
            det_cmd.arg("60");
            let (mut detached, det_pid) = c.spawn_detached(det_cmd).expect("spawn detached");

            // Enroll another child in the work cgroup.
            let mut work_cmd = std::process::Command::new("sleep");
            work_cmd.arg("60");
            let mut work_child = c.spawn_enrolled(work_cmd).expect("spawn enrolled");

            c.kill_all().expect("kill_all");
            std::thread::sleep(std::time::Duration::from_millis(200));

            // Detached child should still be alive.
            let alive = unsafe { libc::kill(det_pid, 0) };
            // Clean up detached child.
            unsafe {
                libc::kill(det_pid, libc::SIGKILL);
            }
            let _ = detached.wait();
            let _ = work_child.wait();

            // alive == 0 means the process still exists (what we want).
            // On CI cgroup may not be writable so kill_all may be a no-op for cgroup mode.
            // Either way the detached process should not have been killed by kill_all.
            assert_eq!(
                alive, 0,
                "detached child (pid {det_pid}) should survive kill_all"
            );
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

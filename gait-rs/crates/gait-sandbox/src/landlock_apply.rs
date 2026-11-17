//! Linux applier: probes the Landlock ABI, builds the ruleset from the
//! planner's rules and restricts the calling thread (and the children it
//! creates afterwards). Irreversible; never call it in a shared test process.

use crate::sandbox::{SandboxScope, SandboxStatus};

/// The kernel's Landlock ABI version, or the errno that probing failed with.
pub(crate) type AbiProbe = dyn Fn() -> Result<i32, i32>;

/// Probe the running kernel via `landlock_create_ruleset(NULL, 0, VERSION)`.
pub(crate) fn probe_abi() -> Result<i32, i32> {
    // SAFETY: a pure query; null attr and zero size with the VERSION flag.
    let rc = unsafe {
        libc::syscall(libc::SYS_landlock_create_ruleset, std::ptr::null::<libc::c_void>(), 0usize, 1u32)
    };
    if rc < 0 {
        Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(libc::ENOSYS))
    } else {
        Ok(rc as i32)
    }
}

/// Newest ABI this applier handles. Rights added after it (ioctl on devices,
/// pathname unix sockets) stay unhandled, so they are not restricted.
const MAX_ABI: i32 = 4;

fn errno_name(errno: i32) -> String {
    match errno {
        libc::ENOSYS => "ENOSYS".into(),
        libc::EOPNOTSUPP => "EOPNOTSUPP".into(),
        libc::EPERM => "EPERM".into(),
        other => format!("errno {other}"),
    }
}

fn unavailable(reason: String) -> anyhow::Result<SandboxStatus> {
    Ok(SandboxStatus::Unavailable { reason })
}

/// Apply `scope` to the calling thread using `probe` for the ABI.
///
/// Returns `Unavailable` (never an error, never a half-applied ruleset) when
/// the kernel lacks Landlock or the syscalls are blocked. Also confines every
/// child the calling thread creates afterwards; threads that already exist
/// stay unconfined, so callers seal before starting any other thread.
pub(crate) fn apply_with_probe(
    scope: &SandboxScope,
    probe: &AbiProbe,
) -> anyhow::Result<SandboxStatus> {
    use landlock::{
        Access, AccessFs, PathBeneath, PathFd, Ruleset, RulesetAttr, RulesetCreatedAttr, ABI,
    };

    let found = match probe() {
        Ok(v) => v,
        Err(errno) => {
            return unavailable(format!(
                "Landlock unavailable: landlock_create_ruleset failed with {} (kernel without Landlock, \
                 or the syscall is blocked)",
                errno_name(errno)
            ))
        }
    };
    if found < 3 {
        return unavailable(format!(
            "Landlock ABI {found} is too old (need 3 or newer, else truncate and refer rights would be silently dropped)"
        ));
    }

    let abi = ABI::from(found.min(MAX_ABI));
    // Execute stays unhandled: deny does not affect execute.
    let handled = AccessFs::from_all(abi) & !AccessFs::Execute;
    let read_rights = AccessFs::from_read(abi) & !AccessFs::Execute;

    let mut eff = scope.clone();
    if !eff.read_all {
        eff.readable
            .extend(crate::sandbox::platform_system_reads().iter().map(std::path::PathBuf::from));
    }
    eff.writable.push("/dev/null".into());
    eff.writable.push("/dev/tty".into());
    let plan = crate::landlock_plan::plan(&eff);

    let created = Ruleset::default().handle_access(handled).and_then(|r| r.create());
    let mut ruleset = match created {
        Ok(r) => r,
        Err(e) => return unavailable(format!("Landlock ruleset creation failed (ABI {found}): {e}")),
    };
    for rule in plan.read.iter().chain(plan.write.iter()) {
        let mut access = match rule.rights {
            crate::landlock_plan::Rights::Read => read_rights,
            crate::landlock_plan::Rights::ReadWrite => handled,
        };
        if !rule.is_dir {
            access &= AccessFs::from_file(abi);
        }
        // A path that vanished since planning is simply not granted.
        let Ok(fd) = PathFd::new(&rule.path) else { continue };
        ruleset = match ruleset.add_rule(PathBeneath::new(fd, access)) {
            Ok(r) => r,
            Err(e) => {
                return unavailable(format!(
                    "Landlock rule for {} failed: {e}",
                    rule.path.display()
                ))
            }
        };
    }

    // SAFETY: plain prctl with constant arguments.
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
        let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
        return unavailable(format!("Landlock not applied: no_new_privs failed with {}", errno_name(errno)));
    }
    match ruleset.restrict_self() {
        Ok(status) if status.ruleset == landlock::RulesetStatus::NotEnforced => {
            unavailable(format!("Landlock not enforced by the kernel (ABI {found})"))
        }
        Ok(_) => Ok(SandboxStatus::Active),
        Err(e) => unavailable(format!("Landlock restrict_self failed (ABI {found}): {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::process::Command;

    fn reason(s: SandboxStatus) -> String {
        match s {
            SandboxStatus::Unavailable { reason } => reason,
            other => panic!("expected Unavailable, got {other:?}"),
        }
    }

    #[test]
    fn unavailable_reason_names_the_errno() {
        let scope = SandboxScope::default();
        for (errno, name) in [
            (libc::ENOSYS, "ENOSYS"),
            (libc::EOPNOTSUPP, "EOPNOTSUPP"),
            (libc::EPERM, "EPERM"),
        ] {
            let r = reason(apply_with_probe(&scope, &move || Err(errno)).unwrap());
            assert!(r.contains(name), "reason {r:?} must name {name}");
        }
        let r = reason(apply_with_probe(&scope, &|| Err(99)).unwrap());
        assert!(r.contains("99"), "unknown errno is still named: {r:?}");
    }

    #[test]
    fn an_abi_below_three_is_unavailable_and_named() {
        let scope = SandboxScope::default();
        for abi in [0, 1, 2] {
            let r = reason(apply_with_probe(&scope, &move || Ok(abi)).unwrap());
            assert!(r.contains(&format!("ABI {abi}")), "reason {r:?} must name the ABI");
        }
    }

    /// Run this test binary again, running only `child_entry` in `mode`.
    fn run_child(mode: &str, tmp: &std::path::Path) -> (bool, String) {
        let out = Command::new(std::env::current_exe().unwrap())
            .args(["landlock_apply::tests::child_entry", "--exact", "--nocapture", "--test-threads=1"])
            .env("GAIT_LL_CHILD", mode)
            .env("GAIT_LL_TMP", tmp)
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout).into_owned()
            + &String::from_utf8_lossy(&out.stderr);
        (out.status.success(), text)
    }

    fn kernel_supports() -> bool {
        match probe_abi() {
            Ok(abi) if abi >= 3 => true,
            other => {
                eprintln!("skipped: no usable Landlock here ({other:?})");
                false
            }
        }
    }

    /// Entry point of the re-executed child. A no-op in a normal run.
    #[test]
    fn child_entry() {
        let Ok(mode) = std::env::var("GAIT_LL_CHILD") else { return };
        let tmp = PathBuf::from(std::env::var("GAIT_LL_TMP").unwrap());
        let inside = tmp.join("in");
        let scope = match mode.as_str() {
            "apply" => SandboxScope {
                read_all: true,
                writable: vec![inside.clone()],
                ..Default::default()
            },
            _ => SandboxScope {
                readable: vec![inside.clone()],
                writable: vec![inside.clone()],
                ..Default::default()
            },
        };
        let status = apply_with_probe(&scope, &probe_abi).unwrap();
        println!("LL-STATUS:{status:?}");
        println!("LL-INSIDE-WRITE:{}", std::fs::write(inside.join("w"), b"x").is_ok());
        println!("LL-OUTSIDE-WRITE:{}", std::fs::write(tmp.join("w"), b"x").is_ok());
        println!("LL-INSIDE-READ:{}", std::fs::read(inside.join("f")).is_ok());
        println!("LL-OUTSIDE-READ:{}", std::fs::read(tmp.join("outside.txt")).is_ok());
        let sh = Command::new("/bin/sh")
            .args(["-c", "echo hi > /dev/null; echo shell-ok"])
            .output();
        println!(
            "LL-SHELL:{}",
            sh.map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .unwrap_or_else(|e| format!("failed {e}"))
        );
    }

    fn fixture() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("in")).unwrap();
        std::fs::write(tmp.path().join("in/f"), b"x").unwrap();
        std::fs::write(tmp.path().join("outside.txt"), b"x").unwrap();
        tmp
    }

    #[test]
    fn apply_in_a_child_process_returns_active_on_a_landlock_kernel() {
        if !kernel_supports() {
            return;
        }
        let tmp = fixture();
        let (ok, out) = run_child("apply", tmp.path());
        assert!(ok, "child failed: {out}");
        assert!(out.contains("LL-STATUS:Active"), "{out}");
        assert!(out.contains("LL-INSIDE-WRITE:true"), "{out}");
        assert!(out.contains("LL-OUTSIDE-WRITE:false"), "{out}");
        assert!(out.contains("LL-OUTSIDE-READ:true"), "read_all keeps reads open: {out}");
    }

    #[test]
    fn confined_reads_keep_a_shell_working_and_deny_the_rest() {
        if !kernel_supports() {
            return;
        }
        let tmp = fixture();
        let (ok, out) = run_child("confined", tmp.path());
        assert!(ok, "child failed: {out}");
        assert!(out.contains("LL-STATUS:Active"), "{out}");
        assert!(out.contains("LL-INSIDE-READ:true"), "{out}");
        assert!(out.contains("LL-OUTSIDE-READ:false"), "{out}");
        assert!(out.contains("LL-OUTSIDE-WRITE:false"), "{out}");
        assert!(out.contains("LL-SHELL:shell-ok"), "shell must load libc and write /dev/null: {out}");
    }
}

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
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<libc::c_void>(),
            0usize,
            1u32,
        )
    };
    if rc < 0 {
        Err(std::io::Error::last_os_error()
            .raw_os_error()
            .unwrap_or(libc::ENOSYS))
    } else {
        Ok(rc as i32)
    }
}

/// Newest ABI this applier handles. Rights added after it (ioctl on devices,
/// pathname unix sockets) stay unhandled, so they are not restricted.
const MAX_ABI: i32 = 4;

/// An errno shown by name when it is one of the expected ones.
#[derive(Debug, Clone, Copy)]
struct Errno(i32);

impl std::fmt::Display for Errno {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0 {
            libc::ENOSYS => f.write_str("ENOSYS"),
            libc::EOPNOTSUPP => f.write_str("EOPNOTSUPP"),
            libc::EPERM => f.write_str("EPERM"),
            other => write!(f, "errno {other}"),
        }
    }
}

/// Why the seal could not be applied. Its `Display` is the `Unavailable` reason.
#[derive(Debug, thiserror::Error)]
enum SealError {
    #[error(
        "Landlock unavailable: landlock_create_ruleset failed with {0} (kernel without Landlock, \
         or the syscall is blocked)"
    )]
    Probe(Errno),
    #[error(
        "Landlock ABI {0} is too old (need 3 or newer, else truncate and refer rights would be silently dropped)"
    )]
    AbiTooOld(i32),
    #[error("Landlock ruleset creation failed (ABI {abi}): {source}")]
    Create {
        abi: i32,
        source: landlock::RulesetError,
    },
    #[error("Landlock rule for {} failed: {source}", path.display())]
    PathRule {
        path: std::path::PathBuf,
        source: landlock::RulesetError,
    },
    #[error("Landlock network rule for port {port} failed: {source}")]
    NetRule {
        port: u16,
        source: landlock::RulesetError,
    },
    #[error("Landlock not applied: no_new_privs failed with {0}")]
    NoNewPrivs(Errno),
    #[error("Landlock not enforced by the kernel (ABI {0})")]
    NotEnforced(i32),
    #[error("Landlock restrict_self failed (ABI {abi}): {source}")]
    Restrict {
        abi: i32,
        source: landlock::RulesetError,
    },
}

/// Limits of the applied enforcement, for the status reason. Pure.
pub(crate) fn status_notes(
    network: &crate::sandbox::NetworkPolicy,
    net: &crate::landlock_plan::NetPlan,
    found: i32,
) -> Vec<String> {
    use crate::sandbox::NetworkPolicy;
    let mut notes = Vec::new();
    let wants_net = net.handle_connect || net.handle_bind;
    if found < 4 {
        if wants_net {
            notes.push(format!(
                "network not enforced (Landlock ABI {found} lacks TCP rules; needs 4)"
            ));
        }
    } else {
        if let NetworkPolicy::Hosts(_) = network {
            let ports: Vec<String> = net.connect_ports.iter().map(u16::to_string).collect();
            notes.push(format!(
                "network Hosts: host filtering is not enforced, TCP connect allowed to ports {} on any host",
                ports.join(", ")
            ));
        }
        if net.handle_connect {
            notes.push("UDP (including DNS) is not covered by Landlock".into());
        }
    }
    if found > MAX_ABI {
        notes.push(format!(
            "Landlock ABI {found} is newer than the handled ABI {MAX_ABI}: rights added after it are not restricted"
        ));
    }
    notes
}

fn unavailable(why: SealError) -> anyhow::Result<SandboxStatus> {
    Ok(SandboxStatus::Unavailable {
        reason: why.to_string(),
    })
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
        Access, AccessFs, AccessNet, NetPort, PathBeneath, PathFd, Ruleset, RulesetAttr,
        RulesetCreatedAttr, ABI,
    };

    let found = match probe() {
        Ok(v) => v,
        Err(errno) => return unavailable(SealError::Probe(Errno(errno))),
    };
    if found < 3 {
        return unavailable(SealError::AbiTooOld(found));
    }

    let abi = ABI::from(found.min(MAX_ABI));
    // Execute stays unhandled: deny does not affect execute.
    let handled = AccessFs::from_all(abi) & !AccessFs::Execute;
    let read_rights = AccessFs::from_read(abi) & !AccessFs::Execute;

    let mut eff = scope.clone();
    if !eff.read_all {
        eff.readable.extend(
            crate::sandbox::platform_system_reads()
                .iter()
                .map(std::path::PathBuf::from),
        );
    }
    eff.writable.push("/dev/null".into());
    eff.writable.push("/dev/tty".into());
    let plan = crate::landlock_plan::plan(&eff);

    let mut net_handled = landlock::BitFlags::<AccessNet>::empty();
    if found >= 4 {
        if plan.net.handle_connect {
            net_handled |= AccessNet::ConnectTcp;
        }
        if plan.net.handle_bind {
            net_handled |= AccessNet::BindTcp;
        }
    }
    let created = Ruleset::default()
        .handle_access(handled)
        .and_then(|r| r.handle_access(net_handled))
        .and_then(|r| r.create());
    let mut ruleset = match created {
        Ok(r) => r,
        Err(source) => return unavailable(SealError::Create { abi: found, source }),
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
        let Ok(fd) = PathFd::new(&rule.path) else {
            continue;
        };
        ruleset = match ruleset.add_rule(PathBeneath::new(fd, access)) {
            Ok(r) => r,
            Err(source) => {
                return unavailable(SealError::PathRule {
                    path: rule.path.clone(),
                    source,
                })
            }
        };
    }

    if net_handled.contains(AccessNet::ConnectTcp) {
        for port in &plan.net.connect_ports {
            ruleset = match ruleset.add_rule(NetPort::new(*port, AccessNet::ConnectTcp)) {
                Ok(r) => r,
                Err(source) => {
                    return unavailable(SealError::NetRule {
                        port: *port,
                        source,
                    })
                }
            };
        }
    }

    // SAFETY: plain prctl with constant arguments.
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
        let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
        return unavailable(SealError::NoNewPrivs(Errno(errno)));
    }
    // From here no_new_privs stays set even if restrict_self fails: harmless (it
    // only stops later privilege gain) and the status is still `Unavailable`.
    match ruleset.restrict_self() {
        Ok(status) if status.ruleset == landlock::RulesetStatus::NotEnforced => {
            unavailable(SealError::NotEnforced(found))
        }
        Ok(_) => {
            let notes = status_notes(&scope.network, &plan.net, found);
            Ok(if notes.is_empty() {
                SandboxStatus::Active
            } else {
                SandboxStatus::ActiveWithNotes { notes }
            })
        }
        Err(source) => unavailable(SealError::Restrict { abi: found, source }),
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

    fn net(c: bool, b: bool, ports: &[u16]) -> crate::landlock_plan::NetPlan {
        crate::landlock_plan::NetPlan {
            handle_connect: c,
            handle_bind: b,
            connect_ports: ports.to_vec(),
        }
    }

    fn joined(v: Vec<String>) -> String {
        v.join(" | ")
    }

    #[test]
    fn below_abi_four_says_network_not_enforced() {
        use crate::sandbox::NetworkPolicy::*;
        let n = joined(status_notes(&Deny, &net(true, true, &[]), 3));
        assert!(n.contains("network not enforced"), "{n}");
        // Nothing to enforce under All with local_bind: no network note.
        let n = joined(status_notes(&All, &net(false, false, &[]), 3));
        assert!(!n.contains("network not enforced"), "{n}");
    }

    #[test]
    fn hosts_says_host_filtering_and_udp_are_not_enforced() {
        let n = joined(status_notes(
            &crate::sandbox::NetworkPolicy::Hosts(vec!["example.com".into()]),
            &net(true, true, &[80, 443]),
            4,
        ));
        assert!(n.contains("host filtering is not enforced"), "{n}");
        assert!(n.contains("UDP"), "{n}");
        assert!(n.contains("80") && n.contains("443"), "{n}");
    }

    #[test]
    fn deny_mentions_udp_and_a_fully_covered_scope_has_no_notes() {
        use crate::sandbox::NetworkPolicy::*;
        let n = joined(status_notes(&Deny, &net(true, true, &[]), 4));
        assert!(n.contains("UDP"), "{n}");
        assert!(!n.contains("network not enforced"), "{n}");
        assert!(status_notes(&All, &net(false, false, &[]), 4).is_empty());
    }

    #[test]
    fn abi_newer_than_handled_names_the_unhandled_rights() {
        use crate::sandbox::NetworkPolicy::*;
        let n = joined(status_notes(&All, &net(false, false, &[]), MAX_ABI + 2));
        assert!(n.contains(&format!("ABI {}", MAX_ABI + 2)), "{n}");
        assert!(n.contains("not restricted"), "{n}");
    }

    #[test]
    fn an_abi_below_three_is_unavailable_and_named() {
        let scope = SandboxScope::default();
        for abi in [0, 1, 2] {
            let r = reason(apply_with_probe(&scope, &move || Ok(abi)).unwrap());
            assert!(
                r.contains(&format!("ABI {abi}")),
                "reason {r:?} must name the ABI"
            );
        }
    }

    /// Run this test binary again, running only `child_entry` in `mode`.
    fn run_child(mode: &str, tmp: &std::path::Path) -> (bool, String) {
        let out = Command::new(std::env::current_exe().unwrap())
            .args([
                "landlock_apply::tests::child_entry",
                "--exact",
                "--nocapture",
                "--test-threads=1",
            ])
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
        let Ok(mode) = std::env::var("GAIT_LL_CHILD") else {
            return;
        };
        let tmp = PathBuf::from(std::env::var("GAIT_LL_TMP").unwrap());
        let inside = tmp.join("in");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let scope = match mode.as_str() {
            "netdeny" | "netall" => SandboxScope {
                read_all: true,
                network: if mode == "netall" {
                    crate::sandbox::NetworkPolicy::All
                } else {
                    crate::sandbox::NetworkPolicy::Deny
                },
                local_bind: true,
                ..Default::default()
            },
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
        match std::net::TcpStream::connect(addr) {
            Ok(_) => println!("LL-CONNECT:ok"),
            Err(e) => println!("LL-CONNECT:err {:?}", e.raw_os_error()),
        }
        drop(listener);
        println!(
            "LL-INSIDE-WRITE:{}",
            std::fs::write(inside.join("w"), b"x").is_ok()
        );
        println!(
            "LL-OUTSIDE-WRITE:{}",
            std::fs::write(tmp.join("w"), b"x").is_ok()
        );
        println!("LL-INSIDE-READ:{}", std::fs::read(inside.join("f")).is_ok());
        println!(
            "LL-OUTSIDE-READ:{}",
            std::fs::read(tmp.join("outside.txt")).is_ok()
        );
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
        #[cfg(target_os = "linux")]
        let tmp = tempfile::Builder::new().tempdir_in("/tmp").unwrap();
        #[cfg(not(target_os = "linux"))]
        let tmp = tempfile::tempdir().unwrap();
        for root in crate::sandbox::platform_system_reads() {
            assert!(
                !tmp.path().starts_with(root),
                "fixture dir {:?} must live outside every system-read root ({:?})",
                tmp.path(),
                root,
            );
        }
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
        assert!(
            out.contains("LL-OUTSIDE-READ:true"),
            "read_all keeps reads open: {out}"
        );
    }

    #[test]
    fn network_deny_blocks_tcp_connect_and_all_allows_it() {
        let abi = match probe_abi() {
            Ok(a) if a >= 4 => a,
            other => {
                eprintln!("skipped: network rules need Landlock ABI 4 ({other:?})");
                return;
            }
        };
        let _ = abi;
        let tmp = fixture();
        let (ok, out) = run_child("netdeny", tmp.path());
        assert!(ok, "child failed: {out}");
        assert!(
            out.contains(&format!("LL-CONNECT:err Some({})", libc::EACCES)),
            "{out}"
        );
        let (ok, out) = run_child("netall", tmp.path());
        assert!(ok, "child failed: {out}");
        assert!(
            out.contains("LL-CONNECT:ok"),
            "control (network All) must connect: {out}"
        );
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
        assert!(
            out.contains("LL-SHELL:shell-ok"),
            "shell must load libc and write /dev/null: {out}"
        );
    }
}

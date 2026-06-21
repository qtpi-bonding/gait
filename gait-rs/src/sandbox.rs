#![allow(dead_code)] // planned feature: kernel write-enforcement sandbox; wired up in a future change

use std::path::PathBuf;

/// Result of applying the sandbox to this process.
#[derive(Debug, Clone)]
pub enum SandboxStatus {
    /// Kernel enforcement active — writes outside `scope.writable` will be rejected at the
    /// syscall level (Landlock on Linux, Seatbelt on macOS).
    Active,
    /// Kernel enforcement unavailable on this platform / kernel version. Tier-0 scope gate
    /// in turn.rs still applies; a `SandboxApplied` transcript event is emitted with the reason.
    Unavailable { reason: String },
    /// No scope declared — noop sandbox, no restrictions applied.
    Noop,
}

/// Network access policy for the sandboxed process.
#[derive(Debug, Clone, Default)]
pub enum NetworkPolicy {
    /// No network access (default — most restrictive).
    #[default]
    Deny,
    /// Allow network access to the listed hosts (port 443 typically; TBD).
    Hosts(Vec<String>),
    /// Allow all network access (least restrictive).
    All,
}

/// Scope of filesystem and network access for the sandboxed process.
#[derive(Debug, Clone, Default)]
pub struct SandboxScope {
    /// Paths the process may read. Empty means "no read restriction" only when `read_all` is set.
    pub readable: Vec<PathBuf>,
    /// Paths the process may write. Writes outside this set are rejected at the syscall level.
    pub writable: Vec<PathBuf>,
    /// `true` when config readable contains `"*"` — disables read restrictions entirely.
    pub read_all: bool,
    pub network: NetworkPolicy,
}

/// Kernel write-enforcement sandbox. Injected at process start (once, before the turn loop).
/// Implemented per-platform; [`NoopSandbox`] is the fallback for tests / Windows.
pub trait Sandbox: Send + Sync {
    /// Apply enforcement before the turn loop. Called once at process start.
    fn apply(&self, scope: &SandboxScope) -> anyhow::Result<SandboxStatus>;
}

// ── NoopSandbox ──────────────────────────────────────────────────────────────

/// No-op sandbox — always returns [`SandboxStatus::Noop`].
/// Used on Windows, in tests, and when no scope is declared.
pub struct NoopSandbox;

impl Sandbox for NoopSandbox {
    fn apply(&self, _scope: &SandboxScope) -> anyhow::Result<SandboxStatus> {
        Ok(SandboxStatus::Noop)
    }
}

// ── LandlockSandbox ──────────────────────────────────────────────────────────

/// Linux Landlock sandbox (kernel 5.13+, ABI 2+ recommended for cross-directory moves).
/// Write-only governed: reads are ungoverned so read tools are never blocked.
/// Stub — not yet implemented; returns [`SandboxStatus::Unavailable`].
pub struct LandlockSandbox;

impl Sandbox for LandlockSandbox {
    fn apply(&self, _scope: &SandboxScope) -> anyhow::Result<SandboxStatus> {
        Ok(SandboxStatus::Unavailable {
            reason: "Landlock not yet implemented".to_string(),
        })
    }
}

// ── SeatbeltSandbox ──────────────────────────────────────────────────────────

/// macOS Seatbelt sandbox — in-process self-seal via sandbox_compile_string/sandbox_apply.
/// Deprecated since macOS Sierra but still used by Chrome/Firefox; revisit if Apple removes it.
pub struct SeatbeltSandbox;

#[cfg(target_os = "macos")]
#[link(name = "sandbox")]
extern "C" {
    fn sandbox_compile_string(
        profile: *const libc::c_char,
        params: *const libc::c_void,
        err: *mut *mut libc::c_char,
    ) -> *mut libc::c_void;
    fn sandbox_apply(profile: *mut libc::c_void) -> libc::c_int;
    fn sandbox_free_profile(profile: *mut libc::c_void);
    fn sandbox_free_error(err: *mut libc::c_char);
}

impl Sandbox for SeatbeltSandbox {
    fn apply(&self, scope: &SandboxScope) -> anyhow::Result<SandboxStatus> {
        #[cfg(target_os = "macos")]
        {
            let profile = crate::sandbox::sbpl::build_sbpl(scope);
            let c = std::ffi::CString::new(profile)
                .map_err(|e| anyhow::anyhow!("SBPL contained null byte: {e}"))?;
            let mut err: *mut libc::c_char = std::ptr::null_mut();
            let handle = unsafe { sandbox_compile_string(c.as_ptr(), std::ptr::null(), &mut err) };
            if handle.is_null() {
                let reason = if err.is_null() {
                    "sandbox_compile_string returned null (no error string)".to_owned()
                } else {
                    let s = unsafe { std::ffi::CStr::from_ptr(err).to_string_lossy().into_owned() };
                    unsafe { sandbox_free_error(err) };
                    s
                };
                return Ok(SandboxStatus::Unavailable { reason });
            }
            let rc = unsafe { sandbox_apply(handle) };
            unsafe { sandbox_free_profile(handle) };
            if rc == 0 {
                Ok(SandboxStatus::Active)
            } else {
                Ok(SandboxStatus::Unavailable {
                    reason: format!("sandbox_apply returned {rc}"),
                })
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = scope;
            Ok(SandboxStatus::Unavailable {
                reason: "Seatbelt is macOS-only".into(),
            })
        }
    }
}

// ── platform default ─────────────────────────────────────────────────────────

/// Return the appropriate sandbox for this platform.
pub fn platform_sandbox() -> Box<dyn Sandbox> {
    if cfg!(target_os = "linux") {
        Box::new(LandlockSandbox)
    } else if cfg!(target_os = "macos") {
        Box::new(SeatbeltSandbox)
    } else {
        Box::new(NoopSandbox)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn noop_returns_noop() {
        let scope = SandboxScope {
            writable: vec![PathBuf::from("src/")],
            ..Default::default()
        };
        let s = NoopSandbox.apply(&scope).unwrap();
        assert!(matches!(s, SandboxStatus::Noop));
    }

    #[test]
    fn landlock_stub_returns_unavailable() {
        let scope = SandboxScope {
            writable: vec![PathBuf::from("src/")],
            ..Default::default()
        };
        let s = LandlockSandbox.apply(&scope).unwrap();
        assert!(matches!(s, SandboxStatus::Unavailable { .. }));
    }

    // Seatbelt.apply() irreversibly seals the calling process on macOS; calling
    // it in a unit test poisons all subsequent tests in the same binary. The
    // enforcement is exercised by the ignored integration test in
    // tests/seatbelt_seal.rs. Only run the in-process call on non-macOS
    // where the Landlock stub safely returns Unavailable without side effects.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn seatbelt_returns_unavailable_on_non_macos() {
        let scope = SandboxScope {
            writable: vec![PathBuf::from("src/")],
            ..Default::default()
        };
        let s = SeatbeltSandbox.apply(&scope).unwrap();
        assert!(matches!(s, SandboxStatus::Unavailable { .. }));
    }

    #[test]
    fn platform_sandbox_is_box_dyn_sandbox() {
        // Just verifies the constructor compiles and returns the right trait object.
        // Calling .apply() here on macOS would irreversibly seal the test process;
        // enforcement is tested in tests/seatbelt_seal.rs (ignored by default).
        let _sb: Box<dyn Sandbox> = platform_sandbox();
    }

    #[test]
    fn noop_sandbox_with_default_scope_returns_noop() {
        let s = NoopSandbox.apply(&SandboxScope::default()).unwrap();
        assert!(matches!(s, SandboxStatus::Noop));
    }

    #[test]
    fn sbpl_denies_by_default_and_allows_writable_subpaths() {
        let scope = SandboxScope {
            writable: vec![PathBuf::from("/repo"), PathBuf::from("/tmp/gait-hand-abc")],
            readable: vec![],
            read_all: true,
            network: NetworkPolicy::Deny,
        };
        let p = build_sbpl(&scope);
        assert!(p.contains("(deny default)"));
        assert!(
            p.contains("(allow file-read*)"),
            "read_all=true must emit blanket read"
        );
        assert!(
            p.contains("(subpath \"/repo\")"),
            "writable root must appear"
        );
        assert!(p.contains("(subpath \"/tmp/gait-hand-abc\")"));
        assert!(
            !p.contains("network-outbound"),
            "deny network must not emit network rules"
        );
    }

    #[test]
    fn sbpl_allows_network_with_dns_stanza() {
        let scope = SandboxScope {
            writable: vec![PathBuf::from("/repo")],
            readable: vec![],
            read_all: true,
            network: NetworkPolicy::All,
        };
        let p = build_sbpl(&scope);
        assert!(p.contains("(allow network-outbound)"));
        assert!(
            p.contains("(allow system-socket)"),
            "DNS requires system-socket"
        );
        assert!(
            p.contains("mDNSResponder"),
            "DNS requires mach-lookup mDNSResponder"
        );
    }

    #[test]
    fn sbpl_hosts_allowlist_emits_network_rules() {
        // Hosts allowlist: kernel-level = allow-all egress (app-level host check is separate)
        let scope = SandboxScope {
            writable: vec![],
            readable: vec![],
            read_all: true,
            network: NetworkPolicy::Hosts(vec!["github.com".to_owned()]),
        };
        let p = build_sbpl(&scope);
        assert!(
            p.contains("(allow network-outbound)"),
            "Hosts mode must allow outbound"
        );
        assert!(p.contains("(allow system-socket)"));
        assert!(
            p.contains("mDNSResponder"),
            "Hosts mode must emit DNS stanza for mDNSResponder"
        );
    }

    #[test]
    fn confined_reads_include_platform_defaults_and_scope() {
        let scope = SandboxScope {
            read_all: false,
            readable: vec![PathBuf::from("/repo")],
            writable: vec![PathBuf::from("/repo")],
            network: NetworkPolicy::Deny,
        };
        let p = build_sbpl(&scope);
        assert!(
            !p.contains("(allow file-read*)\n"),
            "must NOT blanket-allow reads"
        );
        assert!(p.contains("(subpath \"/usr/lib\")"));
        assert!(p.contains("(subpath \"/private/var/db/dyld\")"));
        assert!(p.contains("(subpath \"/repo\")")); // scope readable
    }

    #[test]
    fn sbpl_escapes_quotes_and_backslashes_in_paths() {
        // Paths with " or \ are legal on macOS and must not break the SBPL profile.
        let scope = SandboxScope {
            writable: vec![PathBuf::from("/repo/a\"b"), PathBuf::from("/repo/c\\d")],
            readable: vec![],
            read_all: true,
            network: NetworkPolicy::Deny,
        };
        let p = build_sbpl(&scope);
        assert!(
            p.contains(r#"(subpath "/repo/a\"b")"#),
            "double-quote in path must be escaped: {p}"
        );
        assert!(
            p.contains(r#"(subpath "/repo/c\\d")"#),
            "backslash in path must be escaped: {p}"
        );
    }

    #[test]
    fn read_only_tool_gets_no_writable() {
        use crate::config::SandboxConfig;
        use crate::types::ToolAccess;
        use std::path::Path;
        let cfg = SandboxConfig::default();
        let scope = super::ceiling::compute_scope(
            ToolAccess {
                reads: true,
                ..Default::default()
            },
            &cfg,
            Path::new("/repo"),
            Path::new("/tmp/jail"),
        );
        assert!(
            scope.writable.is_empty(),
            "read-only tool must not be granted writes"
        );
        assert!(scope.readable.iter().any(|p| p == Path::new("/repo")));
    }

    #[test]
    fn write_tool_gets_codebase_and_jail_writable() {
        use crate::config::SandboxConfig;
        use crate::types::ToolAccess;
        use std::path::{Path, PathBuf};
        let cfg = SandboxConfig::default();
        let scope = super::ceiling::compute_scope(
            ToolAccess {
                reads: true,
                writes: true,
                ..Default::default()
            },
            &cfg,
            Path::new("/repo"),
            Path::new("/tmp/jail"),
        );
        assert!(scope.writable.contains(&PathBuf::from("/repo")));
        assert!(scope.writable.contains(&PathBuf::from("/tmp/jail")));
    }

    #[test]
    fn network_tool_denied_when_config_off() {
        use crate::config::SandboxConfig;
        use crate::types::ToolAccess;
        use std::path::Path;
        let cfg = SandboxConfig::default(); // network: Off
        let scope = super::ceiling::compute_scope(
            ToolAccess {
                network: true,
                ..Default::default()
            },
            &cfg,
            Path::new("/repo"),
            Path::new("/tmp/jail"),
        );
        assert!(matches!(scope.network, NetworkPolicy::Deny));
    }

    #[test]
    fn strict_config_confines_reads_by_default() {
        use crate::config::SandboxConfig;
        use crate::types::ToolAccess;
        use std::path::{Path, PathBuf};
        let cfg = SandboxConfig::default(); // readable: []
        let scope = super::ceiling::compute_scope(
            ToolAccess {
                reads: true,
                ..Default::default()
            },
            &cfg,
            Path::new("/repo"),
            Path::new("/tmp/jail"),
        );
        assert!(!scope.read_all, "default must confine reads");
        assert!(scope.readable.contains(&PathBuf::from("/repo")));
    }

    #[test]
    fn star_readable_opens_reads() {
        use crate::config::SandboxConfig;
        use crate::types::ToolAccess;
        use std::path::Path;
        let mut cfg = SandboxConfig::default();
        cfg.readable = vec!["*".into()];
        let scope = super::ceiling::compute_scope(
            ToolAccess {
                reads: true,
                ..Default::default()
            },
            &cfg,
            Path::new("/repo"),
            Path::new("/tmp/jail"),
        );
        assert!(scope.read_all);
    }
}

pub(crate) mod sbpl {
    //!
    //! Profile structure modeled after Chromium's sandbox policies.
    //!
    //! Note (correction #6): path canonicalization happens in the brain (scope
    //! construction), not here. We emit paths verbatim.

    use super::{NetworkPolicy, SandboxScope};
    use std::fmt::Write;

    /// Minimum set of read-only paths a sealed macOS process needs to exec,
    /// resolve DNS, and do TLS. These are emitted as `(allow file-read* (subpath …))`
    /// when `scope.read_all` is false (confined-read mode, Phase 2d+).
    #[cfg(target_os = "macos")]
    fn platform_read_defaults() -> &'static [&'static str] {
        &[
            "/usr/lib",
            "/usr/share",
            "/System",
            "/Library/Frameworks",
            "/private/var/db/dyld", // dyld shared cache
            "/dev/null",
            "/dev/random",
            "/dev/urandom",
            "/dev/dtracehelper",
            "/etc", // resolv.conf, hosts, ssl (symlink → /private/etc)
            "/private/etc",
            "/private/var/db/timezone",
            "/usr/bin",
            "/bin",
            "/usr/sbin",
            "/sbin",
        ]
    }

    /// On non-macOS platforms SBPL is never applied, so no platform defaults are needed.
    /// The `else` branch in `build_sbpl` still compiles correctly, yielding an empty list.
    #[cfg(not(target_os = "macos"))]
    fn platform_read_defaults() -> &'static [&'static str] {
        &[]
    }

    /// Escape a path string for embedding in an SBPL double-quoted literal.
    /// Escapes backslash and double-quote (the only SBPL-significant chars in a path).
    fn sbpl_escape(path: &std::path::Path) -> String {
        path.to_string_lossy()
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
    }

    /// Build a macOS Seatbelt SBPL profile string from a [`SandboxScope`].
    ///
    /// Pure: no I/O, no FFI. Same input → same output on every platform.
    pub fn build_sbpl(scope: &SandboxScope) -> String {
        let mut s = String::new();
        writeln!(s, "(version 1)").unwrap();
        writeln!(s, "(deny default)").unwrap();

        // exec + fork so the shell can run subprocesses (they inherit this seal).
        writeln!(s, "(allow process-exec)").unwrap();
        writeln!(s, "(allow process-fork)").unwrap();
        writeln!(s, "(allow sysctl-read)").unwrap();

        // Reads. Phase 2b blanket-allows reads when read_all=true.
        // Phase 2d (confined-read): when read_all=false, emit per-path subpath
        // rules for platform defaults + scope.readable instead of a blanket allow.
        if scope.read_all {
            writeln!(s, "(allow file-read*)").unwrap();
        } else {
            // Platform defaults: paths every sealed macOS process must read to exec,
            // resolve DNS, and do TLS. Empty on non-macOS (SBPL never applied there).
            for root in platform_read_defaults() {
                writeln!(s, r#"(allow file-read* (subpath "{root}"))"#).unwrap();
            }
            // Explicit scope paths (canonicalized by caller or best-effort here).
            for p in &scope.readable {
                let c = std::fs::canonicalize(p).unwrap_or_else(|_| p.clone());
                writeln!(s, r#"(allow file-read* (subpath "{}"))"#, sbpl_escape(&c)).unwrap();
            }
            // Broad stat/readdir — needed by many tools even when read content is restricted.
            writeln!(s, "(allow file-read-metadata)").unwrap();
        }

        // Writes (and reads under writable, so tools can read what they write).
        for p in &scope.writable {
            writeln!(
                s,
                r#"(allow file-write* file-read* (subpath "{}"))"#,
                sbpl_escape(p)
            )
            .unwrap();
        }

        // /dev/null always writable for output redirection.
        writeln!(s, r#"(allow file-write-data (literal "/dev/null"))"#).unwrap();

        // Network. DNS stanza required for hostname resolution (correction #3):
        // omitting system-socket/mach-lookup mDNSResponder makes DNS fail
        // (connect times out). Validated on Darwin 24.5.0.
        match &scope.network {
            NetworkPolicy::Deny => {}
            NetworkPolicy::All => {
                writeln!(s, "(allow network-outbound)").unwrap();
                writeln!(s, "(allow network-inbound)").unwrap();
                writeln!(s, "(allow system-socket)").unwrap();
                writeln!(
                    s,
                    r#"(allow mach-lookup (global-name "com.apple.mDNSResponder"))"#
                )
                .unwrap();
            }
            NetworkPolicy::Hosts(_hosts) => {
                // SBPL host-granular filtering is limited. v1 = allow all egress
                // when an allowlist is present; rely on app-level webfetch host
                // check (spec §4). Documented.
                writeln!(s, "(allow network-outbound)").unwrap();
                writeln!(s, "(allow system-socket)").unwrap();
                writeln!(
                    s,
                    r#"(allow mach-lookup (global-name "com.apple.mDNSResponder"))"#
                )
                .unwrap();
            }
        }

        s
    }
}

#[cfg(test)]
#[allow(unused_imports)]
pub(crate) use sbpl::build_sbpl;

pub mod ceiling {
    //!
    //! Pure ceiling computation for one tool call's sandbox scope.
    //!
    //! Inputs: the tool's `ToolAccess` (what it needs), the user's `SandboxConfig`
    //! (what's globally permitted), the codebase root, and the per-runner temp jail.
    //! Output: a `SandboxScope` that is *bounded* by both — the config can only widen,
    //! the access flags can only narrow. No I/O, no FFI: same inputs → same output.

    use super::{NetworkPolicy, SandboxScope};
    use crate::config::{NetworkConfig, SandboxConfig};
    use crate::types::ToolAccess;
    use std::path::{Path, PathBuf};

    /// Compute the per-call sandbox scope ceiling.
    ///
    /// Base: `{codebase, temp_jail}` for reads + writes the tool needs.
    /// Extended with: config `readable` (always) and config `writable` (when writes
    /// are permitted by the tool). `~/` is tilde-expanded against `dirs_next::home_dir()`.
    /// Network: config `Off` always denies; `Hosts`/`All` honored only when
    /// `access.network` is true. `read_all` defaults to true in Phase 2b; Phase 2d
    /// will flip it to false and emit per-path subpath rules instead.
    pub fn compute_scope(
        access: ToolAccess,
        cfg: &SandboxConfig,
        codebase: &Path,
        temp_jail: &Path,
    ) -> SandboxScope {
        // Canonicalize codebase so the SBPL subpath matches what the kernel sees
        // (handles /tmp → /private/tmp on macOS and symlinks). temp_jail is already
        // canonicalized by the caller (synapse.rs) before it reaches here.
        let codebase = codebase
            .canonicalize()
            .unwrap_or_else(|_| codebase.to_path_buf());

        // Base readable + writable: codebase + temp_jail (bound by access).
        let mut readable: Vec<PathBuf> = Vec::new();
        let mut writable: Vec<PathBuf> = Vec::new();

        if access.reads {
            readable.push(codebase.clone());
            readable.push(temp_jail.to_path_buf());
        }
        if access.writes {
            writable.push(codebase.clone());
            writable.push(temp_jail.to_path_buf());
        }

        // Extend from config (tilde-expand "~" prefix paths).
        // `"*"` is the open-read sentinel — it sets read_all=true and must not be
        // added as a literal path, so filter it out before tilde-expanding.
        let home = dirs_next::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
        let read_all = cfg.readable.iter().any(|r| r == "*");
        for p in cfg.readable.iter().filter(|r| *r != "*") {
            readable.push(expand_tilde(p, &home));
        }
        if access.writes {
            for p in &cfg.writable {
                writable.push(expand_tilde(p, &home));
            }
        }

        // Network policy from config (gated on tool access).
        let network = match &cfg.network {
            NetworkConfig::Off => NetworkPolicy::Deny,
            NetworkConfig::All => NetworkPolicy::All,
            NetworkConfig::Hosts(h) => {
                if access.network {
                    NetworkPolicy::Hosts(h.clone())
                } else {
                    NetworkPolicy::Deny
                }
            }
        };
        // If tool doesn't access network, deny regardless of config.
        let network = if access.network {
            network
        } else {
            NetworkPolicy::Deny
        };

        // Phase 2d: read_all is derived from config — only `"*"` in cfg.readable opens reads.
        // When read_all=false, codebase and temp_jail are already in `readable` (added above
        // when access.reads=true), so the SBPL confined-read subpath rules will cover them.
        SandboxScope {
            readable,
            writable,
            read_all,
            network,
        }
    }

    fn expand_tilde(p: &str, home: &Path) -> PathBuf {
        if let Some(rest) = p.strip_prefix("~/") {
            home.join(rest)
        } else if p == "~" {
            home.to_path_buf()
        } else {
            PathBuf::from(p)
        }
    }
}

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
            let profile = sbpl::build_sbpl(scope);
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

/// Paths that are always readable on this platform regardless of the `readable` config —
/// required for the process to exec, resolve DNS, and do TLS. Returned without the internal
/// `@` literal-vs-subpath sentinel. Exposed so callers (e.g. `gait init`) can surface these
/// in generated config comments so users know what the sandbox implicitly allows.
pub fn platform_system_reads() -> &'static [&'static str] {
    #[cfg(target_os = "macos")]
    {
        &[
            "/", "/usr", "/System", "/Library", "/bin", "/sbin", "/private", "/var", "/etc", "/dev",
        ]
    }
    #[cfg(not(target_os = "macos"))]
    {
        &[]
    }
}

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

// ── brain_scope ───────────────────────────────────────────────────────────────

/// Build the brain's self-seal scope: compute + network only; all disk data arrives
/// via `BrainInit` / spine relay.
///
/// The brain is a pure compute process — it holds no codebase paths, reads no files,
/// and writes nothing to disk. All context (session log, file previews, permissions)
/// is delivered in-memory by the spine before the seal is applied. Outbound network
/// is required for LLM API calls (api.anthropic.com:443 via reqwest+rustls).
///
/// Validated spike (macOS 15 / Darwin 24.5.0): platform-default read paths
/// (dyld, /usr, /System, /Library, /private, /bin, /sbin, /var, /etc, /dev)
/// are sufficient for process exec + DNS + TLS with bundled webpki-roots.
/// No codebase or gait-dir reads are needed after `BrainInit` is delivered.
pub fn brain_scope() -> SandboxScope {
    SandboxScope {
        read_all: false,
        readable: vec![],
        writable: vec![],
        network: NetworkPolicy::All,
    }
}

/// Resolve `.` and `..` components lexically, without touching the filesystem.
/// Unlike `canonicalize`, this does NOT resolve symlinks (so it is a sound,
/// disk-free approximation for the sealed brain's advisory scope pre-check;
/// the hand's kernel seal remains the authoritative enforcement).
pub fn normalize_lexical(path: &std::path::Path) -> std::path::PathBuf {
    debug_assert!(
        path.is_absolute(),
        "normalize_lexical: caller must pass an absolute path"
    );
    use std::path::Component;
    let mut out = std::path::PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    // ── normalize_lexical tests ───────────────────────────────────────────────

    #[test]
    fn normalize_lexical_resolves_dotdot() {
        // Use an absolute path — normalize_lexical requires absolute paths (debug_assert).
        let result = normalize_lexical(std::path::Path::new("/a/b/../c"));
        assert_eq!(result, PathBuf::from("/a/c"));
    }

    #[test]
    fn normalize_lexical_removes_single_dot() {
        let result = normalize_lexical(std::path::Path::new("/x/./y"));
        assert_eq!(result, PathBuf::from("/x/y"));
    }

    #[test]
    fn normalize_lexical_absolute_with_dotdot() {
        let result = normalize_lexical(std::path::Path::new("/a/b/../c"));
        assert_eq!(result, PathBuf::from("/a/c"));
    }

    #[test]
    fn normalize_lexical_no_change_needed() {
        let result = normalize_lexical(std::path::Path::new("/a/b/c"));
        assert_eq!(result, PathBuf::from("/a/b/c"));
    }

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
    fn sbpl_network_all_allows_trustd() {
        let scope = SandboxScope {
            readable: vec![],
            writable: vec![],
            read_all: false,
            network: NetworkPolicy::All,
        };
        let sbpl = build_sbpl(&scope);
        assert!(
            sbpl.contains(r#"(allow mach-lookup (global-name "com.apple.trustd"))"#),
            "expected trustd mach-lookup grant in SBPL output, got:\n{sbpl}"
        );
    }

    #[test]
    fn sbpl_network_hosts_allows_trustd() {
        let scope = SandboxScope {
            readable: vec![],
            writable: vec![],
            read_all: false,
            network: NetworkPolicy::Hosts(vec!["github.com".into()]),
        };
        let sbpl = build_sbpl(&scope);
        assert!(
            sbpl.contains(r#"(allow mach-lookup (global-name "com.apple.trustd"))"#),
            "expected trustd mach-lookup grant in SBPL output, got:\n{sbpl}"
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
            !p.contains("(allow file-read*)"),
            "must NOT blanket-allow reads"
        );
        // Platform defaults: broad paths validated on Darwin 24.5.0.
        assert!(p.contains("(subpath \"/usr\")"), "must allow /usr: {p}");
        assert!(
            p.contains("(subpath \"/System\")"),
            "must allow /System: {p}"
        );
        assert!(
            p.contains("(subpath \"/private\")"),
            "must allow /private: {p}"
        );
        assert!(
            p.contains("(literal \"/\")"),
            "must allow root literal: {p}"
        );
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

    /// Regression guard: `brain_scope()` must emit a confined-read SBPL profile —
    /// no blanket `(allow file-read*)`, platform-default reads only, full network.
    ///
    /// If this test fails after a `brain_scope()` change, re-run the runtime audit
    /// (Steps 1 and 4 in task-2-brief.md) before relaxing `read_all`.
    #[test]
    fn brain_scope_emits_confined_read_profile() {
        let scope = brain_scope();
        assert!(!scope.read_all, "brain_scope must NOT set read_all=true");
        assert!(
            scope.readable.is_empty(),
            "brain_scope must have empty readable"
        );
        assert!(
            scope.writable.is_empty(),
            "brain_scope must have empty writable"
        );
        assert!(
            matches!(scope.network, NetworkPolicy::All),
            "brain_scope must allow all network"
        );

        let p = build_sbpl(&scope);
        assert!(
            !p.contains("(allow file-read*)"),
            "brain_scope profile must NOT blanket-allow file reads: {p}"
        );
        assert!(
            p.contains("(allow network-outbound)"),
            "brain_scope profile must allow network outbound: {p}"
        );
        assert!(
            p.contains("(allow system-socket)"),
            "brain_scope profile must allow system-socket for DNS: {p}"
        );
        // Platform defaults must still be present so dyld/TLS can run.
        #[cfg(target_os = "macos")]
        {
            assert!(p.contains("(subpath \"/usr\")"), "must allow /usr: {p}");
            assert!(
                p.contains("(subpath \"/System\")"),
                "must allow /System: {p}"
            );
            assert!(
                p.contains("(literal \"/\")"),
                "must allow root literal: {p}"
            );
        }
    }

    #[test]
    fn bound_request_ignores_brain_claimed_scope() {
        use gait_types::config::SandboxConfig;
        use std::path::Path;
        // A read tool with a bogus claimed writable must get empty writable.
        let cfg = SandboxConfig::default();
        let scope =
            super::ceiling::bound_request("read", &cfg, Path::new("/repo"), Path::new("/tmp/jail"));
        assert!(scope.writable.is_empty(), "read tool must not get writable");
        assert!(!scope.read_all, "default config must confine reads");
    }

    #[test]
    fn bound_request_shell_gets_writes_and_spawns() {
        use gait_types::config::SandboxConfig;
        use std::path::{Path, PathBuf};
        let cfg = SandboxConfig::default();
        let scope = super::ceiling::bound_request(
            "shell",
            &cfg,
            Path::new("/repo"),
            Path::new("/tmp/jail"),
        );
        assert!(scope.writable.contains(&PathBuf::from("/repo")));
        assert!(scope.writable.contains(&PathBuf::from("/tmp/jail")));
    }

    #[test]
    fn bound_request_unknown_tool_gets_no_access() {
        use gait_types::config::SandboxConfig;
        use std::path::Path;
        let cfg = SandboxConfig::default();
        let scope = super::ceiling::bound_request(
            "spawn_agent",
            &cfg,
            Path::new("/repo"),
            Path::new("/tmp/jail"),
        );
        assert!(scope.writable.is_empty());
        assert!(scope.readable.is_empty());
        assert!(matches!(scope.network, NetworkPolicy::Deny));
    }

    #[test]
    fn read_only_tool_gets_no_writable() {
        use gait_types::config::SandboxConfig;
        use gait_types::types::ToolAccess;
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
        use gait_types::config::SandboxConfig;
        use gait_types::types::ToolAccess;
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
        use gait_types::config::SandboxConfig;
        use gait_types::types::ToolAccess;
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
        use gait_types::config::SandboxConfig;
        use gait_types::types::ToolAccess;
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
        use gait_types::config::SandboxConfig;
        use gait_types::types::ToolAccess;
        use std::path::Path;
        let cfg = SandboxConfig {
            readable: vec!["*".into()],
            ..Default::default()
        };
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

    #[test]
    fn access_for_tool_shell_network_default_false() {
        use gait_types::config::SandboxConfig;
        let cfg = SandboxConfig::default();
        let access = super::ceiling::access_for_tool("shell", &cfg);
        assert!(!access.network);
    }

    #[test]
    fn access_for_tool_shell_network_overridden_true() {
        use gait_types::config::{SandboxConfig, ToolAccessOverride};
        let mut cfg = SandboxConfig::default();
        cfg.tools.insert(
            "shell".to_string(),
            ToolAccessOverride {
                reads: None,
                writes: None,
                network: Some(true),
            },
        );
        let access = super::ceiling::access_for_tool("shell", &cfg);
        assert!(access.network);
        // reads/writes keep their built-in default (true for shell) since the
        // override left them unset.
        assert!(access.reads);
        assert!(access.writes);
    }

    #[test]
    fn access_for_tool_unset_override_fields_fall_through_to_default() {
        use gait_types::config::{SandboxConfig, ToolAccessOverride};
        let mut cfg = SandboxConfig::default();
        cfg.tools.insert(
            "webfetch".to_string(),
            ToolAccessOverride {
                reads: Some(true),
                writes: None,
                network: None,
            },
        );
        let access = super::ceiling::access_for_tool("webfetch", &cfg);
        assert!(access.reads); // overridden
        assert!(access.network); // built-in default for webfetch, unset in override
    }

    #[test]
    fn bound_request_applies_per_tool_override() {
        use gait_types::config::{NetworkConfig, SandboxConfig, ToolAccessOverride};
        use std::path::Path;
        let mut cfg = SandboxConfig::default();
        cfg.network = NetworkConfig::All;
        cfg.tools.insert(
            "shell".to_string(),
            ToolAccessOverride {
                reads: None,
                writes: None,
                network: Some(true),
            },
        );
        let scope =
            super::ceiling::bound_request("shell", &cfg, Path::new("/tmp"), Path::new("/tmp"));
        // NetworkPolicy does not derive PartialEq (confirmed: sandbox.rs's existing
        // tests all use matches!, not assert_eq!, for this exact reason) — use
        // matches! here too, not assert_eq!.
        assert!(matches!(scope.network, NetworkPolicy::All));
    }

    #[test]
    fn codebase_var_expands_in_readable_and_writable() {
        use gait_types::config::SandboxConfig;
        use gait_types::types::ToolAccess;
        use std::path::{Path, PathBuf};
        let cfg = SandboxConfig {
            readable: vec!["$CODEBASE".into()],
            writable: vec!["$CODEBASE".into()],
            ..Default::default()
        };
        let codebase = Path::new("/repo/myproject");
        let scope = super::ceiling::compute_scope_nofs(
            ToolAccess {
                reads: true,
                writes: true,
                ..Default::default()
            },
            &cfg,
            codebase,
            Path::new("/tmp/jail"),
        );
        assert!(scope.readable.contains(&PathBuf::from("/repo/myproject")));
        assert!(scope.writable.contains(&PathBuf::from("/repo/myproject")));
    }

    // ── compute_scope_nofs tests ──────────────────────────────────────────────

    #[test]
    fn compute_scope_nofs_read_tool_gets_readable_paths() {
        use gait_types::config::SandboxConfig;
        use gait_types::types::ToolAccess;
        use std::path::Path;
        let cfg = SandboxConfig::default();
        let scope = super::ceiling::compute_scope_nofs(
            ToolAccess {
                reads: true,
                ..Default::default()
            },
            &cfg,
            Path::new("/repo/sub/../src"), // deliberately un-normalized — passed as-given to verify compute_scope_nofs stores the codebase path verbatim (no normalization inside)
            Path::new("/tmp/jail"),
        );
        // The codebase path is used as-given (no canonicalize call)
        assert!(scope
            .readable
            .iter()
            .any(|p| p == Path::new("/repo/sub/../src")));
        assert!(scope.writable.is_empty());
    }

    #[test]
    fn compute_scope_nofs_write_tool_gets_writable() {
        use gait_types::config::SandboxConfig;
        use gait_types::types::ToolAccess;
        use std::path::{Path, PathBuf};
        let cfg = SandboxConfig::default();
        let scope = super::ceiling::compute_scope_nofs(
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
    fn compute_scope_delegates_to_nofs() {
        // compute_scope_nofs produces same shape (count, read_all) as compute_scope.
        // The actual path values may differ when the OS has symlinks (e.g. /tmp →
        // /private/tmp on macOS), so we only compare structure, not exact paths.
        use gait_types::config::SandboxConfig;
        use gait_types::types::ToolAccess;
        use std::path::Path;
        let cfg = SandboxConfig::default();
        let access = ToolAccess {
            reads: true,
            writes: true,
            ..Default::default()
        };
        let codebase = Path::new("/tmp");
        let jail = Path::new("/tmp/jail");
        let via_full = super::ceiling::compute_scope(access, &cfg, codebase, jail);
        let via_nofs = super::ceiling::compute_scope_nofs(access, &cfg, codebase, jail);
        assert_eq!(via_full.writable.len(), via_nofs.writable.len());
        assert_eq!(via_full.readable.len(), via_nofs.readable.len());
        assert_eq!(via_full.read_all, via_nofs.read_all);
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
    /// (or `(literal …)` for the root entry) when `scope.read_all` is false
    /// (confined-read mode, Phase 2d+).
    ///
    /// Derived from the Codex sandbox baseline and validated on Darwin 24.5.0 by
    /// iterating C-based SBPL tests until `sh -c "echo hi"` exits 0.
    ///
    /// Key findings:
    /// - `/usr` (full subtree) is required — narrowing to `/usr/lib` etc. is not enough;
    ///   dyld walks the whole `/usr` tree for shared cache probing.
    /// - `"/"` (root literal, NOT subpath) is required for `getcwd()` and path resolution;
    ///   without it, processes receive SIGABRT even with all other paths allowed.
    /// - `/private` (full subtree) covers `/private/var`, `/private/tmp`, `/private/etc`.
    /// - `/var` is needed as the non-canonical alias (symlink to `/private/var`).
    /// - `/Library` (full subtree) covers Frameworks, Preferences, Apple sub-trees.
    /// - The dyld shared cache on macOS 13+ Sequoia lives under
    ///   `/System/Volumes/Preboot/Cryptexes/OS/System/Library/dyld/`, covered by `/System`.
    /// - `/private/var/db/dyld` no longer exists on macOS Sequoia; kept as comment only.
    #[cfg(target_os = "macos")]
    fn platform_read_defaults() -> &'static [&'static str] {
        &[
            // Root directory literal — required for getcwd() and path resolution by dyld/sh.
            // Must be emitted as (literal "/") not (subpath "/").  Sentinel: prefix with "@".
            "@/", // /usr (full): dyld, libc, bin, sbin, libexec, share, etc.
            "/usr",
            // System frameworks: Foundation, CoreFoundation, PrivateFrameworks, dyld cache.
            // On macOS Sequoia the dyld shared cache is under:
            //   /System/Volumes/Preboot/Cryptexes/OS/System/Library/dyld/
            "/System",
            // /Library (full): Frameworks, Preferences, Apple, LaunchDaemons, etc.
            "/Library",
            // Standard executables — /bin and /sbin are separate mount points on macOS.
            "/bin", "/sbin",
            // /private (full): /private/var (db, folders, run, tmp), /private/etc, /private/tmp.
            "/private",
            // /var as the non-canonical symlink alias (→ /private/var).
            // Seatbelt matches pre-symlink-resolution paths for some operations.
            "/var", // /etc as the non-canonical symlink alias (→ /private/etc).
            "/etc",
            // /dev: device nodes (null, urandom, tty, fd, ptmx, dtracehelper).
            "/dev",
            // NOTE: /tmp and /private/tmp are intentionally NOT in read defaults.
            // Processes that need to write temp files get explicit writable scope entries.
            // Omitting /tmp allows tests to use /tmp as a confidentiality-test sentinel
            // (out-of-scope reads should be denied).
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
        // process-info* and signal are required for any parent↔child coordination
        // (waitpid, kill, getpid, etc.). Without these, spawned subprocesses may
        // receive SIGABRT when the sandbox policy denies the credential lookup.
        writeln!(s, "(allow process-info* (target same-sandbox))").unwrap();
        writeln!(s, "(allow signal (target same-sandbox))").unwrap();
        writeln!(s, "(allow sysctl-read)").unwrap();

        // IOKit: RootDomainUserClient is queried by CoreFoundation/libSystem at
        // startup; denying it causes SIGABRT on Darwin 24.5+.
        #[cfg(target_os = "macos")]
        writeln!(
            s,
            r#"(allow iokit-open (iokit-registry-entry-class "RootDomainUserClient"))"#
        )
        .unwrap();

        // Mach services required by libSystem / CoreFoundation at process startup.
        // opendirectoryd.libinfo: getpwuid/getgrgid credential lookups (bash, dyld).
        // cfprefsd: CoreFoundation user-preference daemon; accessed even in non-GUI apps.
        // cfprefsd.agent (local-name): per-user agent variant.
        // system.logger + logd: logging plumbing (libSystem calls these at init).
        #[cfg(target_os = "macos")]
        {
            writeln!(s, r#"(allow mach-lookup"#).unwrap();
            writeln!(
                s,
                r#"  (global-name "com.apple.system.opendirectoryd.libinfo")"#
            )
            .unwrap();
            writeln!(
                s,
                r#"  (global-name "com.apple.system.DirectoryService.libinfo_v1")"#
            )
            .unwrap();
            writeln!(s, r#"  (global-name "com.apple.cfprefsd.daemon")"#).unwrap();
            writeln!(s, r#"  (global-name "com.apple.cfprefsd.agent")"#).unwrap();
            writeln!(s, r#"  (local-name "com.apple.cfprefsd.agent")"#).unwrap();
            writeln!(s, r#"  (global-name "com.apple.logd")"#).unwrap();
            writeln!(s, r#"  (global-name "com.apple.system.logger")"#).unwrap();
            writeln!(s, r#")"#).unwrap();
        }

        // User preferences (CFPreferences): libSystem reads these at startup.
        #[cfg(target_os = "macos")]
        {
            writeln!(s, "(allow user-preference-read)").unwrap();
            writeln!(
                s,
                r#"(allow ipc-posix-shm-read* (ipc-posix-name-prefix "apple.cfprefs."))"#
            )
            .unwrap();
            writeln!(s, "(allow ipc-posix-sem)").unwrap();
        }

        // MAC policy syscalls needed for firmlink resolution and sandbox queries.
        #[cfg(target_os = "macos")]
        {
            writeln!(
                s,
                r#"(allow system-mac-syscall (mac-policy-name "vnguard"))"#
            )
            .unwrap();
            writeln!(
                s,
                r#"(allow system-mac-syscall (require-all (mac-policy-name "Sandbox") (mac-syscall-number 67)))"#
            )
            .unwrap();
        }

        // File-map-executable: required by dyld to map framework binaries.
        // Blanket allow (not path-restricted) because dyld maps binaries from many
        // locations including /usr, /System, /Library, /bin, /sbin, and scope paths.
        // Restricting to named subpaths causes SIGABRT in practice (Darwin 24.5.0).
        #[cfg(target_os = "macos")]
        writeln!(s, "(allow file-map-executable)").unwrap();

        // Terminal and standard file descriptor access for shell subprocesses.
        // /dev/tty: required by many programs (even in non-interactive mode for ioctl checks).
        // /dev/fd/*: used by sh for /dev/stdin etc.
        #[cfg(target_os = "macos")]
        {
            writeln!(s, r#"(allow file-read* file-write* (literal "/dev/tty"))"#).unwrap();
            writeln!(s, r#"(allow file-read* file-write* (literal "/dev/ptmx"))"#).unwrap();
            writeln!(
                s,
                r#"(allow file-read* file-write-data (subpath "/dev/fd"))"#
            )
            .unwrap();
            writeln!(s, r#"(allow file-read-metadata (subpath "/dev"))"#).unwrap();
        }

        // Reads. Phase 2b blanket-allows reads when read_all=true.
        // Phase 2d (confined-read): when read_all=false, emit per-path subpath
        // rules for platform defaults + scope.readable instead of a blanket allow.
        if scope.read_all {
            writeln!(s, "(allow file-read*)").unwrap();
        } else {
            // Platform defaults: paths every sealed macOS process must read to exec,
            // resolve DNS, and do TLS. Empty on non-macOS (SBPL never applied there).
            // Entries prefixed with "@" are emitted as (literal …) instead of (subpath …).
            // Currently only "@/" uses this — root directory must be a literal, not subpath.
            for root in platform_read_defaults() {
                if let Some(path) = root.strip_prefix('@') {
                    writeln!(s, r#"(allow file-read* (literal "{path}"))"#).unwrap();
                } else {
                    writeln!(s, r#"(allow file-read* (subpath "{root}"))"#).unwrap();
                }
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
        // Canonicalize writable paths so symlinks in the path (e.g. /var → /private/var
        // on macOS) are resolved to the kernel-visible path before embedding in the profile.
        for p in &scope.writable {
            let c = std::fs::canonicalize(p).unwrap_or_else(|_| p.clone());
            writeln!(
                s,
                r#"(allow file-write* file-read* (subpath "{}"))"#,
                sbpl_escape(&c)
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
                // TLS cert-chain validation is an XPC call to trustd, not a file
                // read — without this, DNS/TCP succeed but every HTTPS handshake
                // fails with a generic CERTIFICATE_VERIFY_FAILED.
                writeln!(s, r#"(allow mach-lookup (global-name "com.apple.trustd"))"#).unwrap();
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
                writeln!(s, r#"(allow mach-lookup (global-name "com.apple.trustd"))"#).unwrap();
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
    use gait_types::config::{NetworkConfig, SandboxConfig};
    use gait_types::types::ToolAccess;
    use std::path::{Path, PathBuf};

    /// Return the `ToolAccess` declared by the named builtin tool.
    /// Unknown tools get the zero value (no access) — they are brain-side control tools
    /// and must never run in a hand. This is the spine's authoritative source for scope
    /// computation; the brain's claimed scope is ignored.
    pub fn access_for_tool(tool_name: &str, cfg: &SandboxConfig) -> ToolAccess {
        let mut access = match tool_name {
            "read" | "grep" | "glob" | "ls" | "codesearch" => ToolAccess {
                reads: true,
                ..Default::default()
            },
            "write" | "apply_patch" => ToolAccess {
                reads: true,
                writes: true,
                ..Default::default()
            },
            "shell" => ToolAccess {
                reads: true,
                writes: true,
                spawns: true,
                ..Default::default()
            },
            "webfetch" => ToolAccess {
                network: true,
                ..Default::default()
            },
            _ => ToolAccess::default(),
        };
        if let Some(o) = cfg.tools.get(tool_name) {
            if let Some(reads) = o.reads {
                access.reads = reads;
            }
            if let Some(writes) = o.writes {
                access.writes = writes;
            }
            if let Some(network) = o.network {
                access.network = network;
            }
        }
        access
    }

    /// Compute the authoritative per-call scope for `tool_name` without trusting
    /// anything the brain claimed. The spine calls this after receiving an `Impulse`
    /// — it derives access from the builtin tool registry and intersects with config.
    /// A brain that sends a bogus claimed scope (e.g., `writable: ["/"]` on a read
    /// tool) cannot widen past what `access_for_tool` permits.
    pub fn bound_request(
        tool_name: &str,
        cfg: &SandboxConfig,
        codebase: &Path,
        jail: &Path,
    ) -> SandboxScope {
        compute_scope(access_for_tool(tool_name, cfg), cfg, codebase, jail)
    }

    /// Compute the per-call sandbox scope ceiling.
    ///
    /// Base: `{codebase, temp_jail}` for reads + writes the tool needs.
    /// Extended with: config `readable` (always) and config `writable` (when writes
    /// are permitted by the tool). `~/` is tilde-expanded against `dirs_next::home_dir()`.
    /// Network: config `Off` always denies; `Hosts`/`All` honored only when
    /// `access.network` is true. `read_all` defaults to true in Phase 2b; Phase 2d
    /// will flip it to false and emit per-path subpath rules instead.
    ///
    /// Canonicalizes `codebase` via the filesystem. For a disk-free variant (e.g., the
    /// sealed brain's advisory pre-check), use [`compute_scope_nofs`] with a
    /// lexically-normalized codebase path.
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
        compute_scope_nofs(access, cfg, &codebase, temp_jail)
    }

    /// Like [`compute_scope`], but assumes `codebase` is already normalized and does
    /// NO filesystem access — for the sealed brain's pre-check. The seal path keeps
    /// using [`compute_scope`] (which canonicalizes, on the unsealed spine/hand side).
    pub fn compute_scope_nofs(
        access: ToolAccess,
        cfg: &SandboxConfig,
        codebase: &Path, // caller-normalized (e.g. via normalize_lexical)
        temp_jail: &Path,
    ) -> SandboxScope {
        // Base readable + writable: codebase + temp_jail (bound by access).
        let mut readable: Vec<PathBuf> = Vec::new();
        let mut writable: Vec<PathBuf> = Vec::new();

        if access.reads {
            readable.push(codebase.to_path_buf());
            readable.push(temp_jail.to_path_buf());
        }
        if access.writes {
            writable.push(codebase.to_path_buf());
            writable.push(temp_jail.to_path_buf());
        }

        // Extend from config (expand "~/" and "$CODEBASE" in paths).
        // `"*"` is the open-read sentinel — it sets read_all=true and must not be
        // added as a literal path, so filter it out before expanding.
        let home = dirs_next::home_dir().unwrap_or_else(|| PathBuf::from("/tmp"));
        let read_all = cfg.readable.iter().any(|r| r == "*");
        for p in cfg.readable.iter().filter(|r| *r != "*") {
            readable.push(expand_path_vars(p, &home, codebase));
        }
        if access.writes {
            for p in &cfg.writable {
                writable.push(expand_path_vars(p, &home, codebase));
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

    /// Expand `~/` (home dir) and `$CODEBASE` (codebase root) in a config path string.
    fn expand_path_vars(p: &str, home: &Path, codebase: &Path) -> PathBuf {
        if let Some(rest) = p.strip_prefix("$CODEBASE/") {
            codebase.join(rest)
        } else if p == "$CODEBASE" {
            codebase.to_path_buf()
        } else if let Some(rest) = p.strip_prefix("~/") {
            home.join(rest)
        } else if p == "~" {
            home.to_path_buf()
        } else {
            PathBuf::from(p)
        }
    }
}

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
/// Stub — not yet implemented; returns [`SandboxStatus::Unavailable`].
pub struct SeatbeltSandbox;

impl Sandbox for SeatbeltSandbox {
    fn apply(&self, _scope: &SandboxScope) -> anyhow::Result<SandboxStatus> {
        Ok(SandboxStatus::Unavailable {
            reason: "Seatbelt not yet implemented".to_string(),
        })
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

    #[test]
    fn seatbelt_stub_returns_unavailable() {
        let scope = SandboxScope {
            writable: vec![PathBuf::from("src/")],
            ..Default::default()
        };
        let s = SeatbeltSandbox.apply(&scope).unwrap();
        assert!(matches!(s, SandboxStatus::Unavailable { .. }));
    }

    #[test]
    fn platform_sandbox_is_box_dyn_sandbox() {
        let sb = platform_sandbox();
        let _ = sb.apply(&SandboxScope::default()).unwrap();
    }

    #[test]
    fn noop_sandbox_with_default_scope_returns_noop() {
        let s = NoopSandbox.apply(&SandboxScope::default()).unwrap();
        assert!(matches!(s, SandboxStatus::Noop));
    }
}

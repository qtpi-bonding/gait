//! Dependency-light host facts shared by domain-specific path discovery.
//!
//! This crate reports the compiled platform, native user directories, and
//! environment values. It deliberately does not define Gait paths, external
//! CLI conventions, filesystem operations, process APIs, or sandbox backends.
//! Consumers turn these facts into their own policies.

use std::ffi::OsString;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlatformKind {
    MacOs,
    Linux,
    Windows,
    Other,
}

/// Injectable host directories and environment access.
///
/// Tool-specific conventions such as `CODEX_HOME` and Gait-specific conventions
/// such as `GAIT_HOME` remain in their domain crates. Production uses
/// [`NativePlatform`]; tests can provide a fake.
///
/// Keep this trait limited to host facts. New process, keyring, filesystem,
/// clock, executable-lookup, and network capabilities belong behind separate
/// domain boundaries.
pub trait Platform: Send + Sync {
    /// Operating-system family of this host implementation.
    fn kind(&self) -> PlatformKind;
    /// Native user home directory.
    fn home_dir(&self) -> Option<PathBuf>;
    /// Native base directory for user configuration.
    fn config_dir(&self) -> Option<PathBuf>;
    /// Native base directory for user data.
    fn data_dir(&self) -> Option<PathBuf>;
    /// Native base directory for disposable user caches.
    fn cache_dir(&self) -> Option<PathBuf>;
    /// Non-empty environment value, preserving non-Unicode platform strings.
    fn env(&self, name: &str) -> Option<OsString>;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct NativePlatform;

impl Platform for NativePlatform {
    fn kind(&self) -> PlatformKind {
        if cfg!(target_os = "macos") {
            PlatformKind::MacOs
        } else if cfg!(target_os = "linux") {
            PlatformKind::Linux
        } else if cfg!(target_os = "windows") {
            PlatformKind::Windows
        } else {
            PlatformKind::Other
        }
    }

    fn home_dir(&self) -> Option<PathBuf> {
        dirs_next::home_dir()
    }

    fn config_dir(&self) -> Option<PathBuf> {
        dirs_next::config_dir()
    }

    fn data_dir(&self) -> Option<PathBuf> {
        dirs_next::data_dir()
    }

    fn cache_dir(&self) -> Option<PathBuf> {
        dirs_next::cache_dir()
    }

    fn env(&self, name: &str) -> Option<OsString> {
        std::env::var_os(name).filter(|value| !value.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_platform_reports_the_compiled_target() {
        let expected = if cfg!(target_os = "macos") {
            PlatformKind::MacOs
        } else if cfg!(target_os = "linux") {
            PlatformKind::Linux
        } else if cfg!(target_os = "windows") {
            PlatformKind::Windows
        } else {
            PlatformKind::Other
        };
        assert_eq!(NativePlatform.kind(), expected);
    }
}

//! Global, machine-local Gait state.
//!
//! `GaitHome` is deliberately distinct from a gait-dir. A gait-dir is a Git
//! repository containing durable workspace/session state; `GaitHome` is a
//! non-repository root for credentials and caches.

use gait_platform::{NativePlatform, Platform};
use std::path::{Path, PathBuf};

pub const GAIT_HOME_ENV: &str = "GAIT_HOME";
const DEFAULT_GAIT_HOME_DIR: &str = ".gait-home";

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum GaitHomeError {
    #[error("cannot resolve GAIT_HOME: user home directory is unavailable")]
    HomeUnavailable,
    #[error("GAIT_HOME must be an absolute path: {0}")]
    RelativePath(PathBuf),
}

/// Strongly typed root for global Gait state.
///
/// This type computes paths only. Callers performing writes remain responsible
/// for creating the required parent directories.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GaitHome {
    root: PathBuf,
}

impl GaitHome {
    /// Resolve from `GAIT_HOME`, then fall back to `<user-home>/.gait-home`.
    pub fn resolve() -> Result<Self, GaitHomeError> {
        Self::resolve_with(&NativePlatform)
    }

    /// Injectable form of [`Self::resolve`] for domain callers and tests.
    pub fn resolve_with(platform: &dyn Platform) -> Result<Self, GaitHomeError> {
        if let Some(value) = platform
            .env(GAIT_HOME_ENV)
            .filter(|value| !value.is_empty())
        {
            return Self::from_path(PathBuf::from(value));
        }

        let home = platform.home_dir().ok_or(GaitHomeError::HomeUnavailable)?;
        Self::from_path(home.join(DEFAULT_GAIT_HOME_DIR))
    }

    /// Construct a home from an explicit absolute path.
    pub fn from_path(path: PathBuf) -> Result<Self, GaitHomeError> {
        if !path.is_absolute() {
            return Err(GaitHomeError::RelativePath(path));
        }
        Ok(Self { root: path })
    }

    pub fn as_path(&self) -> &Path {
        &self.root
    }

    pub fn auth_file(&self) -> PathBuf {
        self.root.join("auth.json")
    }

    pub fn auth_pending_dir(&self) -> PathBuf {
        self.root.join("auth-pending")
    }

    pub fn models_cache_file(&self) -> PathBuf {
        self.root.join("cache").join("models.json")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gait_platform::PlatformKind;
    use std::ffi::OsString;

    struct FakePlatform {
        home: Option<PathBuf>,
        gait_home: Option<OsString>,
    }

    impl Platform for FakePlatform {
        fn kind(&self) -> PlatformKind {
            PlatformKind::MacOs
        }

        fn home_dir(&self) -> Option<PathBuf> {
            self.home.clone()
        }

        fn config_dir(&self) -> Option<PathBuf> {
            None
        }

        fn data_dir(&self) -> Option<PathBuf> {
            None
        }

        fn cache_dir(&self) -> Option<PathBuf> {
            None
        }

        fn env(&self, name: &str) -> Option<OsString> {
            (name == GAIT_HOME_ENV)
                .then(|| self.gait_home.clone())
                .flatten()
        }
    }

    #[test]
    fn explicit_absolute_home_wins() {
        let platform = FakePlatform {
            home: Some(PathBuf::from("/Users/test")),
            gait_home: Some(OsString::from("/Volumes/state/gait")),
        };
        assert_eq!(
            GaitHome::resolve_with(&platform).unwrap().as_path(),
            Path::new("/Volumes/state/gait")
        );
    }

    #[test]
    fn defaults_below_user_home() {
        let platform = FakePlatform {
            home: Some(PathBuf::from("/Users/test")),
            gait_home: None,
        };
        assert_eq!(
            GaitHome::resolve_with(&platform).unwrap().as_path(),
            Path::new("/Users/test/.gait-home")
        );
    }

    #[test]
    fn empty_override_is_treated_as_unset() {
        let platform = FakePlatform {
            home: Some(PathBuf::from("/Users/test")),
            gait_home: Some(OsString::new()),
        };
        assert_eq!(
            GaitHome::resolve_with(&platform).unwrap().as_path(),
            Path::new("/Users/test/.gait-home")
        );
    }

    #[test]
    fn relative_override_is_rejected() {
        let platform = FakePlatform {
            home: Some(PathBuf::from("/Users/test")),
            gait_home: Some(OsString::from("relative/gait")),
        };
        assert_eq!(
            GaitHome::resolve_with(&platform),
            Err(GaitHomeError::RelativePath(PathBuf::from("relative/gait")))
        );
    }

    #[test]
    fn missing_user_home_is_an_error() {
        let platform = FakePlatform {
            home: None,
            gait_home: None,
        };
        assert_eq!(
            GaitHome::resolve_with(&platform),
            Err(GaitHomeError::HomeUnavailable)
        );
    }

    #[test]
    fn owned_paths_are_below_the_global_home() {
        let home = GaitHome::from_path(PathBuf::from("/Users/test/.gait-home")).unwrap();
        assert_eq!(
            home.auth_file(),
            PathBuf::from("/Users/test/.gait-home/auth.json")
        );
        assert_eq!(
            home.models_cache_file(),
            PathBuf::from("/Users/test/.gait-home/cache/models.json")
        );
    }
}

use crate::git::Repos;
use crate::session::session_json::read_session_json;
use std::path::PathBuf;

/// The durable runtime instance: identity + bindings + injected repos.
/// Not the live provider/tools — those are `ResolvedProfile`.
/// The live model lives on `ResolvedProfile.model`.
pub struct Session {
    pub id: String,
    pub gait_dir: PathBuf,
    pub codebase_path: PathBuf,
    pub assigned_by: Option<String>,
    pub repos: Repos,
}

impl Session {
    /// Load identity/bindings from `sessions/<id>/session.json` (defaults if absent).
    /// `codebase_path` falls back to the provided default when the json has none.
    pub fn load(
        gait_dir: PathBuf,
        id: String,
        default_codebase: PathBuf,
        repos: Repos,
    ) -> Result<Self, anyhow::Error> {
        let sj = read_session_json(&gait_dir, &id)?;
        let codebase_path = sj
            .codebase_path
            .map(PathBuf::from)
            .unwrap_or(default_codebase);
        Ok(Session {
            id,
            gait_dir,
            codebase_path,
            assigned_by: sj.assigned_by,
            repos,
        })
    }

    /// Convenience constructor for the common case: creates a session using the
    /// production git repos, without requiring the caller to construct `Repos`.
    /// Use `Session::load` directly when you need a custom `Repos` (e.g. tests).
    pub fn open(
        gait_dir: PathBuf,
        default_codebase: PathBuf,
        id: impl Into<String>,
    ) -> Result<Self, anyhow::Error> {
        Self::load(
            gait_dir,
            id.into(),
            default_codebase,
            crate::git::Repos::production(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn load_defaults_when_no_session_json() {
        let dir = TempDir::new().unwrap();
        let s = Session::load(
            dir.path().to_path_buf(),
            "s1".into(),
            std::path::PathBuf::from("/code"),
            Repos::production(),
        )
        .unwrap();
        assert_eq!(s.id, "s1");
        assert_eq!(s.codebase_path, std::path::PathBuf::from("/code"));
        assert!(s.assigned_by.is_none());
    }

    #[test]
    fn open_creates_session_without_explicit_repos() {
        let dir = TempDir::new().unwrap();
        let s = Session::open(
            dir.path().to_path_buf(),
            std::path::PathBuf::from("/code"),
            "s-open-test",
        )
        .unwrap();
        assert_eq!(s.id, "s-open-test");
        assert_eq!(s.codebase_path, std::path::PathBuf::from("/code"));
    }
}

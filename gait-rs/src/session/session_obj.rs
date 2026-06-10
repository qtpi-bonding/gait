use std::path::PathBuf;
use crate::git::Repos;
use crate::session::session_json::read_session_json;

/// The durable runtime instance: identity + bindings + injected repos.
/// Not the live provider/tools — those are `ResolvedProfile`.
pub struct Session {
    pub id: String,
    pub workspace: PathBuf,
    pub codebase_path: PathBuf,
    pub model: String,
    pub assigned_by: Option<String>,
    pub repos: Repos,
}

impl Session {
    /// Load identity/bindings from `sessions/<id>/session.json` (defaults if absent).
    /// `codebase_path` falls back to the provided default when the json has none.
    pub fn load(
        workspace: PathBuf,
        id: String,
        model: String,
        default_codebase: PathBuf,
        repos: Repos,
    ) -> Self {
        let sj = read_session_json(&workspace, &id);
        let codebase_path = sj.codebase_path
            .map(PathBuf::from)
            .unwrap_or(default_codebase);
        Session { id, workspace, codebase_path, model, assigned_by: sj.assigned_by, repos }
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
            dir.path().to_path_buf(), "s1".into(), "anthropic/x".into(),
            std::path::PathBuf::from("/code"), Repos::production());
        assert_eq!(s.id, "s1");
        assert_eq!(s.codebase_path, std::path::PathBuf::from("/code"));
        assert!(s.assigned_by.is_none());
    }
}

//! Locating a gait workspace, git-style. Precedence: explicit `--gait-dir` >
//! `$GAIT_DIR` > walk up for a `.gait/` directory or a `.gaitref` pointer file >
//! error. `.gait/` is always a dir; `.gaitref` is always a file. Shared by the
//! CLI, the REPL, and the ACP adapter. See
//! docs/plans/2026-07-09-gait-workspace-discovery-design.md.
use std::path::{Path, PathBuf};

use gait_platform::{NativePlatform, Platform};

#[derive(Debug, thiserror::Error, coded_error::CodedError)]
#[coded(namespace = "gait.state.gait_dir")]
pub enum GaitDirError {
    #[error("{0}")]
    #[coded(code = "not_found", class = Logical)]
    NotFound(String),
    #[error("cannot read {path}: {why}")]
    #[coded(code = "gaitref_unreadable", class = Infrastructure)]
    GaitrefUnreadable { path: String, why: String },
    #[error("{0} names no path")]
    #[coded(code = "gaitref_empty", class = Logical)]
    GaitrefEmpty(String),
    #[error("{0}")]
    #[coded(code = "home_unavailable", class = Infrastructure)]
    HomeUnavailable(String),
}

pub fn resolve(explicit: Option<PathBuf>, start: &Path) -> anyhow::Result<PathBuf> {
    resolve_with(&NativePlatform, explicit, start)
}

pub fn resolve_with(
    platform: &dyn Platform,
    explicit: Option<PathBuf>,
    start: &Path,
) -> anyhow::Result<PathBuf> {
    if let Some(dir) = explicit {
        return Ok(dir);
    }
    if let Some(dir) = platform.env("GAIT_DIR").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    discover_with(platform, start)
}

/// Walk up from `start` for a `.gait/` directory or a `.gaitref` pointer file.
/// This is `resolve` **without** the `$GAIT_DIR` step — the ACP adapter is a
/// multi-cwd process and must resolve each session purely from its own cwd.
pub fn discover(start: &Path) -> anyhow::Result<PathBuf> {
    discover_with(&NativePlatform, start)
}

pub fn discover_with(platform: &dyn Platform, start: &Path) -> anyhow::Result<PathBuf> {
    let mut cur = Some(start);
    while let Some(dir) = cur {
        let d = dir.join(".gait");
        if d.is_dir() {
            return Ok(d);
        }
        let r = dir.join(".gaitref");
        if r.is_file() {
            return read_gaitref_with(platform, &r);
        }
        cur = dir.parent();
    }
    Err(coded_error::into_anyhow(GaitDirError::NotFound(format!(
        "not a gait workspace: no .gait/ or .gaitref found from {} upward — \
         run `gait init .gait`, `gait link <gait-dir>`, or pass --gait-dir",
        start.display()
    ))))
}

/// Read a `.gaitref` pointer → the gait-dir it names. First non-empty line,
/// optional `gaitdir:` prefix, then a path (`~`-expanded; absolute verbatim;
/// relative resolved against the `.gaitref` file's own directory).
fn read_gaitref_with(platform: &dyn Platform, marker: &Path) -> anyhow::Result<PathBuf> {
    let raw = std::fs::read_to_string(marker)
        .map_err(|e| {
            coded_error::into_anyhow(GaitDirError::GaitrefUnreadable {
                path: marker.display().to_string(),
                why: e.to_string(),
            })
        })?;
    let line = raw
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    let path = line.strip_prefix("gaitdir:").unwrap_or(line).trim();
    if path.is_empty() {
        return Err(coded_error::into_anyhow(GaitDirError::GaitrefEmpty(
            marker.display().to_string(),
        )));
    }
    let expanded = expand_tilde_with(platform, path)?;
    if expanded.is_absolute() {
        Ok(expanded)
    } else {
        Ok(marker
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(expanded))
    }
}

fn expand_tilde_with(platform: &dyn Platform, path: &str) -> anyhow::Result<PathBuf> {
    if path == "~" {
        return platform.home_dir().ok_or_else(|| {
            coded_error::into_anyhow(GaitDirError::HomeUnavailable(
                "cannot expand '~': user home directory is unavailable".to_string(),
            ))
        });
    }
    if let Some(rest) = path.strip_prefix("~/") {
        let home = platform.home_dir().ok_or_else(|| {
            coded_error::into_anyhow(GaitDirError::HomeUnavailable(format!(
                "cannot expand '{path}': user home directory is unavailable"
            )))
        })?;
        return Ok(home.join(rest));
    }
    Ok(PathBuf::from(path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gait_platform::PlatformKind;
    use std::ffi::OsString;

    struct FakePlatform {
        home: Option<PathBuf>,
        gait_dir: Option<OsString>,
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
            (name == "GAIT_DIR")
                .then(|| self.gait_dir.clone())
                .flatten()
        }
    }

    #[test]
    fn discover_outside_a_workspace_is_a_coded_not_found() {
        let t = tempfile::TempDir::new().unwrap();
        let err = discover_with(&NativePlatform, t.path()).unwrap_err();
        let facts = coded_error::facts_anyhow(&err);
        assert_eq!(facts.code, "gait.state.gait_dir.not_found");
        assert_eq!(facts.class, coded_error::ErrorClass::Logical);
    }

    #[test]
    fn empty_gaitref_is_coded() {
        let t = tempfile::TempDir::new().unwrap();
        std::fs::write(t.path().join(".gaitref"), "  \n").unwrap();
        let err = discover_with(&NativePlatform, t.path()).unwrap_err();
        let facts = coded_error::facts_anyhow(&err);
        assert_eq!(facts.code, "gait.state.gait_dir.gaitref_empty");
        assert_eq!(facts.class, coded_error::ErrorClass::Logical);
    }

    #[test]
    fn gait_dir_error_codes_are_unique() {
        coded_error::check_codes("gait.state.gait_dir.", &[GaitDirError::ALL_CODES]).unwrap();
    }

    #[test]
    fn explicit_wins() {
        let p = PathBuf::from("/x/explicit");
        assert_eq!(
            resolve(Some(p.clone()), Path::new("/unrelated")).unwrap(),
            p
        );
    }

    #[test]
    fn injected_gait_dir_wins_over_discovery() {
        let platform = FakePlatform {
            home: Some(PathBuf::from("/Users/test")),
            gait_dir: Some(OsString::from("/tmp/from-env")),
        };
        assert_eq!(
            resolve_with(&platform, None, Path::new("/unrelated")).unwrap(),
            PathBuf::from("/tmp/from-env")
        );
    }

    #[test]
    fn discover_returns_gait_dir_directory() {
        let t = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(t.path().join(".gait")).unwrap();
        let sub = t.path().join("a/b");
        std::fs::create_dir_all(&sub).unwrap();
        assert_eq!(discover(&sub).unwrap(), t.path().join(".gait"));
    }

    #[test]
    fn discover_follows_gaitref_absolute() {
        let t = tempfile::TempDir::new().unwrap();
        std::fs::write(t.path().join(".gaitref"), "gaitdir: /opt/home\n").unwrap();
        assert_eq!(discover(t.path()).unwrap(), PathBuf::from("/opt/home"));
    }

    #[test]
    fn discover_follows_gaitref_bare_relative_to_marker_dir() {
        let t = tempfile::TempDir::new().unwrap();
        let repo = t.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(repo.join(".gaitref"), ".gait-dir\n").unwrap();
        assert_eq!(discover(&repo).unwrap(), repo.join(".gait-dir"));
    }

    #[test]
    fn discover_prefers_gait_dir_over_gaitref_at_same_level() {
        let t = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(t.path().join(".gait")).unwrap();
        std::fs::write(t.path().join(".gaitref"), "/elsewhere\n").unwrap();
        assert_eq!(discover(t.path()).unwrap(), t.path().join(".gait"));
    }

    #[test]
    fn discover_errors_when_nothing_found() {
        let t = tempfile::TempDir::new().unwrap();
        assert!(discover(t.path()).is_err());
    }

    #[test]
    fn discover_errors_on_empty_gaitref() {
        let t = tempfile::TempDir::new().unwrap();
        std::fs::write(t.path().join(".gaitref"), "\n").unwrap();
        assert!(discover(t.path()).is_err());
    }

    #[test]
    fn discover_expands_gaitref_against_injected_home() {
        let t = tempfile::TempDir::new().unwrap();
        std::fs::write(t.path().join(".gaitref"), "~/workspaces/gait\n").unwrap();
        let platform = FakePlatform {
            home: Some(PathBuf::from("/Users/test")),
            gait_dir: None,
        };
        assert_eq!(
            discover_with(&platform, t.path()).unwrap(),
            PathBuf::from("/Users/test/workspaces/gait")
        );
    }

    #[test]
    fn discover_rejects_tilde_when_home_is_unavailable() {
        let t = tempfile::TempDir::new().unwrap();
        std::fs::write(t.path().join(".gaitref"), "~/workspaces/gait\n").unwrap();
        let platform = FakePlatform {
            home: None,
            gait_dir: None,
        };
        let error = discover_with(&platform, t.path()).unwrap_err();
        assert!(error.to_string().contains("home directory is unavailable"));
    }
}

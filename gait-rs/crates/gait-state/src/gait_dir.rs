//! Locating a gait workspace, git-style. Precedence: explicit `--gait-dir` >
//! `$GAIT_DIR` > walk up for a `.gait/` directory or a `.gaitref` pointer file >
//! error. `.gait/` is always a dir; `.gaitref` is always a file. Shared by the
//! CLI, the REPL, and the ACP adapter. See
//! docs/plans/2026-07-09-gait-workspace-discovery-design.md.
use std::path::{Path, PathBuf};

pub fn resolve(explicit: Option<PathBuf>, start: &Path) -> anyhow::Result<PathBuf> {
    if let Some(dir) = explicit {
        return Ok(dir);
    }
    if let Ok(env) = std::env::var("GAIT_DIR") {
        if !env.is_empty() {
            return Ok(PathBuf::from(env));
        }
    }
    discover(start)
}

/// Walk up from `start` for a `.gait/` directory or a `.gaitref` pointer file.
/// This is `resolve` **without** the `$GAIT_DIR` step — the ACP adapter is a
/// multi-cwd process and must resolve each session purely from its own cwd.
pub fn discover(start: &Path) -> anyhow::Result<PathBuf> {
    let mut cur = Some(start);
    while let Some(dir) = cur {
        let d = dir.join(".gait");
        if d.is_dir() {
            return Ok(d);
        }
        let r = dir.join(".gaitref");
        if r.is_file() {
            return read_gaitref(&r);
        }
        cur = dir.parent();
    }
    anyhow::bail!(
        "not a gait workspace: no .gait/ or .gaitref found from {} upward — \
         run `gait init .gait`, `gait link <gait-dir>`, or pass --gait-dir",
        start.display()
    )
}

/// Read a `.gaitref` pointer → the gait-dir it names. First non-empty line,
/// optional `gaitdir:` prefix, then a path (`~`-expanded; absolute verbatim;
/// relative resolved against the `.gaitref` file's own directory).
fn read_gaitref(marker: &Path) -> anyhow::Result<PathBuf> {
    let raw = std::fs::read_to_string(marker)
        .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", marker.display()))?;
    let line = raw
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    let path = line.strip_prefix("gaitdir:").unwrap_or(line).trim();
    if path.is_empty() {
        anyhow::bail!("{} names no path", marker.display());
    }
    let expanded = expand_tilde(path);
    if expanded.is_absolute() {
        Ok(expanded)
    } else {
        Ok(marker
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(expanded))
    }
}

fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_wins() {
        let p = PathBuf::from("/x/explicit");
        assert_eq!(
            resolve(Some(p.clone()), Path::new("/unrelated")).unwrap(),
            p
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
}

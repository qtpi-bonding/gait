//! Named mode operations shared by CLI and REPL — mirrors `inference.rs`.
//! Selection resolution is pure: frontends submit the returned event through
//! core's session-update port. These functions deliberately perform no
//! terminal I/O.
use crate::OpError;
use std::path::Path;

pub use gait_state::modes::ModeInfo;

pub fn list(gait_dir: &Path) -> Vec<String> {
    gait_state::modes::list_modes(gait_dir)
}

pub fn show(gait_dir: &Path, name: &str) -> Result<ModeInfo, OpError> {
    let m = gait_state::modes::load_mode(gait_dir, name)
        .map_err(|e| OpError::NotFound(format!("mode '{name}' not found: {e}")))?;
    Ok(ModeInfo {
        name: m.name,
        allow: m.allow,
        deny: m.deny,
        steer: m.steer,
        description: m.description,
        classify_policy: m.classify_policy,
    })
}

pub fn resolve_selection(
    gait_dir: &Path,
    name: &str,
) -> Result<gait_state::modes::ResolvedModeSelection, OpError> {
    gait_state::modes::resolve_selection(gait_dir, name)
        .map_err(|e| OpError::NotFound(format!("mode '{name}' not found: {e}")))
}

/// Author a new mode at `<gait_dir>/modes/<name>/MODE.yaml`. Refuses overwrite
/// (mirrors `inference::add`).
pub fn add(
    gait_dir: &Path,
    name: &str,
    allow: &[String],
    deny: &[String],
    steer: &str,
    description: &str,
) -> Result<ModeInfo, OpError> {
    gait_state::modes::write_mode(gait_dir, name, allow, deny, steer, description, None)?;
    Ok(ModeInfo {
        name: name.to_string(),
        allow: allow.to_vec(),
        deny: deny.to_vec(),
        steer: steer.to_string(),
        description: description.to_string(),
        classify_policy: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn list_is_empty_for_a_fresh_gait_dir() {
        let t = TempDir::new().unwrap();
        assert!(list(t.path()).is_empty());
    }

    #[test]
    fn add_then_show_round_trips() {
        let t = TempDir::new().unwrap();
        let info = add(
            t.path(),
            "review",
            &["read".to_string()],
            &["shell:rm *".to_string()],
            "review only",
            "Review mode",
        )
        .unwrap();
        assert_eq!(info.name, "review");
        assert_eq!(info.allow, vec!["read"]);

        let shown = show(t.path(), "review").unwrap();
        assert_eq!(shown.name, "review");
        assert_eq!(shown.deny, vec!["shell:rm *"]);
        assert_eq!(shown.steer, "review only");
    }

    #[test]
    fn add_then_list_includes_it() {
        let t = TempDir::new().unwrap();
        add(t.path(), "write", &["write".to_string()], &[], "", "").unwrap();
        assert_eq!(list(t.path()), vec!["write".to_string()]);
    }

    #[test]
    fn show_missing_mode_is_not_found() {
        let t = TempDir::new().unwrap();
        let err = show(t.path(), "nope").unwrap_err();
        assert!(matches!(err, OpError::NotFound(_)), "got {err:?}");
    }

    #[test]
    fn resolve_selection_default_clears() {
        let t = TempDir::new().unwrap();
        let sel = resolve_selection(t.path(), "default").unwrap();
        assert_eq!(sel.info.name, "default");
    }
}

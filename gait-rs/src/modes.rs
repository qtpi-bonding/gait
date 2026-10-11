//! Session-log activation for user-defined mode overlays. File I/O
//! (`load_mode`, `list_modes`, `write_mode`, `append_mode_allow`,
//! `resolve_selection`) lives in `gait_state::modes` — see that module's doc
//! comment. This file keeps only `activate_mode`, which appends directly to
//! a session's transcript log (a top-level-crate concern, alongside the
//! crate's other session-log writers), plus re-exports so existing
//! `crate::modes::...` call sites elsewhere in this crate keep working
//! unchanged.

pub use gait_state::modes::{
    append_mode_allow, clear_mode, list_modes, load_mode, resolve_selection, write_mode, ModeInfo,
    ResolvedModeSelection,
};

use std::path::Path;

/// Append a `Mode` event to the session log. `name == "" | "default"` appends the
/// empty clear sentinel (back to baseline); any other name is resolved from disk.
pub fn activate_mode(log_path: &Path, gait_dir: &Path, name: &str) -> anyhow::Result<()> {
    let body = if name.is_empty() || name == "default" {
        crate::proto::events::mode("", vec![], vec![], "", "", None)
    } else {
        let m = load_mode(gait_dir, name)?;
        crate::proto::events::mode(
            &m.name,
            m.deny,
            m.allow,
            &m.steer,
            &m.description,
            m.classify_policy,
        )
    };
    // Callers may activate a mode before the session's worktree/log directory
    // has otherwise been created (e.g. `--mode` on a brand-new CLI session).
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    crate::proto::wire::append(log_path, body)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn default_clears_without_disk() {
        let t = TempDir::new().unwrap();
        let log = t.path().join("s.log");
        std::fs::write(&log, b"").unwrap();
        activate_mode(&log, t.path(), "default").unwrap();
    }

    #[test]
    fn activates_an_authored_mode() {
        let t = TempDir::new().unwrap();
        let modes_dir = t.path().join("modes").join("plan");
        std::fs::create_dir_all(&modes_dir).unwrap();
        std::fs::write(modes_dir.join("MODE.md"), "---\ndeny: [write]\n---\n").unwrap();
        let log = t.path().join("s.log");
        activate_mode(&log, t.path(), "plan").unwrap();
        assert!(log.exists());
    }
}

//! User-defined **mode** overlays: `<gait_dir>/modes/<name>/MODE.yaml` (parallel
//! to `skills/<name>/SKILL.md`). A mode is a `{allow, deny, model?, steer}` lens
//! folded from the log as a replay-complete `Mode` event. `default` is reserved
//! (the no-op baseline; never read from disk). See
//! docs/plans/2026-07-08-mode-overlay-design.md.

use crate::proto::gait::log::v1 as pb;
use std::path::Path;

#[derive(serde::Deserialize, Default)]
struct ModeFile {
    #[serde(default)]
    allow: Vec<String>,
    #[serde(default)]
    deny: Vec<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    steer: String,
}

/// Resolve `modes/<name>/MODE.yaml` into a replay-complete `pb::Mode`.
pub fn load_mode(gait_dir: &Path, name: &str) -> anyhow::Result<pb::Mode> {
    let path = gait_dir.join("modes").join(name).join("MODE.yaml");
    let text = std::fs::read_to_string(&path)
        .map_err(|e| anyhow::anyhow!("no such mode '{name}' ({}): {e}", path.display()))?;
    let f: ModeFile = serde_yaml::from_str(&text)
        .map_err(|e| anyhow::anyhow!("mode '{name}' is malformed: {e}"))?;
    Ok(pb::Mode {
        name: name.to_string(),
        permissions: Some(pb::PermissionPolicy {
            allow: f.allow,
            deny: f.deny,
        }),
        model: f.model,
        steer: f.steer,
    })
}

/// List authored mode names (`modes/*/MODE.yaml`), excluding the reserved
/// `default`. Sorted for stable display.
pub fn list_modes(gait_dir: &Path) -> Vec<String> {
    let mut out = vec![];
    if let Ok(rd) = std::fs::read_dir(gait_dir.join("modes")) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name != "default" && e.path().join("MODE.yaml").is_file() {
                out.push(name);
            }
        }
    }
    out.sort();
    out
}

/// Append a `Mode` event to the session log. `name == "" | "default"` appends the
/// empty clear sentinel (back to baseline); any other name is resolved from disk.
pub fn activate_mode(log_path: &Path, gait_dir: &Path, name: &str) -> anyhow::Result<()> {
    let body = if name.is_empty() || name == "default" {
        crate::proto::events::mode("", vec![], vec![], None, "")
    } else {
        let m = load_mode(gait_dir, name)?;
        let p = m.permissions.unwrap_or_default();
        crate::proto::events::mode(&m.name, p.allow, p.deny, m.model, &m.steer)
    };
    crate::proto::wire::append(log_path, body)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_mode(dir: &Path, name: &str, body: &str) {
        let d = dir.join("modes").join(name);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("MODE.yaml"), body).unwrap();
    }

    #[test]
    fn loads_full_mode() {
        let t = TempDir::new().unwrap();
        write_mode(
            t.path(),
            "plan",
            "deny: [write, edit]\nmodel: anthropic/x\nsteer: no edits\n",
        );
        let m = load_mode(t.path(), "plan").unwrap();
        assert_eq!(m.name, "plan");
        assert_eq!(m.permissions.unwrap().deny, vec!["write", "edit"]);
        assert_eq!(m.model.as_deref(), Some("anthropic/x"));
        assert_eq!(m.steer, "no edits");
    }

    #[test]
    fn missing_and_malformed_are_clean_errors() {
        let t = TempDir::new().unwrap();
        assert!(load_mode(t.path(), "nope").is_err());
        write_mode(t.path(), "bad", "deny: [unclosed\n");
        assert!(load_mode(t.path(), "bad").is_err());
    }

    #[test]
    fn default_clears_without_disk() {
        let t = TempDir::new().unwrap();
        // no modes/default on disk; activating "default" must still succeed
        let log = t.path().join("s.log");
        std::fs::write(&log, b"").unwrap();
        activate_mode(&log, t.path(), "default").unwrap();
    }
}

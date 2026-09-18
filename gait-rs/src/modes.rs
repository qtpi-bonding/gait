//! User-defined **mode** overlays: `<gait_dir>/modes/<name>/MODE.yaml` (parallel
//! to `skills/<name>/SKILL.md`). A mode is a `{deny, steer}` safety lens
//! folded from the log as a replay-complete `Mode` event. `default` is reserved
//! (the no-op baseline; never read from disk). See
//! docs/plans/2026-07-08-mode-overlay-design.md.

use crate::proto::gait::log::v1 as pb;
use std::path::Path;

#[derive(serde::Deserialize, Default)]
struct ModeFile {
    #[serde(default)]
    deny: Vec<String>,
    #[serde(default)]
    allow: Vec<String>,
    #[serde(default)]
    steer: String,
    #[serde(default)]
    description: String,
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
        deny: f.deny,
        allow: f.allow,
        steer: f.steer,
        description: f.description,
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
        crate::proto::events::mode("", vec![], vec![], "", "")
    } else {
        let m = load_mode(gait_dir, name)?;
        crate::proto::events::mode(&m.name, m.deny, m.allow, &m.steer, &m.description)
    };
    // Callers may activate a mode before the session's worktree/log directory
    // has otherwise been created (e.g. `--mode` on a brand-new CLI session).
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    crate::proto::wire::append(log_path, body)?;
    Ok(())
}

/// Append a pattern to a mode's allow list, creating the mode file if needed.
pub fn append_mode_allow(gait_dir: &Path, mode_name: &str, pattern: &str) -> anyhow::Result<()> {
    let dir = gait_dir.join("modes").join(mode_name);
    let path = dir.join("MODE.yaml");
    let mut f: ModeFile = if path.exists() {
        let text = std::fs::read_to_string(&path)?;
        serde_yaml::from_str(&text)
            .map_err(|e| anyhow::anyhow!("mode '{mode_name}' is malformed: {e}"))?
    } else {
        ModeFile::default()
    };
    if !f.allow.iter().any(|p| p == pattern) {
        f.allow.push(pattern.to_string());
    }
    std::fs::create_dir_all(&dir)?;
    let out = serde_yaml::to_string(&serde_yaml::Value::Mapping({
        let mut m = serde_yaml::Mapping::new();
        m.insert("deny".into(), serde_yaml::to_value(&f.deny)?);
        m.insert("allow".into(), serde_yaml::to_value(&f.allow)?);
        m.insert("steer".into(), serde_yaml::Value::String(f.steer.clone()));
        m.insert("description".into(), serde_yaml::Value::String(f.description.clone()));
        m
    }))?;
    std::fs::write(&path, out)?;
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
        write_mode(t.path(), "plan", "deny: [write, edit]\nsteer: no edits\n");
        let m = load_mode(t.path(), "plan").unwrap();
        assert_eq!(m.name, "plan");
        assert_eq!(m.deny, vec!["write", "edit"]);
        assert_eq!(m.steer, "no edits");
    }

    #[test]
    fn loads_allow_field() {
        let t = TempDir::new().unwrap();
        write_mode(t.path(), "write", "allow: [read, write, edit]\ndeny: [\"shell:rm *\"]\n");
        let m = load_mode(t.path(), "write").unwrap();
        assert_eq!(m.allow, vec!["read", "write", "edit"]);
        assert_eq!(m.deny, vec!["shell:rm *"]);
    }

    #[test]
    fn allow_defaults_empty_when_absent() {
        let t = TempDir::new().unwrap();
        write_mode(t.path(), "bare", "deny: [write]\n");
        assert!(load_mode(t.path(), "bare").unwrap().allow.is_empty());
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

    #[test]
    fn loads_description() {
        let t = TempDir::new().unwrap();
        write_mode(
            t.path(),
            "plan",
            "deny: [write]\ndescription: Plan only — no edits\nsteer: no edits\n",
        );
        let m = load_mode(t.path(), "plan").unwrap();
        assert_eq!(m.description, "Plan only — no edits");
    }

    #[test]
    fn description_defaults_empty_when_absent() {
        let t = TempDir::new().unwrap();
        write_mode(t.path(), "bare", "deny: [write]\n");
        assert_eq!(load_mode(t.path(), "bare").unwrap().description, "");
    }

    #[test]
    fn append_mode_allow_creates_and_dedups() {
        let t = TempDir::new().unwrap();
        append_mode_allow(t.path(), "write", "shell:cargo *").unwrap();
        append_mode_allow(t.path(), "write", "shell:cargo *").unwrap();
        append_mode_allow(t.path(), "write", "read").unwrap();
        let m = load_mode(t.path(), "write").unwrap();
        assert_eq!(m.allow, vec!["shell:cargo *", "read"]);
    }
}

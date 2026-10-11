//! User-defined **mode** overlays: `<gait_dir>/modes/<name>/MODE.yaml` (parallel
//! to `<gait_dir>/inferences/<name>/inference.textproto` — see `inferences.rs`, whose
//! shape this file mirrors). A mode is a `{allow, deny, steer, description}`
//! safety lens. `default` is reserved (the no-op baseline; never read from
//! disk, never a valid name to author over).
//!
//! Log-write activation (`activate_mode`, appending a `Mode` event directly
//! to a session's transcript at `--mode` time on `run`) stays in the
//! top-level crate's `src/modes.rs` — that's a session-log concern, not file
//! I/O, and lives alongside the top-level crate's other session-log writers.
//! Everything here is pure file I/O plus `resolve_selection`, which only
//! *builds* an event body (mirrors `inferences::load_inference` /
//! `gait-ops::inference::resolve_selection`) — it does not append anything.

use crate::proto::gait::log::v1 as pb;
use std::path::Path;

#[derive(serde::Deserialize, Default)]
pub struct ModeFile {
    #[serde(default)]
    pub allow: Vec<String>,
    #[serde(default)]
    pub deny: Vec<String>,
    #[serde(default)]
    pub steer: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub classify_policy: Option<ClassifyPolicyArgs>,
}

#[derive(serde::Deserialize, serde::Serialize, Clone, Debug, Default)]
pub struct ClassifyPolicyArgs {
    #[serde(default)]
    pub inference: String,
    #[serde(default)]
    pub risk_threshold: f64,
    #[serde(default)]
    pub max_consecutive_denials: u32,
}

impl From<&ClassifyPolicyArgs> for pb::ClassifyPolicy {
    fn from(a: &ClassifyPolicyArgs) -> Self {
        pb::ClassifyPolicy {
            inference: a.inference.clone(),
            risk_threshold: a.risk_threshold,
            max_consecutive_denials: a.max_consecutive_denials,
        }
    }
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
        allow: f.allow,
        deny: f.deny,
        steer: f.steer,
        description: f.description,
        classify_policy: f.classify_policy.as_ref().map(pb::ClassifyPolicy::from),
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

/// Author a new mode at `<gait_dir>/modes/<name>/MODE.yaml`. Refuses
/// overwrite (mirrors `inferences::write_inference`) and refuses the
/// reserved name `default`.
pub fn write_mode(
    gait_dir: &Path,
    name: &str,
    allow: &[String],
    deny: &[String],
    steer: &str,
    description: &str,
    classify_policy: Option<ClassifyPolicyArgs>,
) -> anyhow::Result<()> {
    if name.is_empty() || name == "default" {
        anyhow::bail!("'{name}' is not a valid mode name (reserved)");
    }
    let mode_dir = gait_dir.join("modes").join(name);
    let mode_file = mode_dir.join("MODE.yaml");
    if mode_file.exists() {
        anyhow::bail!(
            "mode '{name}' already exists at {}; pick a new name or `rm -rf {}` first",
            mode_file.display(),
            mode_dir.display()
        );
    }
    std::fs::create_dir_all(&mode_dir)?;
    let f = ModeFile {
        allow: allow.to_vec(),
        deny: deny.to_vec(),
        steer: steer.to_string(),
        description: description.to_string(),
        classify_policy,
    };
    std::fs::write(
        &mode_file,
        serde_yaml::to_string(&SerializableModeFile::from(&f))?,
    )
    .map_err(|e| anyhow::anyhow!("write {}: {e}", mode_file.display()))?;
    Ok(())
}

/// Append a pattern to `modes/<name>/MODE.yaml`'s `allow` list, creating the
/// mode file if needed. Dedups: a pattern already present is not added
/// twice. Preserves `deny`/`steer`/`description`.
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
    std::fs::write(
        &path,
        serde_yaml::to_string(&SerializableModeFile::from(&f))?,
    )?;
    Ok(())
}

/// The empty/clear sentinel `pb::Mode` — mirrors `inferences::clear_inference`.
pub fn clear_mode() -> pb::Mode {
    pb::Mode {
        name: String::new(),
        allow: vec![],
        deny: vec![],
        steer: String::new(),
        description: String::new(),
        classify_policy: None,
    }
}

/// A resolved mode selection's info (for CLI/API display) — mirrors
/// `gait-ops::inference::InferenceInfo`.
#[derive(Debug, Clone)]
pub struct ModeInfo {
    pub name: String,
    pub allow: Vec<String>,
    pub deny: Vec<String>,
    pub steer: String,
    pub description: String,
    pub classify_policy: Option<pb::ClassifyPolicy>,
}

/// A fully resolved mode selection, ready for a caller to append as a
/// session event — mirrors `gait-ops::inference::ResolvedInferenceSelection`.
/// This function does not write anything; it only builds the event body.
#[derive(Debug, Clone)]
pub struct ResolvedModeSelection {
    pub info: ModeInfo,
    pub mode: pb::Mode,
    pub event_body: pb::event::Body,
}

pub fn resolve_selection(gait_dir: &Path, name: &str) -> anyhow::Result<ResolvedModeSelection> {
    let mode = if name == "default" {
        clear_mode()
    } else {
        load_mode(gait_dir, name)?
    };
    let info = ModeInfo {
        name: if mode.name.is_empty() {
            "default".to_string()
        } else {
            mode.name.clone()
        },
        allow: mode.allow.clone(),
        deny: mode.deny.clone(),
        steer: mode.steer.clone(),
        description: mode.description.clone(),
        classify_policy: mode.classify_policy.clone(),
    };
    Ok(ResolvedModeSelection {
        info,
        event_body: crate::proto::events::mode(
            &mode.name,
            mode.deny.clone(),
            mode.allow.clone(),
            &mode.steer,
            &mode.description,
            mode.classify_policy.clone(),
        ),
        mode,
    })
}

/// Serde helper so `write_mode`/`append_mode_allow` emit a stable field
/// order (`allow`, `deny`, `steer`, `description`) regardless of struct
/// declaration order — `serde_yaml::to_string` on `ModeFile` directly would
/// work too, but an explicit intermediate keeps the on-disk shape a
/// deliberate choice, not an accident of field order.
#[derive(serde::Serialize)]
struct SerializableModeFile {
    allow: Vec<String>,
    deny: Vec<String>,
    steer: String,
    description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    classify_policy: Option<ClassifyPolicyArgs>,
}

impl From<&ModeFile> for SerializableModeFile {
    fn from(f: &ModeFile) -> Self {
        Self {
            allow: f.allow.clone(),
            deny: f.deny.clone(),
            steer: f.steer.clone(),
            description: f.description.clone(),
            classify_policy: f.classify_policy.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_mode_file(dir: &std::path::Path, name: &str, body: &str) {
        let d = dir.join("modes").join(name);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("MODE.yaml"), body).unwrap();
    }

    #[test]
    fn loads_full_mode() {
        let t = TempDir::new().unwrap();
        write_mode_file(
            t.path(),
            "plan",
            "allow: [read]\ndeny: [write]\nsteer: no edits\n",
        );
        let m = load_mode(t.path(), "plan").unwrap();
        assert_eq!(m.name, "plan");
        assert_eq!(m.allow, vec!["read"]);
        assert_eq!(m.deny, vec!["write"]);
        assert_eq!(m.steer, "no edits");
    }

    #[test]
    fn missing_and_malformed_are_clean_errors() {
        let t = TempDir::new().unwrap();
        assert!(load_mode(t.path(), "nope").is_err());
        write_mode_file(t.path(), "bad", "deny: [unclosed\n");
        assert!(load_mode(t.path(), "bad").is_err());
    }

    #[test]
    fn list_modes_excludes_default_and_requires_mode_yaml() {
        let t = TempDir::new().unwrap();
        write_mode_file(t.path(), "plan", "deny: [write]\n");
        write_mode_file(t.path(), "write", "allow: [write]\n");
        std::fs::create_dir_all(t.path().join("modes").join("default")).unwrap(); // no MODE.yaml
        std::fs::create_dir_all(t.path().join("modes").join("empty-dir")).unwrap(); // no MODE.yaml
        let names = list_modes(t.path());
        assert_eq!(names, vec!["plan".to_string(), "write".to_string()]);
    }

    #[test]
    fn write_mode_then_load_round_trips() {
        let t = TempDir::new().unwrap();
        write_mode(
            t.path(),
            "review",
            &["read".to_string()],
            &["shell:rm *".to_string()],
            "review only, no writes",
            "Review mode",
            None,
        )
        .unwrap();
        let m = load_mode(t.path(), "review").unwrap();
        assert_eq!(m.name, "review");
        assert_eq!(m.allow, vec!["read"]);
        assert_eq!(m.deny, vec!["shell:rm *"]);
        assert_eq!(m.steer, "review only, no writes");
        assert_eq!(m.description, "Review mode");
    }

    #[test]
    fn write_mode_refuses_overwrite() {
        let t = TempDir::new().unwrap();
        write_mode(t.path(), "p", &[], &[], "", "", None).unwrap();
        let err = write_mode(t.path(), "p", &["read".into()], &[], "", "", None).unwrap_err();
        assert!(format!("{err}").contains("already exists"), "got: {err}");
    }

    #[test]
    fn write_mode_reserved_name_default_is_rejected() {
        let t = TempDir::new().unwrap();
        let err = write_mode(t.path(), "default", &[], &[], "", "", None).unwrap_err();
        assert!(format!("{err}").contains("reserved"), "got: {err}");
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

    #[test]
    fn clear_mode_is_the_empty_sentinel() {
        let m = clear_mode();
        assert_eq!(m.name, "");
        assert!(m.allow.is_empty());
        assert!(m.deny.is_empty());
    }

    #[test]
    fn resolve_selection_default_clears() {
        let t = TempDir::new().unwrap();
        let sel = resolve_selection(t.path(), "default").unwrap();
        assert_eq!(sel.info.name, "default");
        assert!(sel.info.allow.is_empty());
    }

    #[test]
    fn resolve_selection_loads_named_mode() {
        let t = TempDir::new().unwrap();
        write_mode(t.path(), "plan", &[], &["write".into()], "", "", None).unwrap();
        let sel = resolve_selection(t.path(), "plan").unwrap();
        assert_eq!(sel.info.name, "plan");
        assert_eq!(sel.info.deny, vec!["write"]);
    }

    #[test]
    fn loads_classify_policy() {
        let t = TempDir::new().unwrap();
        write_mode_file(
            t.path(),
            "fast-review",
            "allow: [read]\nclassify_policy:\n  inference: jev-fast\n  risk_threshold: 0.3\n  max_consecutive_denials: 3\n",
        );
        let m = load_mode(t.path(), "fast-review").unwrap();
        let cp = m.classify_policy.expect("classify_policy must be Some");
        assert_eq!(cp.inference, "jev-fast");
        assert!((cp.risk_threshold - 0.3).abs() < 1e-9);
        assert_eq!(cp.max_consecutive_denials, 3);
    }

    #[test]
    fn classify_policy_defaults_to_none_when_absent() {
        let t = TempDir::new().unwrap();
        write_mode_file(t.path(), "bare", "deny: [write]\n");
        assert!(load_mode(t.path(), "bare")
            .unwrap()
            .classify_policy
            .is_none());
    }

    #[test]
    fn write_mode_with_classify_policy_round_trips() {
        let t = TempDir::new().unwrap();
        write_mode(
            t.path(),
            "fast-review",
            &["read".to_string()],
            &[],
            "",
            "",
            Some(ClassifyPolicyArgs {
                inference: "jev-fast".to_string(),
                risk_threshold: 0.3,
                max_consecutive_denials: 3,
            }),
        )
        .unwrap();
        let m = load_mode(t.path(), "fast-review").unwrap();
        let cp = m.classify_policy.expect("classify_policy must be Some");
        assert_eq!(cp.inference, "jev-fast");
    }
}

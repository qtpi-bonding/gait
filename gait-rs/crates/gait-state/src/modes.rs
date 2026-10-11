//! User-defined mode overlays stored in `<gait_dir>/modes/<name>/MODE.md`.
//!
//! A mode's safety lens consists of `{allow, deny, steer, description}`. The
//! name `default` is reserved for the empty/clear mode. `MODE.md` has a YAML
//! frontmatter plus markdown body shape: an opening `---` line, YAML fields,
//! a closing `---` line, and the markdown body containing the steer.
use crate::proto::gait::log::v1 as pb;
use std::path::Path;

/// Validate that a mode name is one safe, normal path component.
fn validate_name(name: &str) -> anyhow::Result<()> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.contains('/')
        || name.contains('\\')
        || name.contains('\0')
    {
        anyhow::bail!("mode name '{name}' is invalid: must be a single normal path component");
    }
    Ok(())
}

/// Split a MODE.md into YAML frontmatter and its markdown body.
/// Split a MODE.md at the first subsequent whole-line `---` fence.
fn split_frontmatter<'a>(text: &'a str, name: &str) -> anyhow::Result<(&'a str, &'a str)> {
    let first_end = text.find('\n').unwrap_or(text.len());
    let first = text[..first_end]
        .strip_suffix('\r')
        .unwrap_or(&text[..first_end]);
    if first != "---" {
        anyhow::bail!("mode '{name}' is malformed: missing opening fence");
    }
    let mut start = first_end;
    if start < text.len() {
        start += 1;
    }
    let mut pos = start;
    while pos <= text.len() {
        let end = text[pos..]
            .find('\n')
            .map(|n| pos + n)
            .unwrap_or(text.len());
        let line = text[pos..end].strip_suffix('\r').unwrap_or(&text[pos..end]);
        if line == "---" {
            let body_start = if end < text.len() { end + 1 } else { end };
            return Ok((&text[start..pos], &text[body_start..]));
        }
        if end == text.len() {
            break;
        }
        pos = end + 1;
    }
    anyhow::bail!("mode '{name}' is malformed: unclosed frontmatter fence")
}

/// Parse and validate MODE.md YAML frontmatter into a mode.
fn parse_frontmatter(yaml: &str, name: &str) -> anyhow::Result<pb::Mode> {
    let value: serde_yaml::Value = serde_yaml::from_str(yaml)
        .map_err(|e| anyhow::anyhow!("mode '{name}' is malformed: {e}"))?;
    let mapping = match value {
        serde_yaml::Value::Null => serde_yaml::Mapping::new(),
        value => value.as_mapping().cloned().ok_or_else(|| {
            anyhow::anyhow!("mode '{name}' is malformed: frontmatter must be a mapping")
        })?,
    };
    for key in mapping.keys() {
        let key = key.as_str().unwrap_or("");
        if key == "steer" {
            anyhow::bail!("mode '{name}' is malformed: the steer is the markdown body");
        }
        if !matches!(
            key,
            "name" | "allow" | "deny" | "description" | "classify_policy"
        ) {
            anyhow::bail!("mode '{name}' is malformed: unknown frontmatter key '{key}'");
        }
    }
    let json = serde_json::to_value(&mapping)
        .map_err(|e| anyhow::anyhow!("mode '{name}' is malformed: {e}"))?;
    let mut mode: pb::Mode = serde_json::from_value(json)
        .map_err(|e| anyhow::anyhow!("mode '{name}' is malformed: {e}"))?;
    if !mode.name.is_empty() && mode.name != name {
        anyhow::bail!("mode '{name}' declares name '{}'", mode.name);
    }
    mode.name = name.to_string();
    Ok(mode)
}

fn read_parts(text: &str, name: &str) -> anyhow::Result<(pb::Mode, String)> {
    let (yaml, body) = split_frontmatter(text, name)?;
    Ok((parse_frontmatter(yaml, name)?, body.to_string()))
}

/// Resolve `modes/<name>/MODE.md` into a replay-complete `pb::Mode`.
pub fn load_mode(gait_dir: &Path, name: &str) -> anyhow::Result<pb::Mode> {
    validate_name(name)?;
    let dir = gait_dir.join("modes").join(name);
    let path = dir.join("MODE.md");
    if !path.is_file() {
        let old = dir.join("MODE.yaml");
        if old.is_file() {
            anyhow::bail!("mode '{name}' uses the removed MODE.yaml format; rewrite it as MODE.md (frontmatter: allow/deny/description/classify_policy; body: the steer)");
        }
    }
    let text = std::fs::read_to_string(&path)
        .map_err(|e| anyhow::anyhow!("no such mode '{name}' ({}): {e}", path.display()))?;
    let (mut mode, body) = read_parts(&text, name)?;
    // One trailing newline ends the last line of the body; it is not part of the steer.
    mode.steer = body
        .strip_suffix("\r\n")
        .or_else(|| body.strip_suffix('\n'))
        .unwrap_or(&body)
        .to_string();
    Ok(mode)
}

/// List authored mode names (`modes/*/MODE.md`), excluding `default`.
pub fn list_modes(gait_dir: &Path) -> Vec<String> {
    let mut out = vec![];
    if let Ok(rd) = std::fs::read_dir(gait_dir.join("modes")) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name != "default" && e.path().join("MODE.md").is_file() {
                out.push(name);
            }
        }
    }
    out.sort();
    out
}

fn yaml_value<T: serde::Serialize>(value: &T) -> serde_yaml::Value {
    serde_yaml::to_value(value).expect("serializable protobuf field")
}

fn mode_frontmatter(mode: &pb::Mode) -> serde_yaml::Mapping {
    let mut m = serde_yaml::Mapping::new();
    if !mode.description.is_empty() {
        m.insert(
            serde_yaml::Value::String("description".into()),
            yaml_value(&mode.description),
        );
    }
    if !mode.allow.is_empty() {
        m.insert(
            serde_yaml::Value::String("allow".into()),
            yaml_value(&mode.allow),
        );
    }
    if !mode.deny.is_empty() {
        m.insert(
            serde_yaml::Value::String("deny".into()),
            yaml_value(&mode.deny),
        );
    }
    if let Some(ref policy) = mode.classify_policy {
        m.insert(
            serde_yaml::Value::String("classify_policy".into()),
            yaml_value(policy),
        );
    }
    m
}

/// Join YAML frontmatter and a markdown body into MODE.md text.
fn join_frontmatter(mapping: &serde_yaml::Mapping, body: &str) -> anyhow::Result<String> {
    let mut out = String::from("---\n");
    if !mapping.is_empty() {
        let yaml = serde_yaml::to_string(mapping)?;
        out.push_str(yaml.trim_end_matches('\n'));
        out.push('\n');
    }
    out.push_str("---\n");
    out.push_str(body);
    Ok(out)
}

/// Author a new mode as MODE.md. A trailing newline on steer is not significant.
pub fn write_mode(
    gait_dir: &Path,
    name: &str,
    allow: &[String],
    deny: &[String],
    steer: &str,
    description: &str,
    classify_policy: Option<pb::ClassifyPolicy>,
) -> anyhow::Result<()> {
    validate_name(name)?;
    if name == "default" {
        anyhow::bail!("'{name}' is not a valid mode name (reserved)");
    }
    let dir = gait_dir.join("modes").join(name);
    let path = dir.join("MODE.md");
    if path.exists() {
        anyhow::bail!(
            "mode '{name}' already exists at {}; pick a new name or `rm -rf {}` first",
            path.display(),
            dir.display()
        );
    }
    let mut body = steer.to_string();
    if !body.is_empty() && !body.ends_with('\n') {
        body.push('\n');
    }
    let mode = pb::Mode {
        name: name.into(),
        allow: allow.to_vec(),
        deny: deny.to_vec(),
        steer: String::new(),
        description: description.into(),
        classify_policy,
    };
    std::fs::create_dir_all(&dir)?;
    std::fs::write(&path, join_frontmatter(&mode_frontmatter(&mode), &body)?)
        .map_err(|e| anyhow::anyhow!("write {}: {e}", path.display()))?;
    Ok(())
}

/// Append a pattern to MODE.md's allow list, preserving the body byte-for-byte.
pub fn append_mode_allow(gait_dir: &Path, mode_name: &str, pattern: &str) -> anyhow::Result<()> {
    validate_name(mode_name)?;
    let dir = gait_dir.join("modes").join(mode_name);
    let path = dir.join("MODE.md");
    let (mut mapping, body) = if path.exists() {
        let text = std::fs::read_to_string(&path)?;
        let (yaml, body) = split_frontmatter(&text, mode_name)?;
        let value: serde_yaml::Value = serde_yaml::from_str(yaml)
            .map_err(|e| anyhow::anyhow!("mode '{mode_name}' is malformed: {e}"))?;
        (
            value.as_mapping().cloned().ok_or_else(|| {
                anyhow::anyhow!("mode '{mode_name}' is malformed: frontmatter must be a mapping")
            })?,
            body.to_string(),
        )
    } else {
        (serde_yaml::Mapping::new(), String::new())
    };
    let key = serde_yaml::Value::String("allow".into());
    let list = mapping
        .entry(key)
        .or_insert_with(|| serde_yaml::Value::Sequence(vec![]));
    let seq = list
        .as_sequence_mut()
        .ok_or_else(|| anyhow::anyhow!("mode '{mode_name}' is malformed: allow must be a list"))?;
    if !seq.iter().any(|v| v.as_str() == Some(pattern)) {
        seq.push(serde_yaml::Value::String(pattern.into()));
    }
    std::fs::create_dir_all(&dir)?;
    std::fs::write(&path, join_frontmatter(&mapping, &body)?)?;
    Ok(())
}

/// Return the empty/clear sentinel `pb::Mode`.
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

#[derive(Debug, Clone)]
/// Summary information for a resolved mode.
pub struct ModeInfo {
    pub name: String,
    pub allow: Vec<String>,
    pub deny: Vec<String>,
    pub steer: String,
    pub description: String,
    pub classify_policy: Option<pb::ClassifyPolicy>,
}
#[derive(Debug, Clone)]
/// A selected mode and the event body emitted for that selection.
pub struct ResolvedModeSelection {
    pub info: ModeInfo,
    pub mode: pb::Mode,
    pub event_body: pb::event::Body,
}
/// Resolve `default` to the clear sentinel or load a named MODE.md mode.
pub fn resolve_selection(gait_dir: &Path, name: &str) -> anyhow::Result<ResolvedModeSelection> {
    let mode = if name == "default" {
        clear_mode()
    } else {
        load_mode(gait_dir, name)?
    };
    let info = ModeInfo {
        name: if mode.name.is_empty() {
            "default".into()
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn put(t: &TempDir, n: &str, s: &str) {
        let d = t.path().join("modes").join(n);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("MODE.md"), s).unwrap();
    }

    fn err<T: std::fmt::Debug>(r: anyhow::Result<T>) -> String {
        r.unwrap_err().to_string()
    }

    #[test]
    fn loads_full_mode() {
        let t = TempDir::new().unwrap();
        put(
            &t,
            "plan",
            "---\nallow: [read]\ndeny: [write]\ndescription: plan\n---\nno edits\n",
        );
        let m = load_mode(t.path(), "plan").unwrap();
        assert_eq!(m.name, "plan");
        assert_eq!(m.allow, vec!["read"]);
        assert_eq!(m.deny, vec!["write"]);
        assert_eq!(m.description, "plan");
        assert_eq!(m.steer, "no edits");
    }

    #[test]
    fn missing_and_malformed_are_clean_errors() {
        let t = TempDir::new().unwrap();
        assert!(err(load_mode(t.path(), "nope")).contains("no such mode"));
        put(&t, "bad", "---\ndeny: [unclosed\n");
        assert!(!err(load_mode(t.path(), "bad")).is_empty());
    }

    #[test]
    fn list_modes_excludes_default_and_requires_mode_file() {
        let t = TempDir::new().unwrap();
        put(&t, "plan", "---\n---\n");
        put(&t, "write", "---\n---\n");
        std::fs::create_dir_all(t.path().join("modes/default")).unwrap();
        std::fs::create_dir_all(t.path().join("modes/empty")).unwrap();
        assert_eq!(list_modes(t.path()), vec!["plan", "write"]);
    }

    #[test]
    fn write_mode_then_load_round_trips() {
        let t = TempDir::new().unwrap();
        write_mode(
            t.path(),
            "review",
            &["read".into()],
            &["rm".into()],
            "review",
            "desc",
            None,
        )
        .unwrap();
        let m = load_mode(t.path(), "review").unwrap();
        assert_eq!(m.allow, vec!["read"]);
        assert_eq!(m.deny, vec!["rm"]);
        assert_eq!(m.steer, "review");
        assert_eq!(m.description, "desc");
    }

    #[test]
    fn write_mode_refuses_overwrite() {
        let t = TempDir::new().unwrap();
        write_mode(t.path(), "p", &[], &[], "", "", None).unwrap();
        assert!(err(write_mode(t.path(), "p", &[], &[], "", "", None)).contains("already exists"));
    }

    #[test]
    fn write_mode_reserved_name_default_is_rejected() {
        let t = TempDir::new().unwrap();
        assert!(err(write_mode(t.path(), "default", &[], &[], "", "", None)).contains("reserved"));
    }

    #[test]
    fn append_mode_allow_creates_and_dedups() {
        let t = TempDir::new().unwrap();
        append_mode_allow(t.path(), "write", "shell:cargo *").unwrap();
        append_mode_allow(t.path(), "write", "shell:cargo *").unwrap();
        append_mode_allow(t.path(), "write", "read").unwrap();
        assert_eq!(
            load_mode(t.path(), "write").unwrap().allow,
            vec!["shell:cargo *", "read"]
        );
    }

    #[test]
    fn clear_mode_is_the_empty_sentinel() {
        let m = clear_mode();
        assert!(m.name.is_empty() && m.allow.is_empty() && m.deny.is_empty() && m.steer.is_empty());
    }

    #[test]
    fn resolve_selection_default_clears() {
        let s = resolve_selection(TempDir::new().unwrap().path(), "default").unwrap();
        assert_eq!(s.info.name, "default");
        assert!(s.mode.name.is_empty());
    }

    #[test]
    fn resolve_selection_loads_named_mode() {
        let t = TempDir::new().unwrap();
        write_mode(t.path(), "plan", &[], &["write".into()], "", "", None).unwrap();
        assert_eq!(
            resolve_selection(t.path(), "plan").unwrap().info.deny,
            vec!["write"]
        );
    }

    #[test]
    fn loads_classify_policy() {
        let t = TempDir::new().unwrap();
        put(&t, "x", "---\nclassify_policy:\n  inference: jev-fast\n  risk_threshold: 0.3\n  max_consecutive_denials: 3\n---\n");
        let p = load_mode(t.path(), "x").unwrap().classify_policy.unwrap();
        assert_eq!(p.inference, "jev-fast");
        assert_eq!(p.max_consecutive_denials, 3);
        assert!((p.risk_threshold - 0.3).abs() < 1e-9);
    }

    #[test]
    fn classify_policy_defaults_to_none_when_absent() {
        let t = TempDir::new().unwrap();
        put(&t, "x", "---\n---\n");
        assert!(load_mode(t.path(), "x").unwrap().classify_policy.is_none());
    }

    #[test]
    fn write_mode_with_classify_policy_round_trips() {
        let t = TempDir::new().unwrap();
        let p = pb::ClassifyPolicy {
            inference: "jev".into(),
            risk_threshold: 0.3,
            max_consecutive_denials: 2,
        };
        write_mode(t.path(), "x", &[], &[], "", "", Some(p.clone())).unwrap();
        assert_eq!(load_mode(t.path(), "x").unwrap().classify_policy, Some(p));
    }

    #[test]
    fn body_containing_fences_round_trips() {
        let t = TempDir::new().unwrap();
        let body = "one\n---\ntwo\n";
        write_mode(t.path(), "x", &[], &[], body, "", None).unwrap();
        assert_eq!(load_mode(t.path(), "x").unwrap().steer, body.trim_end_matches('\n'));
    }

    #[test]
    fn lookalike_fences_are_values() {
        let t = TempDir::new().unwrap();
        put(
            &t,
            "x",
            "---\ndescription: \"---x\"\nallow: [read]\n---\nbody\n",
        );
        let m = load_mode(t.path(), "x").unwrap();
        assert_eq!(m.description, "---x");
        assert_eq!(m.steer, "body");
    }

    #[test]
    fn unclosed_fence_is_reported() {
        let t = TempDir::new().unwrap();
        put(&t, "x", "---\ndescription: x\n");
        assert!(err(load_mode(t.path(), "x")).contains("unclosed"));
    }

    #[test]
    fn missing_opening_fence_is_reported() {
        let t = TempDir::new().unwrap();
        put(&t, "x", "description: x\n---\n");
        assert!(err(load_mode(t.path(), "x")).contains("missing opening fence"));
    }

    #[test]
    fn unknown_key_names_key() {
        let t = TempDir::new().unwrap();
        put(&t, "x", "---\nsecret: x\n---\n");
        assert!(err(load_mode(t.path(), "x")).contains("secret"));
    }

    #[test]
    fn steer_key_must_be_body() {
        let t = TempDir::new().unwrap();
        put(&t, "x", "---\nsteer: x\n---\n");
        assert!(err(load_mode(t.path(), "x")).contains("markdown body"));
    }

    #[test]
    fn declared_name_mismatch_is_reported() {
        let t = TempDir::new().unwrap();
        put(&t, "x", "---\nname: y\n---\n");
        assert!(err(load_mode(t.path(), "x")).contains("declares name"));
    }

    #[test]
    fn stale_yaml_format_is_reported() {
        let t = TempDir::new().unwrap();
        let d = t.path().join("modes/x");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("MODE.yaml"), "allow: []").unwrap();
        assert!(err(load_mode(t.path(), "x")).contains("removed MODE.yaml format"));
    }

    #[test]
    fn invalid_names_are_rejected_everywhere() {
        let t = TempDir::new().unwrap();
        for name in ["..", "a/b", ""] {
            assert!(err(load_mode(t.path(), name)).contains("invalid"));
            assert!(err(write_mode(t.path(), name, &[], &[], "", "", None)).contains("invalid"));
            assert!(err(append_mode_allow(t.path(), name, "x")).contains("invalid"));
        }
    }

    #[test]
    fn append_preserves_frontmatter_and_body() {
        let t = TempDir::new().unwrap();
        let body = "keep\n---\nbytes\n";
        put(&t, "x", "---\ndescription: desc\ndeny: [no]\nclassify_policy:\n  inference: i\n---\nkeep\n---\nbytes\n");
        let before = load_mode(t.path(), "x").unwrap().steer;
        append_mode_allow(t.path(), "x", "yes").unwrap();
        append_mode_allow(t.path(), "x", "yes").unwrap();
        let m = load_mode(t.path(), "x").unwrap();
        assert_eq!(m.steer, body.trim_end_matches('\n'));
        assert_eq!(m.description, "desc");
        assert_eq!(m.deny, vec!["no"]);
        assert_eq!(m.classify_policy.unwrap().inference, "i");
        assert_eq!(m.allow, vec!["yes"]);
        assert_eq!(before, m.steer);
    }

    #[test]
    fn crlf_file_loads() {
        let t = TempDir::new().unwrap();
        put(
            &t,
            "x",
            "---\r\nallow: [read]\r\ndescription: x\r\n---\r\nbody\r\n",
        );
        let m = load_mode(t.path(), "x").unwrap();
        assert_eq!(m.allow, vec!["read"]);
        assert_eq!(m.steer, "body");
    }

    #[test]
    fn empty_write_is_empty_mode() {
        let t = TempDir::new().unwrap();
        write_mode(t.path(), "x", &[], &[], "", "", None).unwrap();
        assert_eq!(
            std::fs::read_to_string(t.path().join("modes/x/MODE.md")).unwrap(),
            "---\n---\n"
        );
        let m = load_mode(t.path(), "x").unwrap();
        assert_eq!(m.name, "x");
        assert!(m.allow.is_empty());
        assert!(m.deny.is_empty());
        assert!(m.steer.is_empty());
        assert!(m.description.is_empty());
        assert!(m.classify_policy.is_none());
    }

    #[test]
    fn empty_steer_has_no_body() {
        let t = TempDir::new().unwrap();
        write_mode(t.path(), "x", &[], &[], "", "", None).unwrap();
        assert_eq!(load_mode(t.path(), "x").unwrap().steer, "");
    }
}

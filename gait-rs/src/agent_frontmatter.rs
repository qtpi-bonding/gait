use std::path::PathBuf;

const FENCE: &str = "---";

/// Parse the YAML frontmatter from an agent .md file and return the allow list.
/// Returns an empty vec if there is no frontmatter or no allow key.
pub fn read_agent_allow(path: PathBuf) -> anyhow::Result<Vec<String>> {
    let content = std::fs::read_to_string(&path).unwrap_or_default();
    let (fm, _body) = split_frontmatter(&content);
    if fm.is_empty() { return Ok(vec![]); }
    let yaml: serde_yaml::Value = serde_yaml::from_str(fm)
        .unwrap_or(serde_yaml::Value::Null);
    Ok(extract_string_list(&yaml, "allow"))
}

/// Parse the YAML frontmatter from an agent .md file and return the deny list.
pub fn read_agent_deny(path: PathBuf) -> anyhow::Result<Vec<String>> {
    let content = std::fs::read_to_string(&path).unwrap_or_default();
    let (fm, _body) = split_frontmatter(&content);
    if fm.is_empty() { return Ok(vec![]); }
    let yaml: serde_yaml::Value = serde_yaml::from_str(fm)
        .unwrap_or(serde_yaml::Value::Null);
    Ok(extract_string_list(&yaml, "deny"))
}

/// Read the optional `model` field from an agent file's YAML frontmatter.
pub fn read_agent_model(path: &std::path::Path) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    let (fm, _) = split_frontmatter(&content);
    if fm.is_empty() { return None; }
    let yaml: serde_yaml::Value = serde_yaml::from_str(fm).ok()?;
    yaml.get("model").and_then(|v| v.as_str()).map(|s| s.to_string())
}

/// Return the markdown body of an agent file (everything after the frontmatter).
/// Returns empty string if the file doesn't exist or has no body.
pub fn read_agent_body(path: &std::path::Path) -> String {
    let content = std::fs::read_to_string(path).unwrap_or_default();
    let (_fm, body) = split_frontmatter(&content);
    body.trim_end().to_string()
}

/// Append a pattern to the allow list in an agent file's YAML frontmatter.
/// Creates frontmatter if none exists. Preserves the prompt body verbatim.
pub fn append_agent_allow(path: PathBuf, pattern: &str) -> anyhow::Result<()> {
    append_to_list(path, "allow", pattern)
}

/// Append a pattern to the deny list in an agent file's YAML frontmatter.
pub fn append_agent_deny(path: PathBuf, pattern: &str) -> anyhow::Result<()> {
    append_to_list(path, "deny", pattern)
}

/// Resolve a short agent name like "code" to its path in the workspace.
/// "code" → "{workspace}/agents/code.md"
pub fn resolve_agent_path(workspace: &std::path::Path, name: &str) -> PathBuf {
    // If name already looks like a path (contains / or .md), use as-is relative to workspace
    if name.contains('/') || name.ends_with(".md") {
        workspace.join(name)
    } else {
        workspace.join("agents").join(format!("{}.md", name))
    }
}

fn append_to_list(path: PathBuf, key: &str, pattern: &str) -> anyhow::Result<()> {
    let content = if path.exists() {
        std::fs::read_to_string(&path)?
    } else {
        String::new()
    };

    let (fm, body) = split_frontmatter(&content);

    let mut yaml: serde_yaml::Value = if fm.is_empty() {
        serde_yaml::Value::Mapping(serde_yaml::Mapping::new())
    } else {
        serde_yaml::from_str(fm).unwrap_or(serde_yaml::Value::Mapping(serde_yaml::Mapping::new()))
    };

    let key_val = serde_yaml::Value::String(key.into());
    if let serde_yaml::Value::Mapping(ref mut map) = yaml {
        let list = map.entry(key_val).or_insert(serde_yaml::Value::Sequence(vec![]));
        if let serde_yaml::Value::Sequence(ref mut seq) = list {
            if !seq.iter().any(|v| v.as_str() == Some(pattern)) {
                seq.push(serde_yaml::Value::String(pattern.to_string()));
            }
        }
    }

    let new_fm = serde_yaml::to_string(&yaml)?;
    std::fs::write(&path, format!("---\n{}---\n{}", new_fm, body))?;
    Ok(())
}

fn split_frontmatter(content: &str) -> (&str, &str) {
    if !content.starts_with(FENCE) {
        return ("", content);
    }
    let rest = &content[FENCE.len()..];
    let rest = rest.strip_prefix('\n').unwrap_or(rest);
    if let Some(end) = rest.find(&format!("\n{}", FENCE)) {
        let fm = &rest[..end];
        let body_start = end + 1 + FENCE.len();
        let body = &rest[body_start..];
        let body = body.strip_prefix('\n').unwrap_or(body);
        (fm, body)
    } else {
        ("", content)
    }
}

fn extract_string_list(yaml: &serde_yaml::Value, key: &str) -> Vec<String> {
    yaml.get(key)
        .and_then(|v| v.as_sequence())
        .map(|seq| seq.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn reads_allow_from_frontmatter() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("agent.md"), "---\nallow:\n  - read\n  - shell:cargo *\n---\n# prompt\n").unwrap();
        let allow = read_agent_allow(dir.path().join("agent.md")).unwrap();
        assert_eq!(allow, vec!["read", "shell:cargo *"]);
    }

    #[test]
    fn reads_deny_from_frontmatter() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("agent.md"), "---\nallow:\n  - read\ndeny:\n  - shell:rm *\n---\n# prompt\n").unwrap();
        let deny = read_agent_deny(dir.path().join("agent.md")).unwrap();
        assert_eq!(deny, vec!["shell:rm *"]);
    }

    #[test]
    fn reads_model_from_frontmatter() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("agent.md"), "---\nmodel: anthropic/claude-opus-4-8\nallow:\n  - read\n---\n# prompt\n").unwrap();
        let model = read_agent_model(&dir.path().join("agent.md"));
        assert_eq!(model, Some("anthropic/claude-opus-4-8".to_string()));
    }

    #[test]
    fn reads_body_after_frontmatter() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("agent.md"), "---\nallow:\n  - read\n---\nYou are a coding assistant.\n").unwrap();
        let body = read_agent_body(&dir.path().join("agent.md"));
        assert_eq!(body, "You are a coding assistant.");
    }

    #[test]
    fn reads_body_with_no_frontmatter() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("agent.md"), "You are a coding assistant.\n").unwrap();
        let body = read_agent_body(&dir.path().join("agent.md"));
        assert_eq!(body, "You are a coding assistant.");
    }

    #[test]
    fn returns_empty_body_when_file_missing() {
        let dir = TempDir::new().unwrap();
        let body = read_agent_body(&dir.path().join("missing.md"));
        assert!(body.is_empty());
    }

    #[test]
    fn returns_empty_when_no_frontmatter() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("agent.md"), "# just a prompt\n").unwrap();
        let allow = read_agent_allow(dir.path().join("agent.md")).unwrap();
        assert!(allow.is_empty());
    }

    #[test]
    fn appends_to_agent_allow() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("agent.md"), "---\nallow:\n  - read\n---\n# prompt\n").unwrap();
        append_agent_allow(dir.path().join("agent.md"), "shell:git *").unwrap();
        let allow = read_agent_allow(dir.path().join("agent.md")).unwrap();
        assert!(allow.contains(&"shell:git *".to_string()));
        let raw = std::fs::read_to_string(dir.path().join("agent.md")).unwrap();
        assert!(raw.contains("# prompt"));
    }

    #[test]
    fn appends_to_agent_deny() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("agent.md"), "---\nallow:\n  - read\n---\n# prompt\n").unwrap();
        append_agent_deny(dir.path().join("agent.md"), "shell:rm *").unwrap();
        let deny = read_agent_deny(dir.path().join("agent.md")).unwrap();
        assert!(deny.contains(&"shell:rm *".to_string()));
        let raw = std::fs::read_to_string(dir.path().join("agent.md")).unwrap();
        assert!(raw.contains("# prompt"));
    }

    #[test]
    fn creates_frontmatter_when_none_exists() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("agent.md"), "# just a prompt\n").unwrap();
        append_agent_allow(dir.path().join("agent.md"), "shell:cargo *").unwrap();
        let allow = read_agent_allow(dir.path().join("agent.md")).unwrap();
        assert!(allow.contains(&"shell:cargo *".to_string()));
        assert!(std::fs::read_to_string(dir.path().join("agent.md")).unwrap().contains("# just a prompt"));
    }

    #[test]
    fn agent_allow_deduplicates() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("agent.md"), "---\nallow:\n  - shell:cargo *\n---\n# p\n").unwrap();
        append_agent_allow(dir.path().join("agent.md"), "shell:cargo *").unwrap();
        assert_eq!(read_agent_allow(dir.path().join("agent.md")).unwrap()
            .iter().filter(|p| p.as_str() == "shell:cargo *").count(), 1);
    }

    #[test]
    fn resolve_agent_path_appends_md() {
        let dir = TempDir::new().unwrap();
        let path = resolve_agent_path(dir.path(), "coder");
        assert_eq!(path, dir.path().join("agents/coder.md"));
    }
}

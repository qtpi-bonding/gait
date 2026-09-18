use std::path::PathBuf;

const FENCE: &str = "---";

/// Parse the YAML frontmatter from an agent .md file and return the `skills`
/// whitelist. Returns an empty vec if there is no frontmatter or no `skills`
/// key (default-deny: an agent with no field sees no global skills).
pub fn read_agent_skills(path: PathBuf) -> anyhow::Result<Vec<String>> {
    let content = std::fs::read_to_string(&path).unwrap_or_default();
    let (fm, _body) = split_frontmatter(&content);
    if fm.is_empty() {
        return Ok(vec![]);
    }
    let yaml: serde_yaml::Value = serde_yaml::from_str(fm).unwrap_or(serde_yaml::Value::Null);
    Ok(extract_string_list(&yaml, "skills"))
}

/// Parse the YAML frontmatter from an agent .md file and return the `mcp`
/// whitelist. Returns an empty vec if there is no frontmatter or no `mcp` key.
pub fn read_agent_mcp(path: PathBuf) -> anyhow::Result<Vec<String>> {
    let content = std::fs::read_to_string(&path).unwrap_or_default();
    let (fm, _body) = split_frontmatter(&content);
    if fm.is_empty() {
        return Ok(vec![]);
    }
    let yaml: serde_yaml::Value = serde_yaml::from_str(fm).unwrap_or(serde_yaml::Value::Null);
    Ok(extract_string_list(&yaml, "mcp"))
}

/// Parse the YAML frontmatter from an agent .md file and return the `plugins`
/// whitelist. Returns an empty vec if there is no frontmatter or no `plugins`
/// key.
pub fn read_agent_plugins(path: PathBuf) -> anyhow::Result<Vec<String>> {
    let content = std::fs::read_to_string(&path).unwrap_or_default();
    let (fm, _body) = split_frontmatter(&content);
    if fm.is_empty() {
        return Ok(vec![]);
    }
    let yaml: serde_yaml::Value = serde_yaml::from_str(fm).unwrap_or(serde_yaml::Value::Null);
    Ok(extract_string_list(&yaml, "plugins"))
}

/// Read the optional named inference reference from an agent's frontmatter.
pub fn read_agent_inference(path: &std::path::Path) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    let (fm, _) = split_frontmatter(&content);
    if fm.is_empty() {
        return None;
    }
    let yaml: serde_yaml::Value = serde_yaml::from_str(fm).ok()?;
    yaml.get("inference")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// Return the markdown body of an agent file (everything after the frontmatter).
/// Returns empty string if the file doesn't exist or has no body.
pub fn read_agent_body(path: &std::path::Path) -> String {
    let content = std::fs::read_to_string(path).unwrap_or_default();
    let (_fm, body) = split_frontmatter(&content);
    body.trim_end().to_string()
}

/// Which kind of named markdown context file to resolve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkdownKind {
    /// `agents/<name>/AGENT.md` — role/identity file for a named agent.
    Agent,
    /// `codebases/<key>/PROJECT.md` — per-codebase project instructions.
    Project,
    /// `skills/<name>/SKILL.md` — a loadable skill body.
    Skill,
}

/// Resolve a named markdown context file to its canonical directory-form path.
///
/// - `Agent`   → `<gait_dir>/agents/<name>/AGENT.md`
/// - `Project` → `<gait_dir>/codebases/<name>/PROJECT.md`
/// - `Skill`   → `<gait_dir>/skills/<name>/SKILL.md`
///
/// `USER.md` is a singleton (`<gait_dir>/USER.md`) — resolve it directly, not here.
pub fn resolve_markdown_context_path(
    gait_dir: &std::path::Path,
    kind: MarkdownKind,
    name: &str,
) -> PathBuf {
    match kind {
        MarkdownKind::Agent => gait_dir.join("agents").join(name).join("AGENT.md"),
        MarkdownKind::Project => gait_dir.join("codebases").join(name).join("PROJECT.md"),
        MarkdownKind::Skill => gait_dir.join("skills").join(name).join("SKILL.md"),
    }
}

/// Resolve a short agent name like "code" to its path in the gait_dir.
/// Kept for call sites not yet migrated to `resolve_markdown_context_path`.
pub fn resolve_agent_path(gait_dir: &std::path::Path, name: &str) -> PathBuf {
    if name.contains('/') || name.ends_with(".md") {
        gait_dir.join(name)
    } else {
        gait_dir.join("agents").join(format!("{}.md", name))
    }
}

pub fn split_frontmatter(content: &str) -> (&str, &str) {
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
        .map(|seq| {
            seq.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn reads_skills_from_frontmatter() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("agent.md"),
            "---\nskills:\n  - tdd\n  - brainstorming\n---\n# prompt\n",
        )
        .unwrap();
        let skills = read_agent_skills(dir.path().join("agent.md")).unwrap();
        assert_eq!(skills, vec!["tdd", "brainstorming"]);
    }

    #[test]
    fn reads_skills_wildcard_from_frontmatter() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("agent.md"),
            "---\nskills:\n  - \"*\"\n---\n# prompt\n",
        )
        .unwrap();
        let skills = read_agent_skills(dir.path().join("agent.md")).unwrap();
        assert_eq!(skills, vec!["*"]);
    }

    #[test]
    fn returns_empty_skills_when_field_absent() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("agent.md"),
            "---\nallow:\n  - read\n---\n# prompt\n",
        )
        .unwrap();
        let skills = read_agent_skills(dir.path().join("agent.md")).unwrap();
        assert!(skills.is_empty());
    }

    #[test]
    fn reads_mcp_from_frontmatter() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("agent.md"),
            "---\nmcp:\n  - filesystem\n---\n# prompt\n",
        )
        .unwrap();
        let mcp = read_agent_mcp(dir.path().join("agent.md")).unwrap();
        assert_eq!(mcp, vec!["filesystem"]);
    }

    #[test]
    fn reads_plugins_from_frontmatter() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("agent.md"),
            "---\nplugins:\n  - my-plugin\n---\n# prompt\n",
        )
        .unwrap();
        let plugins = read_agent_plugins(dir.path().join("agent.md")).unwrap();
        assert_eq!(plugins, vec!["my-plugin"]);
    }

    #[test]
    fn reads_inference_from_frontmatter() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("agent.md"),
            "---\ninference: deep\nallow:\n  - read\n---\n# prompt\n",
        )
        .unwrap();
        let inference = read_agent_inference(&dir.path().join("agent.md"));
        assert_eq!(inference, Some("deep".to_string()));
    }

    #[test]
    fn reads_body_after_frontmatter() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("agent.md"),
            "---\nallow:\n  - read\n---\nYou are a coding assistant.\n",
        )
        .unwrap();
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
    fn resolve_agent_path_uses_dir_form() {
        let dir = TempDir::new().unwrap();
        let path = resolve_markdown_context_path(dir.path(), MarkdownKind::Agent, "coder");
        assert_eq!(path, dir.path().join("agents/coder/AGENT.md"));
    }

    #[test]
    fn resolve_markdown_context_path_project_uses_dir_form() {
        let dir = TempDir::new().unwrap();
        let path =
            resolve_markdown_context_path(dir.path(), MarkdownKind::Project, "myrepo-abc1234");
        assert_eq!(path, dir.path().join("codebases/myrepo-abc1234/PROJECT.md"));
    }

    #[test]
    fn resolve_markdown_context_path_skill_uses_dir_form() {
        let dir = TempDir::new().unwrap();
        let path = resolve_markdown_context_path(dir.path(), MarkdownKind::Skill, "tdd");
        assert_eq!(path, dir.path().join("skills/tdd/SKILL.md"));
    }

    #[test]
    fn reads_inference_without_inline_params() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("agent.md"),
            "\
---
inference: deep
---
# My agent
",
        )
        .unwrap();
        assert_eq!(
            read_agent_inference(&dir.path().join("agent.md")).as_deref(),
            Some("deep")
        );
    }

    #[test]
    fn inference_returns_none_when_absent() {
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("agent.md"),
            "---\nallow:\n  - read\n---\n# prompt\n",
        )
        .unwrap();
        assert!(read_agent_inference(&dir.path().join("agent.md")).is_none());
    }

    #[test]
    fn inference_returns_none_when_no_frontmatter() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("agent.md"), "# just a prompt\n").unwrap();
        assert!(read_agent_inference(&dir.path().join("agent.md")).is_none());
    }
}

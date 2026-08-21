use crate::agent_frontmatter::split_frontmatter;
use crate::config::SkillDef;
use std::path::Path;

/// Scan `<gait_dir>/skills/*/SKILL.md` and return one `SkillDef` per valid file.
///
/// A file is valid if it has a non-empty `name` key in its YAML frontmatter and
/// a non-empty markdown body.  Files that fail to parse are silently skipped with
/// an `eprintln!("[gait] warning: …")` diagnostic.
///
/// `SkillDef.enabled` is set to `true` for all discovered skills (disk presence
/// = available).  Caller is responsible for applying the enabled-set filter.
pub fn scan_skills(gait_dir: &Path) -> Vec<SkillDef> {
    let skills_dir = gait_dir.join("skills");
    if !skills_dir.is_dir() {
        return vec![];
    }

    let mut out = Vec::new();
    let entries = match std::fs::read_dir(&skills_dir) {
        Ok(e) => e,
        Err(err) => {
            eprintln!(
                "[gait] warning: could not read skills dir {}: {err}",
                skills_dir.display()
            );
            return vec![];
        }
    };
    for entry in entries.flatten() {
        let skill_md = entry.path().join("SKILL.md");
        if !skill_md.is_file() {
            continue;
        }
        match parse_skill_file(&skill_md) {
            Some(def) => out.push(def),
            None => eprintln!(
                "[gait] warning: skipping invalid skill file {}",
                skill_md.display()
            ),
        }
    }
    out
}

fn parse_skill_file(path: &Path) -> Option<SkillDef> {
    let content = std::fs::read_to_string(path).ok()?;
    let (fm, body) = split_frontmatter(&content);
    if fm.is_empty() {
        return None;
    }
    let yaml: serde_yaml::Value = serde_yaml::from_str(fm).ok()?;
    let name = yaml
        .get("name")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())?
        .to_string();
    let description = yaml
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let body = body.trim_end().to_string();
    if body.is_empty() {
        return None;
    }
    Some(SkillDef {
        name,
        description,
        content: body,
        enabled: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_skill(dir: &TempDir, name: &str, fm: &str, body: &str) {
        let skill_dir = dir.path().join("skills").join(name);
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            format!("---\n{}---\n{}", fm, body),
        )
        .unwrap();
    }

    #[test]
    fn scanner_returns_empty_when_no_skills_dir() {
        let dir = TempDir::new().unwrap();
        assert!(scan_skills(dir.path()).is_empty());
    }

    #[test]
    fn scanner_reads_one_skill() {
        let dir = TempDir::new().unwrap();
        write_skill(
            &dir,
            "tdd",
            "name: tdd\ndescription: Test-driven development workflow\n",
            "# TDD\n\n1. Write failing test.\n",
        );
        let skills = scan_skills(dir.path());
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "tdd");
        assert_eq!(skills[0].description, "Test-driven development workflow");
        assert!(skills[0].content.contains("TDD"));
        assert!(skills[0].enabled);
    }

    #[test]
    fn scanner_reads_multiple_skills() {
        let dir = TempDir::new().unwrap();
        write_skill(
            &dir,
            "tdd",
            "name: tdd\ndescription: TDD workflow\n",
            "# TDD\nbody\n",
        );
        write_skill(
            &dir,
            "review",
            "name: review\ndescription: Code review\n",
            "# Review\nbody\n",
        );
        let mut skills = scan_skills(dir.path());
        skills.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(skills.len(), 2);
        assert_eq!(skills[0].name, "review");
        assert_eq!(skills[1].name, "tdd");
    }

    #[test]
    fn scanner_skips_skill_with_no_name_in_frontmatter() {
        let dir = TempDir::new().unwrap();
        write_skill(&dir, "bad", "description: No name here\n", "# body\n");
        assert!(scan_skills(dir.path()).is_empty());
    }

    #[test]
    fn scanner_skips_skill_with_empty_body() {
        let dir = TempDir::new().unwrap();
        let skill_dir = dir.path().join("skills").join("empty");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: empty\ndescription: x\n---\n   \n",
        )
        .unwrap();
        assert!(scan_skills(dir.path()).is_empty());
    }

    #[test]
    fn scanner_skips_dir_with_no_skill_md() {
        let dir = TempDir::new().unwrap();
        let skill_dir = dir.path().join("skills").join("orphan");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(skill_dir.join("README.md"), "hi").unwrap();
        assert!(scan_skills(dir.path()).is_empty());
    }

    #[test]
    fn description_defaults_to_empty_when_absent() {
        let dir = TempDir::new().unwrap();
        write_skill(&dir, "nodesc", "name: nodesc\n", "# Body\ncontent\n");
        let skills = scan_skills(dir.path());
        assert_eq!(skills[0].description, "");
    }
}

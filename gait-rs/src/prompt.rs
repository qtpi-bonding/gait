//! Prompt composition: the harness preamble + an optional agent body become the
//! agent's `instructions` (the system prompt passed to the model).

use std::path::Path;

pub fn compose_system_prompt(agent_path: Option<&Path>) -> String {
    let gait_hash = &crate::GAIT_BUILD_GIT_HASH[..crate::GAIT_BUILD_GIT_HASH.len().min(7)];
    let mut prompt = format!(
        "<!-- gait-harness.md @ {} -->\n{}",
        gait_hash,
        crate::GAIT_HARNESS_PROMPT.trim_end(),
    );
    if let Some(path) = agent_path {
        let body = crate::agent_frontmatter::read_agent_body(path);
        if !body.is_empty() {
            let label = path.file_name().and_then(|n| n.to_str()).unwrap_or("agent.md");
            let hash = content_hash_short(&body);
            prompt.push_str(&format!("\n\n<!-- {} @ {} -->\n{}", label, hash, body));
        }
    }
    prompt
}

pub fn content_hash_short(s: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    format!("{:016x}", std::hash::Hasher::finish(&h))[..7].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn compose_without_agent_is_just_preamble() {
        let p = compose_system_prompt(None);
        assert!(p.contains("gait-harness.md @"));
        assert!(p.starts_with("<!-- gait-harness.md @"));
    }
}

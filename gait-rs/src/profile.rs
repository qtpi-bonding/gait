use std::collections::HashSet;
use std::path::Path;
use crate::config::GaitConfig;

/// The declarative description of an agent: 9 inputs the runner consumes,
/// built from workspace `GaitConfig` and (optionally) an agent `.md` file.
pub struct Profile {
    pub model: String,                 // "provider/model-id"
    pub instructions: String,          // composed system prompt (preamble + agent body)
    pub allow: Vec<String>,
    pub deny: Vec<String>,
    pub mcp_servers: Vec<crate::config::McpServerDef>,
    pub skills: Vec<crate::config::SkillDef>,
    pub config_tools: Vec<crate::config::ConfigTool>,   // shell-command tool descriptors from gait.jsonc (config.tools: Vec<ConfigTool>)
    pub config_enabled: std::collections::HashSet<String>,
    pub output_schema_retries: u8,
}

impl Profile {
    /// Build the declarative profile from workspace config + an optional agent .md.
    /// Model precedence: explicit override > agent frontmatter > config.
    /// allow/deny: agent frontmatter (when non-empty) supersedes config.
    pub fn from_config_and_agent(
        config: &GaitConfig,
        agent_path: Option<&Path>,
        model_override: Option<String>,
    ) -> Self {
        let model = model_override
            .or_else(|| agent_path.and_then(crate::agent_frontmatter::read_agent_model))
            .unwrap_or_else(|| config.model.clone());

        let instructions = crate::prompt::compose_system_prompt(agent_path);

        let allow = agent_path
            .and_then(|p| crate::agent_frontmatter::read_agent_allow(p.to_path_buf()).ok())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| config.allow.clone());
        let deny = agent_path
            .and_then(|p| crate::agent_frontmatter::read_agent_deny(p.to_path_buf()).ok())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| config.deny.clone());

        let config_enabled: HashSet<String> = config.mcp_servers.iter()
            .filter(|s| s.enabled).map(|s| s.name.clone())
            .chain(config.skills.iter().filter(|s| s.enabled).map(|s| s.name.clone()))
            .collect();

        Profile {
            model,
            instructions,
            allow,
            deny,
            mcp_servers: config.mcp_servers.clone(),
            skills: config.skills.clone(),
            config_tools: config.tools.clone(),
            config_enabled,
            output_schema_retries: config.output_schema_retries,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builder_uses_config_model_when_no_override_or_agent() {
        let config = GaitConfig { model: "anthropic/x".into(), ..GaitConfig::default() };
        let p = Profile::from_config_and_agent(&config, None, None);
        assert_eq!(p.model, "anthropic/x");
        assert_eq!(p.allow, config.allow);
    }

    #[test]
    fn explicit_model_override_wins() {
        let config = GaitConfig { model: "anthropic/x".into(), ..GaitConfig::default() };
        let p = Profile::from_config_and_agent(&config, None, Some("openai/y".into()));
        assert_eq!(p.model, "openai/y");
    }
}

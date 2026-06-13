use std::collections::HashSet;
use std::path::Path;
use crate::config::GaitConfig;

/// The declarative description of an agent: 9 inputs the runner consumes,
/// built from gait_dir `GaitConfig` and (optionally) an agent `.md` file.
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
    pub result_schema: Option<serde_json::Value>,
    pub hooks: Vec<crate::config::HookDef>,
}

/// The live, resolved counterpart of `Profile`: the provider, tools, and
/// MCP clients that a single turn will use. Produced by `Profile::resolve`,
/// which is the single place that wires up the model API, MCP discovery,
/// built-in tools, and config-defined tools.
pub struct ResolvedProfile {
    pub provider: std::sync::Arc<dyn crate::provider::Provider>,
    pub permissioned: crate::tools::PermissionedTools,
    pub model: String,
    pub instructions: String,
    pub mcp_registry: Vec<crate::config::McpToolDef>,
    pub mcp_clients: std::collections::HashMap<String, std::sync::Arc<crate::mcp_client::McpClient>>,
    pub skill_registry: Vec<crate::config::SkillDef>,
    pub config_enabled: std::collections::HashSet<String>,
    pub output_schema_retries: u8,
    pub result_schema: Option<serde_json::Value>,
    pub hooks: Vec<Box<dyn crate::types::Hook>>,
}

/// Deny is monotonic: every layer's deny is final. Union config-deny with the
/// agent's deny (dedup, config order first). Only `allow` is innermost-wins.
pub(crate) fn merge_deny(config_deny: &[String], agent_deny: &[String]) -> Vec<String> {
    let mut out = config_deny.to_vec();
    for d in agent_deny {
        if !out.contains(d) { out.push(d.clone()); }
    }
    out
}

impl Profile {
    /// Build the declarative profile from gait_dir config + an optional agent .md.
    /// Model precedence: explicit override > agent frontmatter > config.
    /// allow/deny: agent frontmatter (when non-empty) supersedes config.
    pub fn from_config_and_agent(
        config: &GaitConfig,
        gait_dir: &Path,
        codebase: Option<&Path>,
        agent_path: Option<&Path>,
        model_override: Option<String>,
    ) -> Self {
        let model = model_override
            .or_else(|| agent_path.and_then(crate::agent_frontmatter::read_agent_model))
            .unwrap_or_else(|| config.model.clone());

        let mut instructions = crate::prompt::compose_system_prompt(
            gait_dir, codebase, agent_path,
            crate::tools::output::DONE_TOOL_NAME,
        );
        if let Some(schema) = &config.result_schema {
            let schema_str = serde_json::to_string_pretty(schema).unwrap_or_default();
            instructions.push_str(&format!(
                "\n<result-schema>\nYour `result` field when calling `structured_done` must match this JSON Schema:\n{}\nValidation is enforced. If your call fails, you will receive the validation error and must retry with a conforming value.\n</result-schema>\n",
                schema_str
            ));
        }

        let allow = agent_path
            .and_then(|p| crate::agent_frontmatter::read_agent_allow(p.to_path_buf()).ok())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| config.allow.clone());
        let agent_deny = agent_path
            .and_then(|p| crate::agent_frontmatter::read_agent_deny(p.to_path_buf()).ok())
            .unwrap_or_default();
        let deny = merge_deny(&config.deny, &agent_deny);

        // Phase 5: skills come from disk first; fall back to config.skills for
        // backward compat with gait.jsonc inline bodies.
        let mut disk_skills = crate::skills::scan_skills(gait_dir);
        // Merge: disk wins over config for same name; config-only entries kept.
        let config_skill_names: std::collections::HashSet<String> =
            disk_skills.iter().map(|s| s.name.clone()).collect();
        for s in &config.skills {
            if !config_skill_names.contains(&s.name) {
                disk_skills.push(s.clone());
            }
        }
        let merged_skills = disk_skills;

        let config_enabled: HashSet<String> = config.mcp_servers.iter()
            .filter(|s| s.enabled).map(|s| s.name.clone())
            .chain(merged_skills.iter().filter(|s| s.enabled).map(|s| s.name.clone()))
            .collect();

        Profile {
            model,
            instructions,
            allow,
            deny,
            mcp_servers: config.mcp_servers.clone(),
            skills: merged_skills,
            config_tools: config.tools.clone(),
            config_enabled,
            output_schema_retries: config.output_schema_retries,
            result_schema: config.result_schema.clone(),
            hooks: config.hooks.clone(),
        }
    }

    /// Build the live provider, MCP clients + tool registry, and tool list
    /// (built-ins + config-defined). The single resolver — call sites in
    /// `bin/gait.rs` and `workflow/exec_prompt.rs` should both go through here.
    pub async fn resolve(
        self,
        codebase_path: std::path::PathBuf,
        session_id: String,
        gait_dir: std::path::PathBuf,
        repos: crate::git::Repos,
        api_key: Option<String>,
    ) -> anyhow::Result<ResolvedProfile> {
        // Provider (no network until .stream()).
        let provider: std::sync::Arc<dyn crate::provider::Provider> =
            std::sync::Arc::from(crate::factory::load_provider(&self.model, api_key)
                .map_err(|e| anyhow::anyhow!("provider: {e}"))?);

        // MCP connect + discover (moved verbatim from bin/gait.rs:223-237).
        let mut mcp_registry: Vec<crate::config::McpToolDef> = vec![];
        let mut mcp_clients: std::collections::HashMap<String, std::sync::Arc<crate::mcp_client::McpClient>> =
            std::collections::HashMap::new();
        for server_def in &self.mcp_servers {
            match crate::mcp_client::McpClient::connect(server_def).await {
                Ok(client) => {
                    match client.discover_tools().await {
                        Ok(tools) => {
                            eprintln!("[gait] mcp {}: {} tool(s)", server_def.name, tools.len());
                            mcp_registry.extend(tools);
                            mcp_clients.insert(server_def.name.clone(), std::sync::Arc::new(client));
                        }
                        Err(e) => eprintln!("[gait] warning: mcp {} tools/list: {e}", server_def.name),
                    }
                }
                Err(e) => eprintln!("[gait] warning: mcp {} connect: {e}", server_def.name),
            }
        }

        // Tools: builtins (moved from bin/gait.rs:242-249) + config tools (moved from bin/gait.rs:250-287).
        let mut tools = crate::tools::builtin_tools(
            crate::tools::ToolCtx::new(codebase_path.clone(), gait_dir.clone(), session_id.clone()),
            mcp_registry.clone(),
            self.skills.clone(),
            repos,
        );
        for ct in &self.config_tools {
            tools.push(build_config_tool(ct, &gait_dir, &codebase_path));
        }
        if let Some(schema) = &self.result_schema {
            if let Some(pos) = tools.iter().position(|t| t.name() == "structured_done") {
                tools[pos] = Box::new(crate::tools::OutputTool::with_schema(schema.clone()));
            }
        }

        let tx_path = gait_dir.join("sessions").join(&session_id).join("log.jsonl");
        let hooks: Vec<Box<dyn crate::types::Hook>> = self.hooks
            .iter()
            .map(|def| -> Box<dyn crate::types::Hook> {
                Box::new(crate::config_hook::ConfigHook::new(
                    def.clone(), tx_path.clone(), gait_dir.clone(), codebase_path.clone(),
                ))
            })
            .collect();
        let permissioned = crate::tools::RegisteredTools(tools)
            .into_enabled()
            .with_permissions(self.allow, self.deny);

        Ok(ResolvedProfile {
            provider,
            permissioned,
            model: self.model,
            instructions: self.instructions,
            mcp_registry,
            mcp_clients,
            skill_registry: self.skills,
            config_enabled: self.config_enabled,
            output_schema_retries: self.output_schema_retries,
            result_schema: self.result_schema,
            hooks,
        })
    }
}

impl ResolvedProfile {
    /// Minimal constructor for tests: caller supplies provider + tools + allow/deny;
    /// everything else is empty/default.
    pub fn for_test(
        provider: std::sync::Arc<dyn crate::provider::Provider>,
        tools: Vec<Box<dyn crate::types::Tool>>,
        allow: Vec<String>,
        deny: Vec<String>,
    ) -> Self {
        let permissioned = crate::tools::RegisteredTools(tools)
            .into_enabled()
            .with_permissions(allow, deny);
        ResolvedProfile {
            provider,
            permissioned,
            model: "test/model".into(),
            instructions: String::new(),
            mcp_registry: vec![],
            mcp_clients: std::collections::HashMap::new(),
            skill_registry: vec![],
            config_enabled: std::collections::HashSet::new(),
            output_schema_retries: 3,
            result_schema: None,
            hooks: vec![],
        }
    }
}

/// Build a `Box<dyn Tool>` from a `ConfigTool` (a `gait.jsonc` tool descriptor).
/// This is the body of the `for ct in &config.tools { ... }` loop formerly in
/// `bin/gait.rs`, moved verbatim: it constructs a `ToolDef` whose `execute`
/// shells out the configured command. `gait_dir` / `codebase_path` are the
/// two possible cwd values for the spawned command.
fn build_config_tool(
    ct: &crate::config::ConfigTool,
    gait_dir: &std::path::Path,
    codebase_path: &std::path::Path,
) -> Box<dyn crate::types::Tool> {
    let cmd = ct.cmd.clone();
    let cwd = if ct.cwd == "workspace" {
        gait_dir.to_path_buf()
    } else {
        codebase_path.to_path_buf()
    };
    let name = ct.name.clone();
    let description = ct.description.clone();
    // Note: config-defined tools receive no arguments from the model — they are fixed shell
    // commands. The empty parameters schema tells the model to call them with no input.
    Box::new(crate::types::ToolDef {
        name,
        description,
        parameters: serde_json::json!({
            "type": "object",
            "properties": {},
            "required": []
        }),
        execute: Box::new(move |_args| {
            let cmd = cmd.clone();
            let cwd = cwd.clone();
            Box::pin(async move {
                let output = tokio::process::Command::new("sh")
                    .arg("-c")
                    .arg(&cmd)
                    .current_dir(&cwd)
                    .output()
                    .await
                    .map_err(|e| anyhow::anyhow!("{}", e))?;
                let stdout = String::from_utf8_lossy(&output.stdout).to_string();
                let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                let combined = if stderr.is_empty() { stdout } else { format!("{}\nstderr:\n{}", stdout, stderr) };
                Ok(serde_json::Value::String(combined))
            })
        }),
        permission: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builder_uses_config_model_when_no_override_or_agent() {
        let config = GaitConfig { model: "anthropic/x".into(), ..GaitConfig::default() };
        let dir = tempfile::TempDir::new().unwrap();
        let p = Profile::from_config_and_agent(&config, dir.path(), None, None, None);
        assert_eq!(p.model, "anthropic/x");
        assert_eq!(p.allow, config.allow);
    }

    #[test]
    fn explicit_model_override_wins() {
        let config = GaitConfig { model: "anthropic/x".into(), ..GaitConfig::default() };
        let dir = tempfile::TempDir::new().unwrap();
        let p = Profile::from_config_and_agent(&config, dir.path(), None, None, Some("openai/y".into()));
        assert_eq!(p.model, "openai/y");
    }

    #[tokio::test]
    async fn resolve_with_no_mcp_builds_provider_and_builtin_tools() {
        use std::collections::HashSet;
        let profile = Profile {
            model: "anthropic/claude-x".into(),
            instructions: "sys".into(),
            allow: vec!["read".into()],
            deny: vec![],
            mcp_servers: vec![],          // no network
            skills: vec![],
            config_tools: vec![],
            config_enabled: HashSet::new(),
            output_schema_retries: 3,
            result_schema: None,
            hooks: vec![],
        };
        let resolved = profile.resolve(
            std::path::PathBuf::from("."),
            "s1".into(),
            std::path::PathBuf::from("."),
            crate::git::Repos::production(),
            Some("test-key".into()),
        ).await.unwrap();
        assert_eq!(resolved.provider.model_id(), "claude-x");
        assert!(resolved.permissioned.tools().len() >= 15, "expected the builtin tools");
        assert_eq!(resolved.permissioned.allow, vec!["read".to_string()]);
    }
    #[test]
    fn merge_deny_unions_and_dedups() {
        let d = merge_deny(&["shell:rm *".into(), "shell:curl *".into()],
                           &["shell:curl *".into(), "edit:secrets/*".into()]);
        // config denies survive; agent denies are added; no duplicates
        assert_eq!(d, vec!["shell:rm *", "shell:curl *", "edit:secrets/*"]);
    }

    #[test]
    fn disk_skills_populate_profile_skills() {
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let skill_dir = dir.path().join("skills").join("tdd");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: tdd\ndescription: TDD workflow\n---\n# TDD\nbody\n",
        ).unwrap();
        let config = GaitConfig::default();
        let profile = Profile::from_config_and_agent(&config, dir.path(), None, None, None);
        assert_eq!(profile.skills.len(), 1);
        assert_eq!(profile.skills[0].name, "tdd");
        assert!(profile.skills[0].content.contains("TDD"));
    }

    #[test]
    fn config_skills_kept_when_no_disk_skill() {
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let config = GaitConfig {
            skills: vec![crate::config::SkillDef {
                name: "inline".into(),
                description: "inline desc".into(),
                content: "# Inline body".into(),
                enabled: true,
            }],
            ..GaitConfig::default()
        };
        let profile = Profile::from_config_and_agent(&config, dir.path(), None, None, None);
        assert_eq!(profile.skills.len(), 1);
        assert_eq!(profile.skills[0].name, "inline");
    }

    #[test]
    fn disk_skill_wins_over_config_skill_same_name() {
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let skill_dir = dir.path().join("skills").join("tdd");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: tdd\ndescription: Disk version\n---\n# Disk TDD\n",
        ).unwrap();
        let config = GaitConfig {
            skills: vec![crate::config::SkillDef {
                name: "tdd".into(),
                description: "Config version".into(),
                content: "# Config TDD".into(),
                enabled: true,
            }],
            ..GaitConfig::default()
        };
        let profile = Profile::from_config_and_agent(&config, dir.path(), None, None, None);
        assert_eq!(profile.skills.len(), 1, "should deduplicate same-named skill");
        assert_eq!(profile.skills[0].description, "Disk version", "disk wins");
    }

}

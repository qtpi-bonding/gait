use crate::config::GaitConfig;
use std::collections::HashSet;
use std::path::Path;

/// The declarative description of an agent: 9 inputs the runner consumes,
/// built from gait_dir `GaitConfig` and (optionally) an agent `.md` file.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct Profile {
    pub model: String,        // "provider/model-id"
    pub instructions: String, // composed system prompt (preamble + agent body)
    pub allow: Vec<String>,
    pub deny: Vec<String>,
    pub mcp_servers: Vec<crate::config::McpServerDef>,
    pub skills: Vec<crate::config::SkillDef>,
    pub config_tools: Vec<crate::config::ConfigTool>, // shell-command tool descriptors from gait.jsonc (config.tools: Vec<ConfigTool>)
    pub config_enabled: std::collections::HashSet<String>,
    pub output_schema_retries: u8,
    pub result_schema: Option<serde_json::Value>,
    pub hooks: Vec<crate::config::HookDef>,
    pub wasm_plugins: Vec<crate::config::WasmPluginDef>,
    pub llm_params: crate::config::LlmParamsConfig,
    /// Ephemeral one-off: register only `structured_done` (schema), no hands.
    #[serde(default)]
    pub hands_free: bool,
    /// Phase 3 (scoped registry): this agent's own registration whitelist,
    /// read from frontmatter. Default-deny -- see `crate::registry`.
    #[serde(default)]
    pub agent_whitelist: crate::registry::AgentWhitelist,
}

/// The live, resolved counterpart of `Profile`: the provider, tools, and
/// MCP clients that a single turn will use. Produced by `Profile::resolve`,
/// which is the single place that wires up the model API, MCP discovery,
/// built-in tools, and config-defined tools.
pub struct ResolvedProfile {
    pub(crate) provider: std::sync::Arc<dyn gait_provider::provider::Provider>,
    pub(crate) permissioned: crate::tools::PermissionedTools,
    pub model: String,
    pub instructions: String,
    pub(crate) mcp_registry: Vec<crate::config::McpToolDef>,
    pub(crate) mcp_clients:
        std::collections::HashMap<String, std::sync::Arc<crate::mcp::McpClient>>,
    pub skill_registry: Vec<crate::config::SkillDef>,
    pub plugin_registry: Vec<crate::config::WasmPluginDef>,
    pub config_enabled: std::collections::HashSet<String>,
    pub output_schema_retries: u8,
    pub result_schema: Option<serde_json::Value>,
    pub(crate) hooks: Vec<Box<dyn crate::types::Hook>>,
    pub llm_params: crate::config::LlmParamsConfig,
    /// Phase 3 (scoped registry): threaded through from `Profile`, on into
    /// `SetupData`'s genesis-seed gate and catalog gate. See `crate::registry`.
    pub agent_whitelist: crate::registry::AgentWhitelist,
}

impl ResolvedProfile {
    /// Append a host-specific tool to the resolved profile and add its name
    /// to the allow list so the model can call it without prompting. Intended
    /// for embedders (e.g. `gait-repl`) that want to inject a tool that the
    /// base `Profile::resolve` does not know about.
    pub fn add_allowed_tool(&mut self, tool: Box<dyn crate::types::Tool>) {
        let name = tool.name().to_string();
        self.permissioned.push_tool(tool);
        if !self.permissioned.allow.iter().any(|a| a == &name) {
            self.permissioned.allow.push(name);
        }
    }
}

/// Overlay a mode's optional model onto the base model string. Pure; the caller
/// applies it to `Profile.model` before `resolve()` binds the provider.
pub fn apply_mode_model(base: String, mode_model: Option<String>) -> String {
    mode_model.unwrap_or(base)
}

/// Inputs for `build_workflow_node_profile` — the slim set of per-node knobs
/// a workflow runner passes when constructing an LLM-facing `Profile` for one
/// prompt node. Keeps the runner free of profile-layer assumptions.
pub struct WorkflowNodeProfileOpts<'a> {
    /// Gait configuration (model, allow/deny, etc.) used as the base layer.
    pub config: &'a crate::config::GaitConfig,
    /// The gait_dir that locates `agents/<name>/AGENT.md` and `USER.md`.
    pub gait_dir: &'a std::path::Path,
    /// Model override (`"provider/model-id"`). Wins over `config.model`.
    pub model: Option<String>,
    /// Optional named agent whose `.md` body becomes the `<agent>` layer.
    pub agent_name: Option<String>,
    /// Optional inline system-prompt body; appended inside an `<agent>` block
    /// (with content hash) AFTER the harness/agent/user layers, BEFORE the
    /// `<result-schema>` block.
    pub system_prompt: Option<&'a str>,
    /// Allow list for the node. Replaces `config.allow` and any agent allow.
    pub allow: Vec<String>,
    /// JSON Schema for `structured_done.result`. When `Some`, the
    /// `<result-schema>` block is appended and `Profile::resolve` wires up
    /// the schema-aware `StructuredDoneTool`.
    pub result_schema: Option<serde_json::Value>,
}

/// Build a `Profile` for a single workflow prompt node.
///
/// Mirrors the legacy in-line logic in `workflow/exec_prompt.rs` (the
/// `<harness>` + optional `<agent>` from `agents/<name>/AGENT.md` + inline
/// `<agent>` for `system_prompt` + optional `<result-schema>`), then layers
/// the explicit `allow` / `deny=[]` / `config_tools=[]` / `result_schema`
/// overrides on top so workflow nodes don't inherit the parent profile's
/// restrictions.
pub fn build_workflow_node_profile(opts: WorkflowNodeProfileOpts<'_>) -> Profile {
    let WorkflowNodeProfileOpts {
        config,
        gait_dir,
        model,
        agent_name,
        system_prompt,
        allow,
        result_schema,
    } = opts;

    // Resolve the agent markdown path (when present) so it can be folded into
    // the layered system prompt. We only point compose_system_prompt at the
    // file if it actually exists; a missing agent file is silently skipped.
    let agent_path: Option<std::path::PathBuf> = agent_name
        .as_deref()
        .map(|name| {
            crate::agent_frontmatter::resolve_markdown_context_path(
                gait_dir,
                crate::agent_frontmatter::MarkdownKind::Agent,
                name,
            )
        })
        .filter(|p| p.exists());

    // Layered system prompt: <harness> + optional <agent> (from disk) +
    // optional <user>. We pass `None` for `codebase` because workflow nodes
    // don't read the `<project>` layer — each node's prompt is its own scope.
    let mut instructions = crate::prompt::compose_system_prompt(
        gait_dir,
        None,
        agent_path.as_deref(),
        crate::tools::structured_done::DONE_TOOL_NAME,
    );

    // Inline system_prompt: append inside an <agent> block with a content
    // hash so the layer is traceable like the disk-backed layers.
    if let Some(sp) = system_prompt {
        let sp = sp.trim();
        if !sp.is_empty() {
            let hash = crate::prompt::content_hash_short(sp);
            instructions.push_str(&format!(
                "<agent src=\"system_prompt\" hash=\"{hash}\">\n{sp}\n</agent>\n"
            ));
        }
    }

    // Result-schema: appended last so the model reads the contract after the
    // persona / instructions. The wording is kept in lockstep with the
    // schema-when-present branch in `Profile::from_config_and_agent`.
    if let Some(schema) = &result_schema {
        let schema_str = serde_json::to_string_pretty(schema).unwrap_or_default();
        instructions.push_str(&format!(
            "\n<result-schema>\nYour `result` field when calling `{}` must match this JSON Schema:\n{}\nValidation is enforced. If your call fails, you will receive the validation error and must retry with a conforming value.\n</result-schema>\n",
            crate::tools::structured_done::DONE_TOOL_NAME,
            schema_str
        ));
    }

    // Build the base profile from config + agent (model precedence: explicit
    // override > agent frontmatter > config), then apply the workflow-node
    // overrides. deny=[] and config_tools=[] reflect that a workflow node is
    // a focused sub-task and should not inherit the parent session's shell
    // tool surface or deny rules.
    let mut profile = Profile::from_config_and_agent(config, gait_dir, None, None, model);
    profile.instructions = instructions;
    profile.deny = vec![];
    profile.allow = allow;
    profile.config_tools = vec![];
    profile.result_schema = result_schema;
    // Synthetic (non-agent-file) profile: `from_config_and_agent` above was
    // called with `agent_path: None`, so `agent_whitelist` defaulted to
    // all-empty (default-deny) -- a real visibility regression for workflow
    // nodes, which previously saw the global registry unfiltered. This isn't
    // the untrusted, narrowly-scoped case default-deny is meant to restrict,
    // so opt it back into "everything visible" explicitly.
    profile.agent_whitelist = crate::registry::AgentWhitelist::wildcard();
    profile
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
            .or_else(|| config.model.clone())
            .unwrap_or_else(|| {
                panic!(
                    "model must be set before building Profile — use resolve_checked() or supply a model override"
                )
            });

        let mut instructions = crate::prompt::compose_system_prompt(
            gait_dir,
            codebase,
            agent_path,
            crate::tools::structured_done::DONE_TOOL_NAME,
        );
        if let Some(schema) = &config.result_schema {
            let schema_str = serde_json::to_string_pretty(schema).unwrap_or_default();
            instructions.push_str(&format!(
                "\n<result-schema>\nYour `result` field when calling `structured_done` must match this JSON Schema:\n{}\nValidation is enforced. If your call fails, you will receive the validation error and must retry with a conforming value.\n</result-schema>\n",
                schema_str
            ));
        }

        // LLM params: merge config baseline with agent-specific override (agent wins per-field).
        let agent_llm_params = agent_path
            .and_then(crate::agent_frontmatter::read_agent_llm_params)
            .unwrap_or_default();
        let llm_params = config.llm_params.clone().merged_with(agent_llm_params);

        let allow = agent_path
            .and_then(|p| crate::agent_frontmatter::read_agent_allow(p.to_path_buf()).ok())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| config.allow.clone());
        let agent_deny = agent_path
            .and_then(|p| crate::agent_frontmatter::read_agent_deny(p.to_path_buf()).ok())
            .unwrap_or_default();
        let deny = crate::permissions::merge_deny(&config.deny, &agent_deny);

        // Phase 5: skills come from disk first; fall back to config.skills for
        // backward compat with gait.jsonc inline bodies. The merge logic is
        // extracted to `skills::merged_skill_registry` so it can be reused
        // and tested independently.
        let merged_skills = crate::skills::merged_skill_registry(gait_dir, config);

        // MCP: disk files first; gait.jsonc entries kept if not already on disk (by name).
        let mut disk_mcp = crate::mcp::scan_mcp_servers(gait_dir);
        let disk_mcp_names: std::collections::HashSet<String> =
            disk_mcp.iter().map(|s| s.name.clone()).collect();
        for s in &config.mcp_servers {
            if !disk_mcp_names.contains(&s.name) {
                disk_mcp.push(s.clone());
            }
        }
        let merged_mcp = disk_mcp;

        // Plugins: scan disk
        let disk_plugins = crate::wasm_plugins::scan_plugins(gait_dir);

        let config_enabled: HashSet<String> = merged_mcp
            .iter()
            .filter(|s| s.enabled)
            .map(|s| s.name.clone())
            .chain(
                merged_skills
                    .iter()
                    .filter(|s| s.enabled)
                    .map(|s| s.name.clone()),
            )
            .chain(
                disk_plugins
                    .iter()
                    .filter(|p| p.enabled)
                    .map(|p| p.name.clone()),
            )
            .collect();

        let agent_whitelist = crate::registry::AgentWhitelist::from_agent_path(agent_path);

        Profile {
            model,
            instructions,
            allow,
            deny,
            mcp_servers: merged_mcp,
            skills: merged_skills,
            config_tools: config.tools.clone(),
            config_enabled,
            output_schema_retries: config.output_schema_retries,
            result_schema: config.result_schema.clone(),
            hooks: config.hooks.clone(),
            wasm_plugins: disk_plugins,
            llm_params,
            hands_free: false,
            agent_whitelist,
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
        credential: Option<gait_auth::ResolvedCredential>,
    ) -> anyhow::Result<ResolvedProfile> {
        // Provider (no network until .stream()).
        let provider: std::sync::Arc<dyn gait_provider::provider::Provider> = std::sync::Arc::from(
            gait_provider::factory::load_provider_with_credential(&self.model, credential)
                .map_err(|e| anyhow::anyhow!("provider: {e}"))?,
        );

        // MCP connect + discover. Errors are silently swallowed: a failed server
        // contributes zero tools and the run continues. MCP config errors are a startup/config
        // problem the user fixes once, not a per-turn alert. Callers can detect MCP misconfig by
        // checking `resolved_profile.mcp_registry.is_empty()` when servers were configured.
        let mut mcp_registry: Vec<crate::config::McpToolDef> = vec![];
        let mut mcp_clients: std::collections::HashMap<
            String,
            std::sync::Arc<crate::mcp::McpClient>,
        > = std::collections::HashMap::new();
        for server_def in &self.mcp_servers {
            match crate::mcp::McpClient::connect(server_def).await {
                Ok(client) => {
                    match client.discover_tools().await {
                        Ok(tools) => {
                            mcp_registry.extend(tools);
                            mcp_clients
                                .insert(server_def.name.clone(), std::sync::Arc::new(client));
                        }
                        Err(_) => {
                            // tools/list failed — this server contributes no tools; continue
                        }
                    }
                }
                Err(_) => {
                    // connect failed — skip this server; continue
                }
            }
        }

        // Tools: builtins (moved from bin/gait.rs:242-249) + config tools (moved from bin/gait.rs:250-287).
        let tool_ctx =
            crate::tools::ToolCtx::new(codebase_path.clone(), gait_dir.clone(), session_id.clone());
        let tools: Vec<Box<dyn crate::types::Tool>> = if self.hands_free {
            // Hands-free: the only registered tool is the schema-aware structured_done.
            // Nothing else exists, so tool routing can never reach a hand.
            let schema = self
                .result_schema
                .clone()
                .unwrap_or_else(|| serde_json::json!({ "type": "object" }));
            vec![Box::new(crate::tools::StructuredDoneTool::with_schema(
                schema,
            ))]
        } else {
            let mut tools = crate::tools::builtin_tools(
                tool_ctx,
                mcp_registry.clone(),
                self.skills.clone(),
                self.wasm_plugins.clone(),
                repos,
            );
            for ct in &self.config_tools {
                tools.push(build_config_tool(ct, &gait_dir, &codebase_path));
            }
            if let Some(schema) = &self.result_schema {
                if let Some(pos) = tools.iter().position(|t| t.name() == "structured_done") {
                    tools[pos] = Box::new(crate::tools::StructuredDoneTool::with_schema(
                        schema.clone(),
                    ));
                }
            }
            tools
        };

        let hooks: Vec<Box<dyn crate::types::Hook>> = self
            .hooks
            .iter()
            .map(|def| -> Box<dyn crate::types::Hook> {
                Box::new(crate::config_hook::ConfigHook::new(def.clone()))
            })
            .collect();
        let (allow, deny) = if self.hands_free {
            (vec!["structured_done".to_string()], vec![])
        } else {
            (self.allow, self.deny)
        };
        let permissioned = crate::tools::RegisteredTools(tools)
            .into_enabled()
            .with_permissions(allow, deny);

        Ok(ResolvedProfile {
            provider,
            permissioned,
            model: self.model,
            instructions: self.instructions,
            mcp_registry,
            mcp_clients,
            skill_registry: self.skills,
            plugin_registry: self.wasm_plugins,
            config_enabled: self.config_enabled,
            output_schema_retries: self.output_schema_retries,
            result_schema: self.result_schema,
            hooks,
            llm_params: self.llm_params,
            agent_whitelist: self.agent_whitelist,
        })
    }
}

impl ResolvedProfile {
    /// Minimal constructor for tests: caller supplies provider + tools + allow/deny;
    /// everything else is empty/default.
    pub fn for_test(
        provider: std::sync::Arc<dyn gait_provider::provider::Provider>,
        tools: Vec<Box<dyn crate::types::Tool>>,
        allow: Vec<String>,
        deny: Vec<String>,
    ) -> Self {
        Self::for_test_with_schema(provider, tools, allow, deny, None)
    }

    pub fn for_test_with_schema(
        provider: std::sync::Arc<dyn gait_provider::provider::Provider>,
        tools: Vec<Box<dyn crate::types::Tool>>,
        allow: Vec<String>,
        deny: Vec<String>,
        result_schema: Option<serde_json::Value>,
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
            plugin_registry: vec![],
            config_enabled: std::collections::HashSet::new(),
            output_schema_retries: 3,
            result_schema,
            hooks: vec![],
            llm_params: crate::config::LlmParamsConfig::default(),
            // Test helper: no agent-file concept here, and existing tests
            // assume unfiltered visibility -- default to wildcard rather than
            // silently breaking every test that exercises loadables.
            agent_whitelist: crate::registry::AgentWhitelist::wildcard(),
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
                    .map_err(|e| crate::tools::ToolError::Internal(anyhow::anyhow!("{e}")))?;
                let stdout = String::from_utf8_lossy(&output.stdout).to_string();
                let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                let combined = if stderr.is_empty() {
                    stdout
                } else {
                    format!("{}\nstderr:\n{}", stdout, stderr)
                };
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
        let config = GaitConfig {
            model: Some("anthropic/x".into()),
            ..GaitConfig::default()
        };
        let dir = tempfile::TempDir::new().unwrap();
        let p = Profile::from_config_and_agent(&config, dir.path(), None, None, None);
        assert_eq!(p.model, "anthropic/x");
        assert_eq!(p.allow, config.allow);
    }

    #[test]
    fn explicit_model_override_wins() {
        let config = GaitConfig {
            model: Some("anthropic/x".into()),
            ..GaitConfig::default()
        };
        let dir = tempfile::TempDir::new().unwrap();
        let p = Profile::from_config_and_agent(
            &config,
            dir.path(),
            None,
            None,
            Some("openai/y".into()),
        );
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
            mcp_servers: vec![], // no network
            skills: vec![],
            config_tools: vec![],
            config_enabled: HashSet::new(),
            output_schema_retries: 3,
            result_schema: None,
            hooks: vec![],
            wasm_plugins: vec![],
            llm_params: crate::config::LlmParamsConfig::default(),
            hands_free: false,
            agent_whitelist: crate::registry::AgentWhitelist::wildcard(),
        };
        let resolved = profile
            .resolve(
                std::path::PathBuf::from("."),
                "s1".into(),
                std::path::PathBuf::from("."),
                crate::git::Repos::production(),
                Some(gait_auth::ResolvedCredential::ApiKey("test-key".into())),
            )
            .await
            .unwrap();
        assert_eq!(resolved.provider.model_id(), "claude-x");
        assert!(
            resolved.permissioned.tools().len() >= 15,
            "expected the builtin tools"
        );
        assert_eq!(resolved.permissioned.allow, vec!["read".to_string()]);
        assert_eq!(
            resolved.plugin_registry.len(),
            0,
            "expected empty plugin registry"
        );
    }
    #[test]
    fn mode_model_overlays_profile_before_resolve() {
        // Given an ActiveMode carrying model="anthropic/x", the built Profile.model
        // is "anthropic/x"; with mode model=None or cleared, it is the base model.
        let base = "anthropic/base".to_string();
        let overlaid = apply_mode_model(base.clone(), Some("anthropic/x".into()));
        assert_eq!(overlaid, "anthropic/x");
        let kept = apply_mode_model(base.clone(), None);
        assert_eq!(kept, base);
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
        )
        .unwrap();
        let config = GaitConfig {
            model: Some("test/model".into()),
            ..GaitConfig::default()
        };
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
            model: Some("test/model".into()),
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
        )
        .unwrap();
        let config = GaitConfig {
            model: Some("test/model".into()),
            skills: vec![crate::config::SkillDef {
                name: "tdd".into(),
                description: "Config version".into(),
                content: "# Config TDD".into(),
                enabled: true,
            }],
            ..GaitConfig::default()
        };
        let profile = Profile::from_config_and_agent(&config, dir.path(), None, None, None);
        assert_eq!(
            profile.skills.len(),
            1,
            "should deduplicate same-named skill"
        );
        assert_eq!(profile.skills[0].description, "Disk version", "disk wins");
    }

    #[test]
    fn disk_mcp_populates_profile_mcp_servers() {
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let mcp_dir = dir.path().join("mcp");
        std::fs::create_dir_all(&mcp_dir).unwrap();
        std::fs::write(
            mcp_dir.join("slack.json"),
            r#"{"command":"npx","args":["-y","@pkg/slack"],"enabled":true}"#,
        )
        .unwrap();
        let config = GaitConfig {
            model: Some("test/model".into()),
            ..GaitConfig::default()
        };
        let profile = Profile::from_config_and_agent(&config, dir.path(), None, None, None);
        assert_eq!(profile.mcp_servers.len(), 1);
        assert_eq!(profile.mcp_servers[0].name, "slack");
        assert_eq!(profile.mcp_servers[0].command.as_deref(), Some("npx"));
    }

    #[test]
    fn disk_mcp_wins_over_config_mcp_same_name() {
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let mcp_dir = dir.path().join("mcp");
        std::fs::create_dir_all(&mcp_dir).unwrap();
        std::fs::write(
            mcp_dir.join("slack.json"),
            r#"{"command":"npx","args":["disk-version"],"enabled":true}"#,
        )
        .unwrap();
        let config = GaitConfig {
            model: Some("test/model".into()),
            mcp_servers: vec![crate::config::McpServerDef {
                name: "slack".into(),
                command: None,
                args: None,
                env: None,
                url: None,
                spawn: Some("node config-version.js".into()),
                connect: None,
                enabled: true,
                description: None,
            }],
            ..GaitConfig::default()
        };
        let profile = Profile::from_config_and_agent(&config, dir.path(), None, None, None);
        assert_eq!(profile.mcp_servers.len(), 1, "should deduplicate");
        assert_eq!(
            profile.mcp_servers[0].command.as_deref(),
            Some("npx"),
            "disk wins"
        );
    }

    #[test]
    fn config_mcp_kept_when_no_disk_mcp() {
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let config = GaitConfig {
            model: Some("test/model".into()),
            mcp_servers: vec![crate::config::McpServerDef {
                name: "remote".into(),
                command: None,
                args: None,
                env: None,
                url: Some("http://localhost/mcp".into()),
                spawn: None,
                connect: None,
                enabled: true,
                description: None,
            }],
            ..GaitConfig::default()
        };
        let profile = Profile::from_config_and_agent(&config, dir.path(), None, None, None);
        assert_eq!(profile.mcp_servers.len(), 1);
        assert_eq!(profile.mcp_servers[0].name, "remote");
    }

    #[test]
    fn disk_plugins_populate_profile() {
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let plugin_dir = dir.path().join("plugins").join("formatter");
        std::fs::create_dir_all(&plugin_dir).unwrap();
        std::fs::write(plugin_dir.join("formatter.wasm"), b"").unwrap();
        std::fs::write(
            plugin_dir.join("PLUGIN.md"),
            "---\nname: formatter\nwasm: formatter.wasm\nenabled: true\n---\n# Formatter\n",
        )
        .unwrap();
        let config = GaitConfig {
            model: Some("test/model".into()),
            ..GaitConfig::default()
        };
        let profile = Profile::from_config_and_agent(&config, dir.path(), None, None, None);
        assert_eq!(profile.wasm_plugins.len(), 1);
        assert_eq!(profile.wasm_plugins[0].name, "formatter");
    }
    #[test]
    fn build_workflow_node_profile_roundtrips_model() {
        let dir = tempfile::TempDir::new().unwrap();
        let config = crate::config::GaitConfig::default();
        let p = build_workflow_node_profile(WorkflowNodeProfileOpts {
            config: &config,
            gait_dir: dir.path(),
            model: Some("test/model".into()),
            agent_name: None,
            system_prompt: Some("do stuff"),
            allow: vec!["read".into()],
            result_schema: None,
        });
        assert_eq!(p.model, "test/model");
        assert!(p.instructions.contains("do stuff"));
        assert_eq!(p.allow, vec!["read".to_string()]);
    }

    #[test]
    fn profile_round_trips_through_json() {
        let p = Profile {
            model: "anthropic/claude-x".into(),
            instructions: "sys".into(),
            allow: vec!["read".into()],
            deny: vec![],
            mcp_servers: vec![],
            skills: vec![],
            config_tools: vec![],
            config_enabled: std::collections::HashSet::new(),
            output_schema_retries: 0,
            result_schema: None,
            hooks: vec![],
            wasm_plugins: vec![],
            llm_params: crate::config::LlmParamsConfig::default(),
            hands_free: false,
            agent_whitelist: crate::registry::AgentWhitelist::wildcard(),
        };
        let json = serde_json::to_string(&p).expect("serialize");
        let back: Profile = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.model, "anthropic/claude-x");
        assert_eq!(back.instructions, "sys");
        assert_eq!(back.allow, vec!["read".to_string()]);
    }

    #[tokio::test]
    async fn hands_free_resolves_only_structured_done() {
        let schema = serde_json::json!({ "type": "object" });
        let profile = Profile {
            model: "anthropic/claude-haiku-4-5-20251001".into(),
            instructions: String::new(),
            allow: vec![],
            deny: vec![],
            mcp_servers: vec![],
            skills: vec![],
            config_tools: vec![],
            config_enabled: std::collections::HashSet::new(),
            output_schema_retries: 3,
            result_schema: Some(schema),
            hooks: vec![],
            wasm_plugins: vec![],
            llm_params: crate::config::LlmParamsConfig::default(),
            hands_free: true,
            agent_whitelist: crate::registry::AgentWhitelist::wildcard(),
        };
        let resolved = profile
            .resolve(
                std::path::PathBuf::from("/tmp/cb"),
                "s".into(),
                std::path::PathBuf::from("/tmp/gd"),
                crate::git::Repos::production(),
                Some(gait_auth::ResolvedCredential::ApiKey("test-key".into())),
            )
            .await
            .unwrap();
        let names: Vec<_> = resolved
            .permissioned
            .tools()
            .iter()
            .map(|t| t.name())
            .collect();
        assert_eq!(names, vec!["structured_done"]);
    }
}

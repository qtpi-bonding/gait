use std::path::{Path, PathBuf};
use std::sync::Arc;
use crate::workflow::schema::{NodeDef, WorkflowDef};
use crate::workflow::vars::{NodeOutput, substitute};
use crate::workflow::exec_shell::NodeContext;
use crate::turn::{TurnOpts, TurnResult, run_turn};
use crate::provider::Provider;

pub async fn exec_prompt_node(
    node: &NodeDef,
    prompt_text: &str,
    ctx: &NodeContext<'_>,
    workflow_def: &WorkflowDef,
    provider: Arc<dyn Provider>,
) -> anyhow::Result<NodeOutput> {
    let task = substitute(prompt_text, &ctx.vars);
    let full_task = if let Some(schema) = &node.output_schema {
        format!("{}\n\nYou MUST call the output() tool with this JSON structure:\n{}", task,
            serde_json::to_string_pretty(schema).unwrap_or_default())
    } else { task };

    let model = node.model.clone()
        .or_else(|| workflow_def.model.clone())
        .unwrap_or_else(|| "minimax/MiniMax-M1-80k".into());
    let allow = node.allow.clone().unwrap_or_else(|| workflow_def.allow.clone());
    let auto = node.auto.unwrap_or(workflow_def.auto);

    let system_prompt = resolve_system_prompt(node, ctx.workspace);
    let codebase = node.codebase.as_ref()
        .map(|cb| PathBuf::from(substitute(cb, &ctx.vars)))
        .unwrap_or_else(|| ctx.codebase.to_path_buf());

    let config = crate::config::load_config(ctx.workspace);

    // Discover tools from all configured MCP servers
    let mut mcp_registry: Vec<crate::config::McpToolDef> = vec![];
    let mut mcp_clients: std::collections::HashMap<String, std::sync::Arc<crate::mcp_client::McpClient>> = std::collections::HashMap::new();
    for server_def in &config.mcp_servers {
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

    let tools = crate::tools::builtin_tools(
        codebase.clone(), ctx.session_id.to_string(), ctx.workspace.to_path_buf(),
        mcp_registry.clone(), config.skills.clone(),
    );

    let result = run_turn(TurnOpts {
        session_id: ctx.session_id.to_string(),
        task: Some(full_task),
        auto,
        workspace: ctx.workspace.to_path_buf(),
        codebase_path: codebase,
        allow,
        deny: vec![],
        system_prompt,
        model,
        mcp_registry,
        mcp_clients,
        skill_registry: config.skills.clone(),
        config_enabled: config.mcp_servers.iter()
            .filter(|s| s.enabled)
            .map(|s| s.name.clone())           // server name, not tool names
            .chain(config.skills.iter().filter(|s| s.enabled).map(|s| s.name.clone()))
            .collect(),
    }, provider, tools).await?;

    match result {
        TurnResult::Done { summary, result } => Ok(NodeOutput {
            text: summary, exit_code: None, json: result, routed_to: None,
        }),
        TurnResult::Permissions => Ok(NodeOutput {
            text: "(permissions required)".into(), exit_code: None, json: None, routed_to: None,
        }),
        TurnResult::Clarify { .. } => Ok(NodeOutput {
            text: "(paused for clarification)".into(), exit_code: None, json: None, routed_to: None,
        }),
        TurnResult::Inform { .. } => Ok(NodeOutput {
            text: "(paused to inform user)".into(), exit_code: None, json: None, routed_to: None,
        }),
    }
}

fn resolve_system_prompt(node: &NodeDef, workspace: &Path) -> String {
    if let Some(sp) = &node.system_prompt { return sp.clone(); }
    if let Some(name) = &node.agent {
        let p = workspace.join("agents").join(format!("{}.md", name));
        if let Ok(s) = std::fs::read_to_string(&p) { return s; }
    }
    let agents_md = workspace.join("AGENTS.md");
    std::fs::read_to_string(&agents_md).unwrap_or_default()
}

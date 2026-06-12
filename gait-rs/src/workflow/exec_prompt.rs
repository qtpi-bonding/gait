use std::path::{Path, PathBuf};
use std::sync::Arc;
use crate::workflow::schema::{NodeDef, WorkflowDef};
use crate::workflow::vars::{NodeOutput, substitute};
use crate::workflow::exec_shell::NodeContext;
use crate::turn::{TurnResult, run_turn};
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
        .unwrap_or_else(|| "minimax/MiniMax-M3".into());
    let allow = node.allow.clone().unwrap_or_else(|| workflow_def.allow.clone());
    let auto = node.auto.unwrap_or(workflow_def.auto);

    let system_prompt = resolve_system_prompt(node, ctx.gait_dir);
    let codebase = node.codebase.as_ref()
        .map(|cb| PathBuf::from(substitute(cb, &ctx.vars)))
        .unwrap_or_else(|| ctx.codebase.to_path_buf());

    let config = crate::config::load_config(ctx.gait_dir);

    // Build the declarative profile for this workflow node, then resolve tools/clients in one place.
    // codebase: None — workflow nodes overwrite `instructions` below, so the
    // project layer is irrelevant here.
    let mut profile = crate::profile::Profile::from_config_and_agent(&config, ctx.gait_dir, None, None, Some(model.clone()));
    profile.instructions = system_prompt;   // workflow body (resolve_system_prompt), not the harness preamble
    profile.deny = vec![];                   // workflow prompt nodes carry no deny
    profile.allow = allow;                   // workflow-resolved allow
    profile.config_tools = vec![];           // PARITY: old exec_prompt used builtins only (no config-tool loop)

    let api_key = crate::auth::resolve_api_key(model.split_once('/').map(|(p, _)| p).unwrap_or(""));
    let repos = crate::git::Repos::production();
    let mut resolved = profile.resolve(
        codebase.clone(),
        ctx.session_id.to_string(),
        ctx.gait_dir.to_path_buf(),
        repos.clone(),
        api_key,
    ).await?;

    resolved.provider = provider;   // workflow reuses its injected provider, not the freshly-resolved one
    let session = crate::session::Session::load(
        ctx.gait_dir.to_path_buf(),
        ctx.session_id.to_string(),
        codebase,
        repos,
    );
    // workflow turns currently discard the live stream; transcript still written. Future work.
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<crate::proto::gait::log::v1::StreamItem>();
    let emitter = crate::proto::Emitter::for_session(&session.gait_dir, &session.id, tx);
    let result = run_turn(
        &session,
        &emitter,
        resolved,
        crate::turn::TurnInput { task: Some(full_task), auto, max_iter: None },
    ).await?;

    match result {
        TurnResult::Done { message, result } => Ok(NodeOutput {
            text: message, exit_code: None, json: result, routed_to: None,
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

fn resolve_system_prompt(node: &NodeDef, gait_dir: &Path) -> String {
    if let Some(sp) = &node.system_prompt { return sp.clone(); }
    if let Some(name) = &node.agent {
        let p = crate::agent_frontmatter::resolve_markdown_context_path(
            gait_dir,
            crate::agent_frontmatter::MarkdownKind::Agent,
            name,
        );
        if let Ok(s) = std::fs::read_to_string(&p) { return s; }
    }
    String::new()
}

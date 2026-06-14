use std::path::PathBuf;
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

    let model = node.model.clone()
        .or_else(|| workflow_def.model.clone())
        .unwrap_or_else(|| "minimax/MiniMax-M3".into());
    let allow = node.allow.clone().unwrap_or_else(|| workflow_def.allow.clone());
    let auto = node.auto.unwrap_or(workflow_def.auto);

    // Harness is always present. Agent layer comes from node.agent (file) or
    // node.system_prompt (inline). Result-schema block appended when declared.
    let agent_path: Option<PathBuf> = node.agent.as_ref().map(|name| {
        crate::agent_frontmatter::resolve_markdown_context_path(
            ctx.gait_dir,
            crate::agent_frontmatter::MarkdownKind::Agent,
            name,
        )
    }).filter(|p| p.exists());

    let mut instructions = crate::prompt::compose_system_prompt(
        ctx.gait_dir,
        None,
        agent_path.as_deref(),
        crate::tools::output::DONE_TOOL_NAME,
    );

    if let Some(sp) = &node.system_prompt {
        let sp = sp.trim();
        if !sp.is_empty() {
            let hash = crate::prompt::content_hash_short(sp);
            instructions.push_str(&format!(
                "<agent src=\"system_prompt\" hash=\"{hash}\">\n{sp}\n</agent>\n"
            ));
        }
    }

    if let Some(schema) = &node.output_schema {
        let schema_str = serde_json::to_string_pretty(schema).unwrap_or_default();
        instructions.push_str(&format!(
            "\n<result-schema>\nYour `result` field when calling `{}` must match this JSON Schema:\n{}\nValidation is enforced. If your call fails, you will receive the validation error and must retry with a conforming value.\n</result-schema>\n",
            crate::tools::output::DONE_TOOL_NAME,
            schema_str
        ));
    }

    let codebase = node.codebase.as_ref()
        .map(|cb| PathBuf::from(substitute(cb, &ctx.vars)))
        .unwrap_or_else(|| ctx.codebase.to_path_buf());

    let config = crate::config::load_config(ctx.gait_dir);
    let mut profile = crate::profile::Profile::from_config_and_agent(&config, ctx.gait_dir, None, None, Some(model.clone()));
    profile.instructions = instructions;
    profile.deny = vec![];
    profile.allow = allow;
    profile.config_tools = vec![];
    if let Some(schema) = node.output_schema.clone() {
        // Wire up OutputTool::with_schema so result validation is enforced at the tool layer.
        profile.result_schema = Some(schema);
    }

    let api_key = crate::auth::resolve_api_key(model.split_once('/').map(|(p, _)| p).unwrap_or(""));
    let repos = crate::git::Repos::production();
    let mut resolved = profile.resolve(
        codebase.clone(),
        ctx.session_id.to_string(),
        ctx.gait_dir.to_path_buf(),
        repos.clone(),
        api_key,
    ).await?;

    resolved.provider = provider;
    let session = crate::session::Session::load(
        ctx.gait_dir.to_path_buf(),
        ctx.session_id.to_string(),
        codebase,
        repos,
    );
    // Publish the sub-turn's deltas/events to the WORKFLOW's channel (so the run's
    // consumer surfaces them), while still writing the sub-turn's own transcript at
    // `session.id`. The clone is transient — it drops when this node returns, so the
    // workflow's owning Recorder stays the single channel-close point.
    let emitter = crate::proto::Recorder::for_session(&session.gait_dir, &session.id, ctx.workflow_tx.clone());
    let result = run_turn(
        &session,
        &emitter,
        resolved,
        crate::turn::TurnInput { task: Some(task), auto, max_iter: None, preview_policy: crate::config::PreviewPolicy::default() },
    ).await?;

    match result {
        TurnResult::Done { message, result, .. } => Ok(NodeOutput {
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

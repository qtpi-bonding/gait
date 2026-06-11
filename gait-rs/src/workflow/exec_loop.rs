use std::sync::Arc;
use crate::workflow::schema::{NodeDef, LoopConfig, WorkflowDef};
use crate::workflow::vars::{NodeOutput, substitute};
use crate::workflow::exec_shell::NodeContext;
use crate::workflow::exec_prompt::exec_prompt_node;
use crate::provider::Provider;

pub async fn exec_loop_node(
    node: &NodeDef,
    loop_cfg: &LoopConfig,
    ctx: &NodeContext<'_>,
    workflow_def: &WorkflowDef,
    provider: Arc<dyn Provider>,
) -> anyhow::Result<NodeOutput> {
    let mut last_output = String::new();

    for iter in 1..=(loop_cfg.max as usize) {
        let mut vars = ctx.vars.clone();
        vars.loop_iter = Some(iter as u32);
        vars.loop_max = Some(loop_cfg.max);
        vars.loop_output = if last_output.is_empty() { None } else { Some(last_output.clone()) };

        let prompt = substitute(&loop_cfg.prompt, &vars);

        let iter_ctx = NodeContext {
            workflow_id: ctx.workflow_id,
            session_id: ctx.session_id,
            gait_dir: ctx.gait_dir,
            codebase: ctx.codebase,
            vars: vars.clone(),
        };

        let result = exec_prompt_node(node, &prompt, &iter_ctx, workflow_def, provider.clone()).await?;
        last_output = result.text.clone();

        if let Some(signal) = &loop_cfg.until {
            if last_output.contains(signal.as_str()) {
                return Ok(NodeOutput {
                    text: last_output, exit_code: None, json: result.json, routed_to: None,
                });
            }
        }

        if let Some(cmd) = &loop_cfg.shell_check {
            let substituted = substitute(cmd, &vars);
            let out = tokio::process::Command::new("sh")
                .arg("-c").arg(&substituted).current_dir(ctx.codebase).output().await;
            if let Ok(o) = out {
                if o.status.success() {
                    return Ok(NodeOutput {
                        text: last_output, exit_code: Some(0), json: None, routed_to: None,
                    });
                }
            }
        }
    }

    anyhow::bail!("loop node exhausted {} iterations without completion signal", loop_cfg.max)
}

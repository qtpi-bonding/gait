use crate::schema::{GateConfig, GATE_PAUSED_SENTINEL};
use crate::vars::{substitute, NodeOutput, VarContext};

pub fn exec_gate_node(
    node_id: &str,
    gate_cfg: &GateConfig,
    vars: &VarContext,
) -> anyhow::Result<NodeOutput> {
    let message = substitute(gate_cfg.message(), vars);
    let _ = node_id; // workflow_gate event is now recorded by the runner main loop
    Ok(NodeOutput {
        text: message,
        exit_code: None,
        json: None,
        routed_to: Some(GATE_PAUSED_SENTINEL.into()),
    })
}

use crate::schema::GATE_PAUSED_SENTINEL;
use crate::vars::{substitute, NodeOutput, VarContext};
use gait_core::proto::gait::workflow::v1::GateNode;

pub fn exec_gate_node(
    node_id: &str,
    gate_cfg: &GateNode,
    vars: &VarContext,
) -> anyhow::Result<NodeOutput> {
    let message = substitute(&gate_cfg.message, vars);
    let _ = node_id; // workflow_gate event is recorded by the runner main loop
    Ok(NodeOutput {
        text: message,
        exit_code: None,
        json: None,
        routed_to: Some(GATE_PAUSED_SENTINEL.into()),
    })
}

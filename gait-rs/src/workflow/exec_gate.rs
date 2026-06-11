use std::path::Path;
use crate::workflow::schema::GateConfig;
use crate::workflow::vars::{NodeOutput, VarContext, substitute};
use crate::proto::wire;

pub fn exec_gate_node(
    workflow_id: &str,
    node_id: &str,
    gate_cfg: &GateConfig,
    vars: &VarContext,
    tx_path: &Path,
) -> anyhow::Result<NodeOutput> {
    let message = substitute(gate_cfg.message(), vars);

    wire::emit(tx_path, wire::raw("workflow_gate", serde_json::json!({
        "workflow_id": workflow_id.to_string(),
        "node_id": node_id.to_string(),
        "message": message.clone(),
    })))?;

    Ok(NodeOutput {
        text: message,
        exit_code: None,
        json: None,
        routed_to: Some("__gate_paused__".into()),
    })
}

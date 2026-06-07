use std::path::Path;
use crate::workflow::schema::GateConfig;
use crate::workflow::vars::{NodeOutput, VarContext, substitute};
use crate::session::transcript::{AnyEvent, append_event, now_ms};

pub fn exec_gate_node(
    workflow_id: &str,
    node_id: &str,
    gate_cfg: &GateConfig,
    vars: &VarContext,
    tx_path: &Path,
) -> anyhow::Result<NodeOutput> {
    let message = substitute(gate_cfg.message(), vars);
    eprintln!("\n[gait workflow] GATE: {}", message);
    eprintln!("[gait workflow] Resume with: gait workflow continue {}", workflow_id);

    append_event(tx_path, &AnyEvent::WorkflowGate {
        workflow_id: workflow_id.to_string(),
        node_id: node_id.to_string(),
        message: message.clone(),
        ts: now_ms(),
    })?;

    Ok(NodeOutput {
        text: message,
        exit_code: None,
        json: None,
        routed_to: Some("__gate_paused__".into()),
    })
}

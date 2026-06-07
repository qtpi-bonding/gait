use std::collections::HashMap;
use crate::session::transcript::AnyEvent;
use crate::workflow::vars::NodeOutput;

#[derive(Debug, Clone, PartialEq)]
pub enum NodeStatus {
    Pending,
    Running,
    Done,
    Skipped,
    Error(String),
    Paused,
}

pub struct WorkflowState {
    pub workflow_id: String,
    pub node_outputs: HashMap<String, NodeOutput>,
    node_statuses: HashMap<String, NodeStatus>,
    pub failed_node: Option<String>,
    pub gate_paused: Option<String>,
}

impl WorkflowState {
    pub fn node_status(&self, id: &str) -> NodeStatus {
        self.node_statuses.get(id).cloned().unwrap_or(NodeStatus::Pending)
    }

    pub fn set_status(&mut self, id: &str, status: NodeStatus) {
        self.node_statuses.insert(id.to_string(), status);
    }

    pub fn is_done(&self, id: &str) -> bool {
        matches!(self.node_status(id), NodeStatus::Done)
    }

    pub fn is_skipped(&self, id: &str) -> bool {
        matches!(self.node_status(id), NodeStatus::Skipped)
    }
}

pub fn reconstruct_state(events: &[AnyEvent]) -> WorkflowState {
    let mut state = WorkflowState {
        workflow_id: String::new(),
        node_outputs: HashMap::new(),
        node_statuses: HashMap::new(),
        failed_node: None,
        gate_paused: None,
    };
    for event in events {
        match event {
            AnyEvent::WorkflowStart { workflow_id, .. } => {
                state.workflow_id = workflow_id.clone();
            }
            AnyEvent::NodeStart { node_id, .. } => {
                state.set_status(node_id, NodeStatus::Running);
            }
            AnyEvent::NodeComplete { node_id, output, exit_code, json_output, routed_to, .. } => {
                state.set_status(node_id, NodeStatus::Done);
                state.node_outputs.insert(node_id.clone(), NodeOutput {
                    text: output.clone(),
                    exit_code: *exit_code,
                    json: json_output.clone(),
                    routed_to: routed_to.clone(),
                });
            }
            AnyEvent::NodeSkip { node_id, .. } => {
                state.set_status(node_id, NodeStatus::Skipped);
            }
            AnyEvent::NodeError { node_id, error, .. } => {
                state.set_status(node_id, NodeStatus::Error(error.clone()));
            }
            AnyEvent::WorkflowGate { node_id, .. } => {
                state.gate_paused = Some(node_id.clone());
                state.set_status(node_id, NodeStatus::Paused);
            }
            AnyEvent::WorkflowError { failed_node, .. } => {
                state.failed_node = Some(failed_node.clone());
            }
            _ => {}
        }
    }
    state
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::transcript::AnyEvent;

    #[test]
    fn reconstruct_from_events() {
        let events = vec![
            AnyEvent::WorkflowStart { workflow_id: "wf1".into(), name: "t".into(), yaml_path: "".into(), args: vec![], ts: 1 },
            AnyEvent::NodeStart { workflow_id: "wf1".into(), node_id: "n1".into(), node_type: "shell".into(), attempt: 1, ts: 2 },
            AnyEvent::NodeComplete { workflow_id: "wf1".into(), node_id: "n1".into(), output: "ok".into(), exit_code: Some(0), json_output: None, routed_to: None, ts: 3 },
            AnyEvent::NodeSkip { workflow_id: "wf1".into(), node_id: "n2".into(), reason: "inactive branch".into(), ts: 4 },
        ];
        let state = reconstruct_state(&events);
        assert_eq!(state.workflow_id, "wf1");
        assert_eq!(state.node_status("n1"), NodeStatus::Done);
        assert_eq!(state.node_status("n2"), NodeStatus::Skipped);
        assert_eq!(state.node_status("n3"), NodeStatus::Pending);
        let out = state.node_outputs.get("n1").unwrap();
        assert_eq!(out.exit_code, Some(0));
    }
}

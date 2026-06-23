use crate::vars::NodeOutput;
use gait_core::proto::gait::workflow::v1::{workflow_event, WorkflowEvent};
use std::collections::HashMap;

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
        self.node_statuses
            .get(id)
            .cloned()
            .unwrap_or(NodeStatus::Pending)
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

    /// Returns the node_id of the last node that was started but not yet
    /// completed, skipped, or errored — i.e. the node that was in-flight when
    /// the workflow was interrupted.
    pub fn open_node(&self) -> Option<&str> {
        // Find the last Running entry (insertion order is preserved in HashMap
        // only in Rust's current impl — to be safe we just return any Running).
        // In practice there is at most one Running node at exit (gate pause exits
        // before a second node starts).
        self.node_statuses.iter().find_map(|(id, status)| {
            if *status == NodeStatus::Running {
                Some(id.as_str())
            } else {
                None
            }
        })
    }
}

/// Fold a proto `WorkflowEvent` run-log into a `WorkflowState`.
/// This is the authoritative replay: state is reconstructed by matching on the
/// typed `WorkflowEvent.kind` oneof directly — no JSON parsing required.
pub fn reconstruct_state(events: &[WorkflowEvent]) -> WorkflowState {
    let mut state = WorkflowState {
        workflow_id: String::new(),
        node_outputs: HashMap::new(),
        node_statuses: HashMap::new(),
        failed_node: None,
        gate_paused: None,
    };
    for event in events {
        let Some(kind) = &event.kind else {
            continue;
        };
        match kind {
            workflow_event::Kind::WorkflowStart(ws) => {
                state.workflow_id = ws.run_id.clone();
            }
            workflow_event::Kind::NodeStart(ns) => {
                state.set_status(&ns.node_id, NodeStatus::Running);
            }
            workflow_event::Kind::NodeComplete(nc) => {
                state.set_status(&nc.node_id, NodeStatus::Done);
                let json = if nc.json_output.is_empty() {
                    None
                } else {
                    serde_json::from_str(&nc.json_output).ok()
                };
                let routed_to = if nc.routed_to.is_empty() {
                    None
                } else {
                    Some(nc.routed_to.clone())
                };
                state.node_outputs.insert(
                    nc.node_id.clone(),
                    NodeOutput {
                        text: nc.output.clone(),
                        exit_code: nc.exit_code,
                        json,
                        routed_to,
                    },
                );
            }
            workflow_event::Kind::NodeSkip(ns) => {
                state.set_status(&ns.node_id, NodeStatus::Skipped);
            }
            workflow_event::Kind::NodeError(ne) => {
                state.set_status(&ne.node_id, NodeStatus::Error(ne.error.clone()));
            }
            workflow_event::Kind::WorkflowGate(wg) => {
                state.gate_paused = Some(wg.node_id.clone());
                state.set_status(&wg.node_id, NodeStatus::Paused);
            }
            workflow_event::Kind::WorkflowError(we) => {
                state.failed_node = Some(we.failed_node.clone());
            }
            workflow_event::Kind::WorkflowComplete(_) => {}
        }
    }
    state
}

#[cfg(test)]
mod tests {
    use super::*;
    use gait_core::proto::gait::workflow::v1::*;

    fn we(kind: workflow_event::Kind) -> WorkflowEvent {
        WorkflowEvent {
            version: 1,
            ts: 0,
            kind: Some(kind),
        }
    }

    #[test]
    fn folds_proto_and_reports_open_node() {
        let evs = vec![
            we(workflow_event::Kind::WorkflowStart(WorkflowStart {
                run_id: "r".into(),
                workflow: "t".into(),
                ..Default::default()
            })),
            we(workflow_event::Kind::NodeStart(NodeStart {
                node_id: "a".into(),
                ..Default::default()
            })),
            we(workflow_event::Kind::NodeComplete(NodeComplete {
                node_id: "a".into(),
                output: "ok".into(),
                ..Default::default()
            })),
            we(workflow_event::Kind::NodeStart(NodeStart {
                node_id: "b".into(),
                ..Default::default()
            })),
        ];
        let st = reconstruct_state(&evs);
        assert_eq!(st.node_status("a"), NodeStatus::Done);
        assert_eq!(st.open_node(), Some("b")); // started, no complete => the open node
    }

    #[test]
    fn reconstruct_from_proto_events() {
        let evs = vec![
            we(workflow_event::Kind::WorkflowStart(WorkflowStart {
                run_id: "wf1".into(),
                workflow: "t".into(),
                ..Default::default()
            })),
            we(workflow_event::Kind::NodeStart(NodeStart {
                node_id: "n1".into(),
                node_type: "shell".into(),
                attempt: 1,
                ..Default::default()
            })),
            we(workflow_event::Kind::NodeComplete(NodeComplete {
                node_id: "n1".into(),
                output: "ok".into(),
                exit_code: Some(0),
                ..Default::default()
            })),
            we(workflow_event::Kind::NodeSkip(NodeSkip {
                node_id: "n2".into(),
                reason: "inactive branch".into(),
            })),
        ];
        let state = reconstruct_state(&evs);
        assert_eq!(state.workflow_id, "wf1");
        assert_eq!(state.node_status("n1"), NodeStatus::Done);
        assert_eq!(state.node_status("n2"), NodeStatus::Skipped);
        assert_eq!(state.node_status("n3"), NodeStatus::Pending);
        let out = state.node_outputs.get("n1").unwrap();
        assert_eq!(out.exit_code, Some(0));
        assert_eq!(state.open_node(), None); // all nodes completed or skipped
    }
}

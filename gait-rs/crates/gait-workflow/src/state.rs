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

#[derive(Debug, Clone, Default)]
pub struct WorkflowState {
    pub workflow_id: String,
    pub workflow_name: String,
    pub node_outputs: HashMap<String, NodeOutput>,
    node_statuses: HashMap<String, NodeStatus>,
    pub failed_node: Option<String>,
    pub gate_paused: Option<String>,
    workflow_status: String,
    /// Per-node effective session id (item 13b/13c), keyed by node id. Recorded
    /// at a node's first `NodeStart` (live or replayed) and reused on every
    /// later resume — see `resolve_node_session`.
    pub node_sessions: HashMap<String, String>,
}

impl WorkflowState {
    /// Workflow-level status folded from the run log, mirroring `status::replay`.
    pub fn workflow_status(&self) -> &str {
        &self.workflow_status
    }

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
        workflow_name: String::new(),
        node_outputs: HashMap::new(),
        node_statuses: HashMap::new(),
        failed_node: None,
        gate_paused: None,
        workflow_status: "running".to_string(),
        node_sessions: HashMap::new(),
    };
    for event in events {
        let Some(kind) = &event.kind else {
            continue;
        };
        match kind {
            workflow_event::Kind::WorkflowStart(ws) => {
                state.workflow_id = ws.run_id.clone();
                state.workflow_name = ws.workflow.clone();
            }
            workflow_event::Kind::NodeStart(ns) => {
                state.set_status(&ns.node_id, NodeStatus::Running);
                state.workflow_status = "running".to_string();
                if !ns.session_id.is_empty() {
                    state
                        .node_sessions
                        .insert(ns.node_id.clone(), ns.session_id.clone());
                }
            }
            workflow_event::Kind::NodeComplete(nc) => {
                state.set_status(&nc.node_id, NodeStatus::Done);
                state.workflow_status = "running".to_string();
                let json = if nc.json_output.is_empty() {
                    None
                } else {
                    match serde_json::from_str(&nc.json_output) {
                        Ok(v) => Some(v),
                        Err(e) => {
                            eprintln!(
                                "[gait workflow] node '{}': dropping unparseable json_output: {}",
                                nc.node_id, e
                            );
                            None
                        }
                    }
                };
                let routed_to = if nc.routed_to.is_empty() {
                    None
                } else {
                    Some(nc.routed_to.clone())
                };
                let session_id = state
                    .node_sessions
                    .get(&nc.node_id)
                    .cloned()
                    .unwrap_or_default();
                state.node_outputs.insert(
                    nc.node_id.clone(),
                    NodeOutput {
                        text: nc.output.clone(),
                        exit_code: nc.exit_code,
                        json,
                        routed_to,
                        session_id,
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
                state.workflow_status = "gate-paused".to_string();
            }
            workflow_event::Kind::WorkflowError(we) => {
                state.failed_node = Some(we.failed_node.clone());
                state.workflow_status = "failed".to_string();
            }
            workflow_event::Kind::WorkflowComplete(_) => {
                state.workflow_status = "done".to_string();
            }
            workflow_event::Kind::HookRun(_) => {}
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
            id: String::new(),
            seq: 0,
            content_hash: String::new(),
            parents: vec![],
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

    #[test]
    fn recovers_name_and_workflow_status() {
        let running = reconstruct_state(&[
            we(workflow_event::Kind::WorkflowStart(WorkflowStart {
                run_id: "r".into(),
                workflow: "deploy".into(),
                ..Default::default()
            })),
            we(workflow_event::Kind::NodeStart(NodeStart {
                node_id: "a".into(),
                ..Default::default()
            })),
        ]);
        assert_eq!(running.workflow_name, "deploy");
        assert_eq!(running.workflow_status(), "running");

        let done = reconstruct_state(&[
            we(workflow_event::Kind::WorkflowStart(WorkflowStart {
                run_id: "r".into(),
                workflow: "deploy".into(),
                ..Default::default()
            })),
            we(workflow_event::Kind::WorkflowComplete(
                WorkflowComplete::default(),
            )),
        ]);
        assert_eq!(done.workflow_status(), "done");

        let gated = reconstruct_state(&[we(workflow_event::Kind::WorkflowGate(WorkflowGate {
            node_id: "g".into(),
            ..Default::default()
        }))]);
        assert_eq!(gated.workflow_status(), "gate-paused");

        let failed = reconstruct_state(&[we(workflow_event::Kind::WorkflowError(WorkflowError {
            failed_node: "x".into(),
            ..Default::default()
        }))]);
        assert_eq!(failed.workflow_status(), "failed");
    }

    #[test]
    fn corrupt_json_output_falls_back_to_none_not_a_panic() {
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
                json_output: "{not valid json".into(),
                ..Default::default()
            })),
        ];
        let st = reconstruct_state(&evs);
        assert_eq!(st.node_status("a"), NodeStatus::Done);
        let out = st.node_outputs.get("a").unwrap();
        assert_eq!(
            out.json, None,
            "a corrupt json_output must still fall back to None, exactly as before this fix"
        );
        assert_eq!(
            out.text, "ok",
            "the rest of the NodeComplete payload must still fold in normally"
        );
    }
}

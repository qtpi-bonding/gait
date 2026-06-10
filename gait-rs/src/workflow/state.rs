use std::collections::HashMap;
use crate::proto::gait::log::v1::{event, payload, Event};
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

/// The `RawEvent` (`type`, original-JSON `payload`) for a workflow event, if any.
/// Workflow events are stored as opaque `RawEvent`s carrying the original tagged
/// JSON object verbatim (e.g. `{"type":"node_start","workflow_id":"w1",...}`).
fn raw_event(e: &Event) -> Option<(&str, serde_json::Value)> {
    match &e.body {
        Some(event::Body::Payload(p)) => match p.kind.as_ref() {
            Some(payload::Kind::Raw(r)) => {
                let v = match &r.payload {
                    Some(s) => serde_json::to_value(s).unwrap_or(serde_json::Value::Null),
                    None => serde_json::Value::Null,
                };
                Some((r.r#type.as_str(), v))
            }
            _ => None,
        },
        _ => None,
    }
}

pub fn reconstruct_state(events: &[Event]) -> WorkflowState {
    let mut state = WorkflowState {
        workflow_id: String::new(),
        node_outputs: HashMap::new(),
        node_statuses: HashMap::new(),
        failed_node: None,
        gate_paused: None,
    };
    for event in events {
        let Some((tag, v)) = raw_event(event) else { continue };
        let s = |key: &str| v.get(key).and_then(|x| x.as_str()).map(|s| s.to_string());
        match tag {
            "workflow_start" => {
                state.workflow_id = s("workflow_id").unwrap_or_default();
            }
            "node_start" => {
                if let Some(node_id) = s("node_id") {
                    state.set_status(&node_id, NodeStatus::Running);
                }
            }
            "node_complete" => {
                if let Some(node_id) = s("node_id") {
                    state.set_status(&node_id, NodeStatus::Done);
                    // pbjson Struct stores all numbers as f64, so accept either.
                    let exit_code = v.get("exit_code")
                        .and_then(|x| x.as_i64().or_else(|| x.as_f64().map(|f| f as i64)))
                        .map(|n| n as i32);
                    let json = v.get("json_output").cloned()
                        .filter(|x| !x.is_null());
                    let routed_to = s("routed_to");
                    state.node_outputs.insert(node_id, NodeOutput {
                        text: s("output").unwrap_or_default(),
                        exit_code,
                        json,
                        routed_to,
                    });
                }
            }
            "node_skip" => {
                if let Some(node_id) = s("node_id") {
                    state.set_status(&node_id, NodeStatus::Skipped);
                }
            }
            "node_error" => {
                if let Some(node_id) = s("node_id") {
                    state.set_status(&node_id, NodeStatus::Error(s("error").unwrap_or_default()));
                }
            }
            "workflow_gate" => {
                if let Some(node_id) = s("node_id") {
                    state.gate_paused = Some(node_id.clone());
                    state.set_status(&node_id, NodeStatus::Paused);
                }
            }
            "workflow_error" => {
                if let Some(failed_node) = s("failed_node") {
                    state.failed_node = Some(failed_node);
                }
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

    fn mint_events(aes: &[AnyEvent]) -> Vec<Event> {
        aes.iter()
            .map(|ae| crate::proto::envelope::mint(crate::proto::convert::any_to_body(ae), None))
            .collect()
    }

    #[test]
    fn reconstruct_from_events() {
        let events = mint_events(&[
            AnyEvent::WorkflowStart { workflow_id: "wf1".into(), name: "t".into(), yaml_path: "".into(), args: vec![], ts: 1 },
            AnyEvent::NodeStart { workflow_id: "wf1".into(), node_id: "n1".into(), node_type: "shell".into(), attempt: 1, ts: 2 },
            AnyEvent::NodeComplete { workflow_id: "wf1".into(), node_id: "n1".into(), output: "ok".into(), exit_code: Some(0), json_output: None, routed_to: None, ts: 3 },
            AnyEvent::NodeSkip { workflow_id: "wf1".into(), node_id: "n2".into(), reason: "inactive branch".into(), ts: 4 },
        ]);
        let state = reconstruct_state(&events);
        assert_eq!(state.workflow_id, "wf1");
        assert_eq!(state.node_status("n1"), NodeStatus::Done);
        assert_eq!(state.node_status("n2"), NodeStatus::Skipped);
        assert_eq!(state.node_status("n3"), NodeStatus::Pending);
        let out = state.node_outputs.get("n1").unwrap();
        assert_eq!(out.exit_code, Some(0));
    }
}

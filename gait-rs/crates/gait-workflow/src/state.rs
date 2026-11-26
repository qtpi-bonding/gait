use gait_core::proto::gait::workflow::v1::{workflow_event, NodeErrorKind, WorkflowEvent};
use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq)]
pub enum NodeStatus {
    Pending,
    Running,
    Done,
    Error(String),
    Paused,
}

#[derive(Debug, Clone, Default)]
pub struct WorkflowState {
    pub workflow_id: String,
    pub workflow_name: String,
    node_statuses: HashMap<String, NodeStatus>,
    pub failed_node: Option<String>,
    pub gate_paused: Option<String>,
    workflow_status: String,
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
        node_statuses: HashMap::new(),
        failed_node: None,
        gate_paused: None,
        workflow_status: "running".to_string(),
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
            }
            workflow_event::Kind::NodeComplete(nc) => {
                state.set_status(&nc.node_id, NodeStatus::Done);
                state.workflow_status = "running".to_string();
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

/// What the run log says about one script step id.
#[derive(Debug, Clone, PartialEq)]
pub enum JournalState {
    /// NodeStart with no terminal event (crashed mid-step).
    Open,
    /// NodeComplete: replay returns the recorded result.
    Complete {
        session_id: String,
        output: String,
        exit_code: Option<i32>,
        json_output: String,
    },
    /// NodeError with kind FAILED or CONTINUED: replays as an `{error}`
    /// result (a failed node counts as finished).
    Errored { error: String },
    /// NodeError with kind CANCELLED: not terminal; the step runs again.
    Cancelled,
}

/// One script step's journal record, keyed by node id in [`ScriptJournal`].
#[derive(Debug, Clone, PartialEq)]
pub struct JournalEntry {
    /// `NodeStart.node_type`: `"shell"`, `"turn"`, `"now"`, `"random"`.
    pub kind: String,
    /// Session id recorded by `NodeStart` (or a later `NodeComplete`).
    pub session_id: String,
    pub interactive: bool,
    /// Whether a WorkflowGate event was recorded after this gate's NodeStart.
    /// An open gate without this event was interrupted while draining and has
    /// not yet been shown to the user.
    pub gate_announced: bool,
    /// What the log's last terminal event for this id says.
    pub state: JournalState,
}

/// Pure replay journal folded from a `WorkflowEvent` run log. Unlike
/// [`reconstruct_state`] it tracks per-step-id terminal disposition, including
/// CANCELLED (not terminal) and OPEN (crashed mid-step) for the crash guard.
#[derive(Debug, Clone, Default)]
pub struct ScriptJournal {
    entries: HashMap<String, JournalEntry>,
    workflow_start_seen: bool,
}

impl ScriptJournal {
    /// Fold the events in order into per-step entries.
    pub fn from_events(events: &[WorkflowEvent]) -> Self {
        let mut journal = ScriptJournal::default();
        for event in events {
            let Some(kind) = &event.kind else {
                continue;
            };
            match kind {
                workflow_event::Kind::WorkflowStart(_) => {
                    journal.workflow_start_seen = true;
                }
                workflow_event::Kind::NodeStart(ns) => {
                    // A later NodeStart for the same id (force-rerun, or a
                    // re-run after Cancelled) resets the entry; the new kind
                    // and session_id win.
                    journal.entries.insert(
                        ns.node_id.clone(),
                        JournalEntry {
                            kind: ns.node_type.clone(),
                            session_id: ns.session_id.clone(),
                            interactive: ns.interactive,
                            gate_announced: false,
                            state: JournalState::Open,
                        },
                    );
                }
                workflow_event::Kind::NodeComplete(nc) => {
                    let Some(entry) = journal.entries.get_mut(&nc.node_id) else {
                        continue;
                    };
                    if !nc.session_id.is_empty() {
                        entry.session_id = nc.session_id.clone();
                    }
                    entry.state = JournalState::Complete {
                        session_id: entry.session_id.clone(),
                        output: nc.output.clone(),
                        exit_code: nc.exit_code,
                        json_output: nc.json_output.clone(),
                    };
                }
                workflow_event::Kind::NodeError(ne) => {
                    let Some(entry) = journal.entries.get_mut(&ne.node_id) else {
                        continue;
                    };
                    // Unknown enum values decode as FAILED, matching the
                    // proto3 default for old log lines without `kind`.
                    entry.state = match NodeErrorKind::try_from(ne.kind) {
                        Ok(NodeErrorKind::Cancelled) => JournalState::Cancelled,
                        _ => JournalState::Errored {
                            error: ne.error.clone(),
                        },
                    };
                }
                workflow_event::Kind::WorkflowGate(wg) => {
                    // Only mark the current open entry.  In particular, a
                    // stale/malformed gate event must not announce a later
                    // NodeStart for the same id.
                    if let Some(entry) = journal.entries.get_mut(&wg.node_id) {
                        if entry.state == JournalState::Open && entry.kind == "gate" {
                            entry.gate_announced = true;
                        }
                    }
                }
                _ => {}
            }
        }
        journal
    }

    /// The journal record for a step id, if any.
    pub fn get(&self, id: &str) -> Option<&JournalEntry> {
        self.entries.get(id)
    }

    /// Iterate over all journal entries (id, entry), in arbitrary order.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &JournalEntry)> {
        self.entries.iter()
    }

    /// Ids whose state is Open, sorted, for the crash guard.
    pub fn open_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self
            .entries
            .iter()
            .filter(|(_, e)| e.state == JournalState::Open)
            .map(|(id, _)| id.clone())
            .collect();
        ids.sort();
        ids
    }

    /// Whether a WorkflowStart event was seen in the log.
    pub fn has_workflow_start(&self) -> bool {
        self.workflow_start_seen
    }
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
    fn journal_preserves_interactive_flag() {
        let events = vec![we(workflow_event::Kind::NodeStart(NodeStart {
            node_id: "chat".into(),
            node_type: "turn".into(),
            interactive: true,
            ..Default::default()
        }))];
        let journal = ScriptJournal::from_events(&events);
        assert!(journal.get("chat").unwrap().interactive);
    }

    #[test]
    fn journal_marks_gate_as_announced_only_after_workflow_gate() {
        let start = we(workflow_event::Kind::NodeStart(NodeStart {
            node_id: "review".into(),
            node_type: "gate".into(),
            ..Default::default()
        }));
        let unannounced = ScriptJournal::from_events(std::slice::from_ref(&start));
        assert!(!unannounced.get("review").unwrap().gate_announced);

        let announced = ScriptJournal::from_events(&[
            start,
            we(workflow_event::Kind::WorkflowGate(WorkflowGate {
                node_id: "review".into(),
                ..Default::default()
            })),
        ]);
        assert!(announced.get("review").unwrap().gate_announced);
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
        ];
        let state = reconstruct_state(&evs);
        assert_eq!(state.workflow_id, "wf1");
        assert_eq!(state.node_status("n1"), NodeStatus::Done);
        assert_eq!(state.node_status("n3"), NodeStatus::Pending);
        assert_eq!(state.open_node(), None); // the only node completed
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
    fn journal_complete_after_start_carries_all_fields() {
        let evs = vec![
            we(workflow_event::Kind::WorkflowStart(WorkflowStart {
                run_id: "r".into(),
                workflow: "t".into(),
                ..Default::default()
            })),
            we(workflow_event::Kind::NodeStart(NodeStart {
                node_id: "a".into(),
                node_type: "shell".into(),
                session_id: "s1".into(),
                ..Default::default()
            })),
            we(workflow_event::Kind::NodeComplete(NodeComplete {
                node_id: "a".into(),
                session_id: "s2".into(),
                output: "ok".into(),
                exit_code: Some(0),
                json_output: "{\"v\":1}".into(),
                ..Default::default()
            })),
        ];
        let j = ScriptJournal::from_events(&evs);
        assert!(j.has_workflow_start());
        assert_eq!(j.open_ids(), Vec::<String>::new());
        let e = j.get("a").expect("entry for a");
        assert_eq!(e.kind, "shell");
        // NodeComplete's non-empty session_id wins over the NodeStart's.
        assert_eq!(
            e.state,
            JournalState::Complete {
                session_id: "s2".into(),
                output: "ok".into(),
                exit_code: Some(0),
                json_output: "{\"v\":1}".into(),
            }
        );
    }

    #[test]
    fn journal_start_only_is_open_and_listed() {
        let evs = vec![we(workflow_event::Kind::NodeStart(NodeStart {
            node_id: "a".into(),
            node_type: "turn".into(),
            session_id: "s1".into(),
            ..Default::default()
        }))];
        let j = ScriptJournal::from_events(&evs);
        assert!(!j.has_workflow_start());
        assert_eq!(j.open_ids(), vec!["a".to_string()]);
        let e = j.get("a").unwrap();
        assert_eq!(e.kind, "turn");
        assert_eq!(e.session_id, "s1");
        assert_eq!(e.state, JournalState::Open);
    }

    #[test]
    fn journal_node_error_kinds_map_to_errored_or_cancelled() {
        let evs = vec![
            we(workflow_event::Kind::NodeStart(NodeStart {
                node_id: "f".into(),
                ..Default::default()
            })),
            we(workflow_event::Kind::NodeStart(NodeStart {
                node_id: "c".into(),
                ..Default::default()
            })),
            we(workflow_event::Kind::NodeStart(NodeStart {
                node_id: "x".into(),
                ..Default::default()
            })),
            we(workflow_event::Kind::NodeError(NodeError {
                node_id: "f".into(),
                error: "boom".into(),
                kind: NodeErrorKind::Failed as i32,
                ..Default::default()
            })),
            we(workflow_event::Kind::NodeError(NodeError {
                node_id: "c".into(),
                error: "skipped past".into(),
                kind: NodeErrorKind::Continued as i32,
                ..Default::default()
            })),
            we(workflow_event::Kind::NodeError(NodeError {
                node_id: "x".into(),
                error: "user cancelled".into(),
                kind: NodeErrorKind::Cancelled as i32,
                ..Default::default()
            })),
        ];
        let j = ScriptJournal::from_events(&evs);
        assert_eq!(
            j.get("f").unwrap().state,
            JournalState::Errored {
                error: "boom".into()
            }
        );
        // CONTINUED counts as finished, like FAILED.
        assert_eq!(
            j.get("c").unwrap().state,
            JournalState::Errored {
                error: "skipped past".into()
            }
        );
        // CANCELLED is not terminal, but it is not Open either: the step
        // just runs again on the next pass.
        assert_eq!(j.get("x").unwrap().state, JournalState::Cancelled);
        assert_eq!(j.open_ids(), Vec::<String>::new());
    }

    #[test]
    fn journal_cancelled_then_fresh_start_is_open_again() {
        let evs = vec![
            we(workflow_event::Kind::NodeStart(NodeStart {
                node_id: "a".into(),
                node_type: "shell".into(),
                ..Default::default()
            })),
            we(workflow_event::Kind::NodeError(NodeError {
                node_id: "a".into(),
                kind: NodeErrorKind::Cancelled as i32,
                ..Default::default()
            })),
            // A re-run after CANCELLED resets the entry; the new kind wins.
            we(workflow_event::Kind::NodeStart(NodeStart {
                node_id: "a".into(),
                node_type: "turn".into(),
                session_id: "s9".into(),
                ..Default::default()
            })),
        ];
        let j = ScriptJournal::from_events(&evs);
        let e = j.get("a").unwrap();
        assert_eq!(e.kind, "turn");
        assert_eq!(e.session_id, "s9");
        assert_eq!(e.state, JournalState::Open);
        assert_eq!(j.open_ids(), vec!["a".to_string()]);
    }

    #[test]
    fn journal_interleaved_ids_stay_independent() {
        let evs = vec![
            we(workflow_event::Kind::NodeStart(NodeStart {
                node_id: "a".into(),
                node_type: "shell".into(),
                ..Default::default()
            })),
            we(workflow_event::Kind::NodeStart(NodeStart {
                node_id: "b".into(),
                node_type: "turn".into(),
                ..Default::default()
            })),
            we(workflow_event::Kind::NodeComplete(NodeComplete {
                node_id: "a".into(),
                output: "done-a".into(),
                ..Default::default()
            })),
            we(workflow_event::Kind::NodeError(NodeError {
                node_id: "b".into(),
                error: "boom".into(),
                ..Default::default()
            })),
        ];
        let j = ScriptJournal::from_events(&evs);
        assert_eq!(
            j.get("a").unwrap().state,
            JournalState::Complete {
                session_id: String::new(),
                output: "done-a".into(),
                exit_code: None,
                json_output: String::new(),
            }
        );
        assert_eq!(
            j.get("b").unwrap().state,
            JournalState::Errored {
                error: "boom".into()
            }
        );
        assert_eq!(j.open_ids(), Vec::<String>::new());
    }

    #[test]
    fn journal_ignores_events_for_unknown_ids() {
        let evs = vec![
            we(workflow_event::Kind::NodeComplete(NodeComplete {
                node_id: "ghost".into(),
                output: "no start".into(),
                ..Default::default()
            })),
            we(workflow_event::Kind::NodeError(NodeError {
                node_id: "phantom".into(),
                error: "no start".into(),
                ..Default::default()
            })),
        ];
        let j = ScriptJournal::from_events(&evs);
        assert!(j.get("ghost").is_none());
        assert!(j.get("phantom").is_none());
        assert!(j.open_ids().is_empty());
    }
}

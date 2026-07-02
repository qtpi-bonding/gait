//! Envelope mint for the workflow run log — the WorkflowEvent analog of
//! `envelope::mint`. Own schema (gait.workflow.v1), same shape: id/seq/parents/
//! content_hash recorded, never verified on read.

use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::json;

use crate::gait::workflow::v1::{workflow_event, WorkflowEvent};
use crate::hash::content_hash;

/// camelCase oneof arm keys (pbjson flattens `kind` to the top level).
const ARMS: &[&str] = &[
    "workflowStart",
    "nodeStart",
    "nodeComplete",
    "nodeError",
    "nodeSkip",
    "workflowGate",
    "workflowComplete",
    "workflowError",
    "hookRun",
];

/// The externally-tagged body object `{ "<arm>": {…} }` that content_hash covers.
pub fn body_json(ev: &WorkflowEvent) -> serde_json::Value {
    let full = serde_json::to_value(ev).expect("serialize workflow event");
    for arm in ARMS {
        if let Some(b) = full.get(*arm) {
            return json!({ *arm: b });
        }
    }
    panic!("workflow event has no kind arm set: {full}");
}

/// Mint an envelope around `kind`, chained after `prev` (None = root).
/// content_hash is stamped LAST over the kind arm.
pub fn mint(
    version: u32,
    kind: workflow_event::Kind,
    prev: Option<&WorkflowEvent>,
) -> WorkflowEvent {
    let (seq, parents) = match prev {
        None => (1u64, Vec::new()),
        Some(p) => (p.seq + 1, vec![p.id.clone()]),
    };
    let mut ev = WorkflowEvent {
        version,
        id: uuid::Uuid::new_v4().to_string(),
        seq,
        ts: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
        content_hash: String::new(),
        parents,
        kind: Some(kind),
    };
    ev.content_hash = content_hash(ev.version, &body_json(&ev));
    ev
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gait::workflow::v1::{workflow_event::Kind, NodeStart};
    use crate::hash::content_hash;

    #[test]
    fn mint_chains_seq_parents_and_recomputes_hash() {
        let mk = |id: &str| {
            Kind::NodeStart(NodeStart {
                node_id: id.into(),
                node_type: "shell".into(),
                attempt: 1,
                session_id: "s".into(),
            })
        };
        let root = mint(1, mk("a"), None);
        assert_eq!(root.seq, 1);
        assert!(root.parents.is_empty());
        assert!(!root.id.is_empty());
        assert_eq!(
            root.content_hash,
            content_hash(root.version, &body_json(&root))
        );

        let child = mint(1, mk("b"), Some(&root));
        assert_eq!(child.seq, 2);
        assert_eq!(child.parents, vec![root.id.clone()]);
    }
}

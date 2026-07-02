//! Append-only pbjson event log for a single workflow run.
//!
//! File layout: one [`WorkflowEvent`] per line in
//! `{workflow_runs_dir}/{run_id}/log.jsonl` (JSON via pbjson, camelCase).
//! Single-writer, append-only. State for a run is reconstructed by replaying
//! events.
//!
//! The pattern mirrors the session transcript (`gait_core::proto::wire::append`)
//! but skips the `Event` envelope: this log is not a Merkle chain, so events
//! are just serialized `WorkflowEvent` JSON values appended line-by-line.

use std::io::Write;
use std::path::{Path, PathBuf};

use gait_core::proto::gait::workflow::v1::{
    NodeComplete, NodeError, NodeSkip, NodeStart, Workflow, WorkflowComplete, WorkflowError,
    WorkflowEvent, WorkflowGate, WorkflowStart,
};

/// Current format generation for the workflow run log. Mirrors the contract
/// used by `gait.log.v1` (bump on breaking schema changes).
pub const WORKFLOW_LOG_VERSION: u32 = 1;

/// A handle to an open workflow-run log file. Use [`WorkflowRunLog::create`]
/// to start a new run, or [`WorkflowRunLog::open`] to attach to an existing
/// run (e.g. on resume after a gate pause).
pub struct WorkflowRunLog {
    path: PathBuf,
}

impl WorkflowRunLog {
    /// Create a new workflow-run log at `{workflow_runs_dir}/{run_id}/log.jsonl`.
    /// The run directory is created if it does not exist; the log file itself
    /// is created exclusively (`O_CREAT | O_EXCL`) — if it already exists,
    /// this returns an error rather than truncating a live run.
    pub fn create(workflow_runs_dir: &Path, run_id: &str) -> anyhow::Result<Self> {
        let run_dir = workflow_runs_dir.join(run_id);
        std::fs::create_dir_all(&run_dir)?;
        let path = run_dir.join("log.jsonl");
        // O_EXCL: fail loudly rather than truncating a live run on ID collision.
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| {
                anyhow::anyhow!(
                    "workflow run log already exists at {}: {}",
                    path.display(),
                    e
                )
            })?;
        Ok(Self { path })
    }

    /// Open an existing workflow-run log at `{workflow_runs_dir}/{run_id}/log.jsonl`
    /// for append-only writes. The file must already exist; use [`create`] to
    /// start a new run.
    pub fn open(workflow_runs_dir: &Path, run_id: &str) -> anyhow::Result<Self> {
        let path = workflow_runs_dir.join(run_id).join("log.jsonl");
        if !path.exists() {
            anyhow::bail!("workflow run log not found: {}", path.display());
        }
        Ok(Self { path })
    }

    /// Path to the underlying log file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read all events from the log. Blank lines are skipped.
    pub fn read_events(&self) -> anyhow::Result<Vec<WorkflowEvent>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let text = std::fs::read_to_string(&self.path)?;
        let mut events = Vec::new();
        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            events.push(serde_json::from_str::<WorkflowEvent>(line)?);
        }
        Ok(events)
    }

    // -- writers ------------------------------------------------------------

    /// Append a workflow start event. The `run_id` should be the same id used
    /// to create the log directory.
    pub fn workflow_start(
        &self,
        run_id: &str,
        workflow: &str,
        yaml_path: &str,
        args: &[String],
        def: &Workflow,
    ) -> anyhow::Result<WorkflowEvent> {
        let ev = WorkflowEvent {
            version: WORKFLOW_LOG_VERSION,
            ts: now_ms(),
            id: String::new(),
            seq: 0,
            content_hash: String::new(),
            parents: vec![],
            kind: Some(
                gait_core::proto::gait::workflow::v1::workflow_event::Kind::WorkflowStart(
                    WorkflowStart {
                        run_id: run_id.to_string(),
                        workflow: workflow.to_string(),
                        yaml_path: yaml_path.to_string(),
                        args: args.to_vec(),
                        def: Some(def.clone()),
                    },
                ),
            ),
        };
        self.append(&ev)?;
        Ok(ev)
    }

    /// Append a node start event.
    pub fn node_start(
        &self,
        node_id: &str,
        node_type: &str,
        attempt: u32,
        session_id: &str,
    ) -> anyhow::Result<WorkflowEvent> {
        let ev = WorkflowEvent {
            version: WORKFLOW_LOG_VERSION,
            ts: now_ms(),
            id: String::new(),
            seq: 0,
            content_hash: String::new(),
            parents: vec![],
            kind: Some(
                gait_core::proto::gait::workflow::v1::workflow_event::Kind::NodeStart(NodeStart {
                    node_id: node_id.to_string(),
                    node_type: node_type.to_string(),
                    attempt,
                    session_id: session_id.to_string(),
                }),
            ),
        };
        self.append(&ev)?;
        Ok(ev)
    }

    /// Append a node complete event.
    pub fn node_complete(
        &self,
        node_id: &str,
        session_id: &str,
        output: &str,
        exit_code: Option<i32>,
        json_output: &str,
        routed_to: &str,
    ) -> anyhow::Result<WorkflowEvent> {
        let ev = WorkflowEvent {
            version: WORKFLOW_LOG_VERSION,
            ts: now_ms(),
            id: String::new(),
            seq: 0,
            content_hash: String::new(),
            parents: vec![],
            kind: Some(
                gait_core::proto::gait::workflow::v1::workflow_event::Kind::NodeComplete(
                    NodeComplete {
                        node_id: node_id.to_string(),
                        session_id: session_id.to_string(),
                        output: output.to_string(),
                        exit_code,
                        json_output: json_output.to_string(),
                        routed_to: routed_to.to_string(),
                    },
                ),
            ),
        };
        self.append(&ev)?;
        Ok(ev)
    }

    /// Append a node error event.
    pub fn node_error(
        &self,
        node_id: &str,
        error: &str,
        attempt: u32,
    ) -> anyhow::Result<WorkflowEvent> {
        let ev = WorkflowEvent {
            version: WORKFLOW_LOG_VERSION,
            ts: now_ms(),
            id: String::new(),
            seq: 0,
            content_hash: String::new(),
            parents: vec![],
            kind: Some(
                gait_core::proto::gait::workflow::v1::workflow_event::Kind::NodeError(NodeError {
                    node_id: node_id.to_string(),
                    error: error.to_string(),
                    attempt,
                }),
            ),
        };
        self.append(&ev)?;
        Ok(ev)
    }

    /// Append a node skip event (e.g. inactive branch path).
    pub fn node_skip(&self, node_id: &str, reason: &str) -> anyhow::Result<WorkflowEvent> {
        let ev = WorkflowEvent {
            version: WORKFLOW_LOG_VERSION,
            ts: now_ms(),
            id: String::new(),
            seq: 0,
            content_hash: String::new(),
            parents: vec![],
            kind: Some(
                gait_core::proto::gait::workflow::v1::workflow_event::Kind::NodeSkip(NodeSkip {
                    node_id: node_id.to_string(),
                    reason: reason.to_string(),
                }),
            ),
        };
        self.append(&ev)?;
        Ok(ev)
    }

    /// Append a workflow gate pause event.
    pub fn workflow_gate(&self, node_id: &str, message: &str) -> anyhow::Result<WorkflowEvent> {
        let ev = WorkflowEvent {
            version: WORKFLOW_LOG_VERSION,
            ts: now_ms(),
            id: String::new(),
            seq: 0,
            content_hash: String::new(),
            parents: vec![],
            kind: Some(
                gait_core::proto::gait::workflow::v1::workflow_event::Kind::WorkflowGate(
                    WorkflowGate {
                        node_id: node_id.to_string(),
                        message: message.to_string(),
                    },
                ),
            ),
        };
        self.append(&ev)?;
        Ok(ev)
    }

    /// Append a workflow complete event.
    pub fn workflow_complete(&self, run_id: &str) -> anyhow::Result<WorkflowEvent> {
        let ev = WorkflowEvent {
            version: WORKFLOW_LOG_VERSION,
            ts: now_ms(),
            id: String::new(),
            seq: 0,
            content_hash: String::new(),
            parents: vec![],
            kind: Some(
                gait_core::proto::gait::workflow::v1::workflow_event::Kind::WorkflowComplete(
                    WorkflowComplete {
                        run_id: run_id.to_string(),
                    },
                ),
            ),
        };
        self.append(&ev)?;
        Ok(ev)
    }

    /// Append a workflow error event (terminal failure).
    pub fn workflow_error(
        &self,
        run_id: &str,
        failed_node: &str,
        error: &str,
    ) -> anyhow::Result<WorkflowEvent> {
        let ev = WorkflowEvent {
            version: WORKFLOW_LOG_VERSION,
            ts: now_ms(),
            id: String::new(),
            seq: 0,
            content_hash: String::new(),
            parents: vec![],
            kind: Some(
                gait_core::proto::gait::workflow::v1::workflow_event::Kind::WorkflowError(
                    WorkflowError {
                        run_id: run_id.to_string(),
                        failed_node: failed_node.to_string(),
                        error: error.to_string(),
                    },
                ),
            ),
        };
        self.append(&ev)?;
        Ok(ev)
    }

    /// Serialize `ev` as one JSON line and append to the log file.
    fn append(&self, ev: &WorkflowEvent) -> anyhow::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        let json = serde_json::to_string(ev)?;
        writeln!(file, "{}", json)?;
        Ok(())
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn log_roundtrips_workflow_start() {
        let dir = TempDir::new().unwrap();
        let log = WorkflowRunLog::create(dir.path(), "test-run-20260616-120000").unwrap();
        let def = gait_core::proto::gait::workflow::v1::Workflow::default();
        log.workflow_start(
            "test-run-20260616-120000",
            "test-workflow",
            "workflows/test.yaml",
            &[],
            &def,
        )
        .unwrap(); // return value (WorkflowEvent) is intentionally discarded in tests
        let events = log.read_events().unwrap();
        assert_eq!(events.len(), 1);
        assert!(events[0].kind.is_some());
    }

    #[test]
    fn workflow_event_has_envelope_fields() {
        let ev = gait_core::proto::gait::workflow::v1::WorkflowEvent {
            version: WORKFLOW_LOG_VERSION,
            id: "abc".into(),
            seq: 1,
            ts: 0,
            content_hash: String::new(),
            parents: vec![],
            kind: None,
        };
        assert_eq!(ev.seq, 1);
        assert_eq!(ev.id, "abc");
    }

    #[test]
    fn hook_run_is_a_typed_kind() {
        use gait_core::proto::gait::workflow::v1::{workflow_event::Kind, WorkflowHookRun};
        let k = Kind::HookRun(WorkflowHookRun {
            node_id: "n".into(),
            hook_type: "before".into(),
            command: "echo hi".into(),
            exit_code: 0,
        });
        assert!(matches!(k, Kind::HookRun(_)));
    }
}

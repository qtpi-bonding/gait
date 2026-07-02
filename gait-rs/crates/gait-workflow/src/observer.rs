//! Output observer seam — replaces the hardcoded `CliSink` drain.
//!
//! [`WorkflowObserver`] is a synchronous callback interface called from the
//! workflow runner at every significant event. Two implementations are
//! provided:
//!
//! - [`CliObserver`]: prints terse workflow-event lines to stderr.
//! - [`ChannelObserver`]: forwards events to a `tokio::sync::mpsc` channel for
//!   programmatic consumers (tests, embedding).
//! - [`NullObserver`]: discards every event (used by the REPL).
//!
//! Turn output (deltas/events/results) no longer flows here — it goes through
//! the caller-supplied [`gait_core::embed::TurnSink`] handed to `run_workflow`.

use gait_core::proto::gait::workflow::v1::{workflow_event, WorkflowEvent};

// ─── trait ────────────────────────────────────────────────────────────────────

/// Synchronous observer for workflow runner output.
///
/// Called inline from the runner after each log write. Stream items are no
/// longer delivered here — turn output flows through the caller-supplied
/// `TurnSink` instead. The observer sees exactly one `on_event` call per
/// terminal event (start, complete, skip, error, etc.).
pub trait WorkflowObserver {
    /// Called after a workflow event has been appended to the run-log.
    fn on_event(&mut self, ev: &WorkflowEvent);
}

// ─── CliObserver ──────────────────────────────────────────────────────────────

/// CLI observer: workflow lifecycle events → terse human-readable lines on
/// stderr. Stream items are handled by the `TurnSink`, not here.
#[derive(Default)]
pub struct CliObserver;

impl WorkflowObserver for CliObserver {
    fn on_event(&mut self, ev: &WorkflowEvent) {
        match &ev.kind {
            Some(workflow_event::Kind::WorkflowStart(s)) => {
                eprintln!("[gait workflow] start: {}", s.workflow);
            }
            Some(workflow_event::Kind::NodeStart(s)) => {
                eprintln!("[gait workflow] node start: {}", s.node_id);
            }
            Some(workflow_event::Kind::NodeComplete(s)) => {
                eprintln!("[gait workflow] node done: {}", s.node_id);
            }
            Some(workflow_event::Kind::NodeSkip(s)) => {
                eprintln!("[gait workflow] node skip: {}", s.node_id);
            }
            Some(workflow_event::Kind::NodeError(s)) => {
                eprintln!("[gait workflow] node error: {} — {}", s.node_id, s.error);
            }
            Some(workflow_event::Kind::WorkflowGate(s)) => {
                eprintln!("[gait workflow] gate: {}", s.node_id);
            }
            Some(workflow_event::Kind::WorkflowComplete(s)) => {
                eprintln!("[gait workflow] complete: {}", s.run_id);
            }
            Some(workflow_event::Kind::WorkflowError(s)) => {
                eprintln!("[gait workflow] error: {} at {}", s.error, s.failed_node);
            }
            Some(workflow_event::Kind::HookRun(_)) => {}
            None => {}
        }
    }
}

// ─── ChannelObserver ──────────────────────────────────────────────────────────

/// Channel observer: forwards lifecycle events to an unbounded mpsc sender.
/// The sender may be `None` to discard events.
pub struct ChannelObserver {
    pub event_tx: Option<tokio::sync::mpsc::UnboundedSender<WorkflowEvent>>,
}

impl ChannelObserver {
    /// Create a `ChannelObserver` that forwards events to the given channel.
    pub fn new(event_tx: tokio::sync::mpsc::UnboundedSender<WorkflowEvent>) -> Self {
        Self {
            event_tx: Some(event_tx),
        }
    }
}

impl WorkflowObserver for ChannelObserver {
    fn on_event(&mut self, ev: &WorkflowEvent) {
        if let Some(tx) = &self.event_tx {
            let _ = tx.send(ev.clone());
        }
    }
}

// ─── NullObserver ──────────────────────────────────────────────────────────────

/// A `WorkflowObserver` that discards every lifecycle event. Used by the REPL,
/// which renders turn output through its `TurnSink` and wants no `[gait workflow]`
/// status lines polluting the chat surface.
#[derive(Default)]
pub struct NullObserver;

impl WorkflowObserver for NullObserver {
    fn on_event(&mut self, _ev: &WorkflowEvent) {}
}

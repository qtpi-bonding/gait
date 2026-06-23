//! Output observer seam — replaces the hardcoded `CliSink` drain.
//!
//! [`WorkflowObserver`] is a synchronous callback interface called from the
//! workflow runner at every significant event. Two implementations are
//! provided:
//!
//! - [`CliObserver`]: writes stream items to stderr (matching prior
//!   `CliSink` behavior) and prints terse event lines.
//! - [`ChannelObserver`]: forwards to `tokio::sync::mpsc` channels for
//!   programmatic consumers (tests, embedding).

use gait_core::proto::gait::log::v1::StreamItem;
use gait_core::proto::gait::workflow::v1::{workflow_event, WorkflowEvent};

// ─── trait ────────────────────────────────────────────────────────────────────

/// Synchronous observer for workflow runner output.
///
/// Called inline from the runner after each log write and after each
/// `try_recv` drain of the `StreamItem` channel. Order within a single
/// node's lifetime is: zero or more `on_stream_item` calls interleaved with
/// async yields, followed by exactly one `on_event` call for the terminal
/// event (start, complete, skip, error, etc.).
pub trait WorkflowObserver {
    /// Called after a workflow event has been appended to the run-log.
    fn on_event(&mut self, ev: &WorkflowEvent);
    /// Called for each `StreamItem` drained from the node task channel.
    fn on_stream_item(&mut self, item: &StreamItem);
}

// ─── CliObserver ──────────────────────────────────────────────────────────────

/// CLI observer: stream items → stderr (JSON, matching prior `CliSink`
/// behavior), workflow events → terse human-readable lines on stderr.
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
            None => {}
        }
    }

    fn on_stream_item(&mut self, item: &StreamItem) {
        if let Ok(j) = serde_json::to_string(item) {
            eprintln!("{j}");
        }
    }
}

// ─── ChannelObserver ──────────────────────────────────────────────────────────

/// Channel observer: forwards events and stream items to unbounded mpsc
/// senders. Either sender may be `None` to discard that category.
pub struct ChannelObserver {
    pub event_tx: Option<tokio::sync::mpsc::UnboundedSender<WorkflowEvent>>,
    pub item_tx: Option<tokio::sync::mpsc::UnboundedSender<StreamItem>>,
}

impl ChannelObserver {
    /// Create a `ChannelObserver` that forwards to both channels.
    pub fn new(
        event_tx: tokio::sync::mpsc::UnboundedSender<WorkflowEvent>,
        item_tx: tokio::sync::mpsc::UnboundedSender<StreamItem>,
    ) -> Self {
        Self {
            event_tx: Some(event_tx),
            item_tx: Some(item_tx),
        }
    }

    /// Create a `ChannelObserver` that only captures events (items discarded).
    pub fn events_only(event_tx: tokio::sync::mpsc::UnboundedSender<WorkflowEvent>) -> Self {
        Self {
            event_tx: Some(event_tx),
            item_tx: None,
        }
    }
}

impl WorkflowObserver for ChannelObserver {
    fn on_event(&mut self, ev: &WorkflowEvent) {
        if let Some(tx) = &self.event_tx {
            let _ = tx.send(ev.clone());
        }
    }

    fn on_stream_item(&mut self, item: &StreamItem) {
        if let Some(tx) = &self.item_tx {
            let _ = tx.send(item.clone());
        }
    }
}

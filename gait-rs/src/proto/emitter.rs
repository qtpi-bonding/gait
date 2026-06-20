use crate::proto::{
    gait::log::v1::{event, stream_item, Event, StreamItem, TextDelta},
    wire,
};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc::UnboundedSender;

pub struct Recorder {
    pub tx_path: PathBuf,
    sender: UnboundedSender<StreamItem>,
    /// (turn, step) stamped onto every event produced by `record`.
    /// Defaults to (0, 0); updated via [`set_position`] as the turn loop advances.
    position: Arc<Mutex<(u32, u32)>>,
}

impl Recorder {
    pub fn new(tx_path: PathBuf, sender: UnboundedSender<StreamItem>) -> Self {
        Self {
            tx_path,
            sender,
            position: Arc::new(Mutex::new((0, 0))),
        }
    }

    /// Stamp subsequent `record` calls with this (turn, step). Reads the
    /// slot under a Mutex so a setter from another task can update it.
    pub fn set_position(&self, turn: u32, step: u32) {
        *self.position.lock().expect("position mutex poisoned") = (turn, step);
    }

    /// Construct from a session layout: writes to `<gait_dir>/sessions/<id>/log.jsonl`.
    /// Mirror of [`Ingestor::from_session`](crate::session::Ingestor::from_session).
    pub fn for_session(
        gait_dir: &Path,
        session_id: &str,
        sender: UnboundedSender<StreamItem>,
    ) -> Self {
        Self::new(
            gait_dir.join("sessions").join(session_id).join("log.jsonl"),
            sender,
        )
    }

    /// A no-op recorder for tests that need a `&Recorder` but don't assert on emission. Records are
    /// dropped: the channel has no receiver and `tx_path` is empty (writes fail
    /// silently). Do NOT use where the transcript/channel matters — use
    /// [`for_session`](Self::for_session).
    pub fn detached() -> Self {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        Self::new(std::path::PathBuf::new(), tx)
    }

    /// Durable: write Event to the log AND send StreamItem{event}. Always both.
    /// `UnboundedSender::send` is sync + non-blocking — safe from anywhere.
    pub fn record(&self, body: event::Body) -> anyhow::Result<Event> {
        let (turn, step) = *self.position.lock().expect("position mutex poisoned");
        let event = wire::write_to_transcript(&self.tx_path, body, turn, step)?;
        // receiver dropped (e.g. headless / test) — transcript write already succeeded, so ignore.
        let _ = self.sender.send(StreamItem {
            item: Some(stream_item::Item::Event(event.clone())),
        });
        Ok(event)
    }

    /// Borrow the channel sender — e.g. for a child `Recorder` (a workflow sub-turn)
    /// that should publish to the same channel. A clone of this lives only as long as
    /// the child; the owning `Recorder` stays the single long-lived sender, so dropping
    /// it closes the channel (and lets a spawned `drive` consumer finish).
    pub fn sender(&self) -> &UnboundedSender<StreamItem> {
        &self.sender
    }

    /// Ephemeral: send StreamItem{delta} only. No transcript write.
    pub fn publish_delta(&self, content: String) {
        // channel-only; if the receiver is gone there's nothing to persist, so ignore.
        let _ = self.sender.send(StreamItem {
            item: Some(stream_item::Item::Delta(TextDelta { content })),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::gait::log::v1::stream_item::Item;
    use tempfile::tempdir;
    use tokio::sync::mpsc;

    fn make(dir: &std::path::Path) -> (Recorder, mpsc::UnboundedReceiver<StreamItem>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Recorder::new(dir.join("log.jsonl"), tx), rx)
    }

    #[test]
    fn for_session_builds_log_path() {
        let dir = tempdir().unwrap();
        let (tx, _rx) = mpsc::unbounded_channel();
        let em = Recorder::for_session(dir.path(), "s1234567", tx);
        assert_eq!(em.tx_path, dir.path().join("sessions/s1234567/log.jsonl"));
    }

    #[test]
    fn record_writes_file_and_sends_event() {
        let dir = tempdir().unwrap();
        let (em, mut rx) = make(dir.path());
        let returned = em
            .record(crate::proto::events::session_start(
                "s1234567".to_string(),
                None,
            ))
            .unwrap();
        assert!(em.tx_path.exists());
        match rx.try_recv().unwrap().item {
            Some(Item::Event(ev)) => assert_eq!(ev.id, returned.id),
            other => panic!("expected Event, got {other:?}"),
        }
    }

    #[test]
    fn publish_delta_sends_only_channel_no_file() {
        let dir = tempdir().unwrap();
        let (em, mut rx) = make(dir.path());
        em.publish_delta("hi".to_string());
        assert!(!em.tx_path.exists());
        match rx.try_recv().unwrap().item {
            Some(Item::Delta(d)) => assert_eq!(d.content, "hi"),
            other => panic!("expected Delta, got {other:?}"),
        }
    }
    #[test]
    fn detached_record_is_a_noop() {
        let rec = Recorder::detached();
        // record returns Err (nowhere to write) and must not panic or create a file
        assert!(rec.record(crate::proto::events::cwd("/tmp")).is_err());
    }

    #[test]
    fn set_position_stamps_turn_and_step() {
        let dir = tempdir().unwrap();
        let (em, _rx) = make(dir.path());
        em.set_position(2, 3);
        let ev = em
            .record(crate::proto::events::assistant_text("x".into()))
            .unwrap();
        assert_eq!(ev.turn, 2);
        assert_eq!(ev.step, 3);
    }
}

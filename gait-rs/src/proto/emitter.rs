use std::path::PathBuf;
use tokio::sync::mpsc::UnboundedSender;
use crate::proto::{wire, gait::log::v1::{Event, event, StreamItem, TextDelta, stream_item}};

pub struct Emitter {
    pub tx_path: PathBuf,
    sender: UnboundedSender<StreamItem>,
}

impl Emitter {
    pub fn new(tx_path: PathBuf, sender: UnboundedSender<StreamItem>) -> Self {
        Self { tx_path, sender }
    }

    /// Durable: write Event to transcript AND send StreamItem{event}. Always both.
    /// `UnboundedSender::send` is sync + non-blocking — safe from anywhere.
    pub fn emit(&self, body: event::Body) -> anyhow::Result<Event> {
        let event = wire::write_to_transcript(&self.tx_path, body)?;
        // receiver dropped (e.g. headless / test) — transcript write already succeeded, so ignore.
        let _ = self.sender.send(StreamItem {
            item: Some(stream_item::Item::Event(event.clone())),
        });
        Ok(event)
    }

    /// Ephemeral: send StreamItem{delta} only. No transcript write.
    pub fn delta(&self, content: String) {
        // channel-only; if the receiver is gone there's nothing to persist, so ignore.
        let _ = self.sender.send(StreamItem {
            item: Some(stream_item::Item::Delta(TextDelta { content })),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;
    use tempfile::tempdir;
    use crate::proto::gait::log::v1::stream_item::Item;

    fn make(dir: &std::path::Path) -> (Emitter, mpsc::UnboundedReceiver<StreamItem>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Emitter::new(dir.join("transcript.jsonl"), tx), rx)
    }

    #[test]
    fn emit_writes_file_and_sends_event() {
        let dir = tempdir().unwrap();
        let (em, mut rx) = make(dir.path());
        let returned = em.emit(crate::proto::events::session_start("s1234567".to_string(), None)).unwrap();
        assert!(em.tx_path.exists());
        match rx.try_recv().unwrap().item {
            Some(Item::Event(ev)) => assert_eq!(ev.id, returned.id),
            other => panic!("expected Event, got {other:?}"),
        }
    }

    #[test]
    fn delta_sends_only_channel_no_file() {
        let dir = tempdir().unwrap();
        let (em, mut rx) = make(dir.path());
        em.delta("hi".to_string());
        assert!(!em.tx_path.exists());
        match rx.try_recv().unwrap().item {
            Some(Item::Delta(d)) => assert_eq!(d.content, "hi"),
            other => panic!("expected Delta, got {other:?}"),
        }
    }
}

use std::path::{Path, PathBuf};
use tokio::sync::mpsc::UnboundedSender;
use crate::proto::{wire, gait::log::v1::{Event, event, StreamItem, TextDelta, stream_item}};

pub struct Recorder {
    pub tx_path: PathBuf,
    sender: UnboundedSender<StreamItem>,
}

impl Recorder {
    pub fn new(tx_path: PathBuf, sender: UnboundedSender<StreamItem>) -> Self {
        Self { tx_path, sender }
    }

    /// Construct from a session layout: writes to `<gait_dir>/sessions/<id>/log.jsonl`.
    /// Mirror of [`Ingestor::from_session`](crate::session::Ingestor::from_session).
    pub fn for_session(gait_dir: &Path, session_id: &str, sender: UnboundedSender<StreamItem>) -> Self {
        Self::new(
            gait_dir.join("sessions").join(session_id).join("log.jsonl"),
            sender,
        )
    }

    /// A no-op recorder for session-less paths (e.g. `stream_text` generation) and
    /// for tests that need a `&Recorder` but don't assert on emission. Records are
    /// dropped: the channel has no receiver and `tx_path` is empty (writes fail
    /// silently). Do NOT use where the transcript/channel matters — use
    /// [`for_session`](Self::for_session).
    pub fn detached() -> Self {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        Self::new(std::path::PathBuf::new(), tx)
    }


    /// Durable: write Event to transcript AND send StreamItem{event}. Always both.
    /// `UnboundedSender::send` is sync + non-blocking — safe from anywhere.
    pub fn record(&self, body: event::Body) -> anyhow::Result<Event> {
        let event = wire::write_to_transcript(&self.tx_path, body)?;
        // receiver dropped (e.g. headless / test) — transcript write already succeeded, so ignore.
        let _ = self.sender.send(StreamItem {
            item: Some(stream_item::Item::Event(event.clone())),
        });
        Ok(event)
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
    use tokio::sync::mpsc;
    use tempfile::tempdir;
    use crate::proto::gait::log::v1::stream_item::Item;

    fn make(dir: &std::path::Path) -> (Recorder, mpsc::UnboundedReceiver<StreamItem>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Recorder::new(dir.join("log.jsonl"), tx), rx)
    }

    #[test]
    fn for_session_builds_transcript_path() {
        let dir = tempdir().unwrap();
        let (tx, _rx) = mpsc::unbounded_channel();
        let em = Recorder::for_session(dir.path(), "s1234567", tx);
        assert_eq!(em.tx_path, dir.path().join("sessions/s1234567/log.jsonl"));
    }

    #[test]
    fn record_writes_file_and_sends_event() {
        let dir = tempdir().unwrap();
        let (em, mut rx) = make(dir.path());
        let returned = em.record(crate::proto::events::session_start("s1234567".to_string(), None)).unwrap();
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

}

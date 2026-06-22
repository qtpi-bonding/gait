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
    /// When `Some`, `record` appends the Event here and skips file+channel.
    /// Holds `Vec<Event>` (not `Vec<event::Body>`) so Phase 2a can serialize and pipe events over the wire.
    buffer: Option<Arc<Mutex<Vec<Event>>>>,
    /// Phase 3 `Sink::Remote`: when `Some`, `record` sends the event here instead
    /// of writing the log file. A brain-side relay task reads from this channel and
    /// sends `Append` signals to the spine over the Nerve. The brain never writes
    /// the gait-dir with this mode active.
    remote: Option<UnboundedSender<Event>>,
    /// Phase 3 Task 7 `Spawn`: when `Some`, `spawn_process` sends (argv, detached) here
    /// instead of directly calling `Command::spawn`. A brain-side relay task reads
    /// from this channel and sends `Spawn` signals to the spine over the Nerve.
    spawn_tx: Option<UnboundedSender<(Vec<String>, bool)>>,
    /// In-memory mirror of every event recorded in remote mode. The brain's
    /// `Ingestor::from_shared` reads this, so the brain never re-reads the log
    /// from disk. `None` outside the brain (buffer/live recorders).
    log_mirror: Option<Arc<Mutex<Vec<Event>>>>,
}

impl Recorder {
    pub fn new(tx_path: PathBuf, sender: UnboundedSender<StreamItem>) -> Self {
        Self {
            tx_path,
            sender,
            position: Arc::new(Mutex::new((0, 0))),
            buffer: None,
            remote: None,
            spawn_tx: None,
            log_mirror: None,
        }
    }

    /// Phase 3 `Sink::Remote`: `record()` sends events to `event_tx` instead of
    /// writing the gait-dir. A brain-side relay task reads from the channel and
    /// forwards each event to the spine as an `Append` signal over the Nerve.
    /// The returned `Recorder` produces no file I/O; the spine is the sole writer.
    pub fn remote_sink(
        stream_sender: UnboundedSender<StreamItem>,
        event_tx: UnboundedSender<Event>,
    ) -> Self {
        Self {
            tx_path: std::path::PathBuf::new(),
            sender: stream_sender,
            position: Arc::new(Mutex::new((0, 0))),
            buffer: None,
            remote: Some(event_tx),
            spawn_tx: None,
            log_mirror: None,
        }
    }

    /// Builder: attach a spawn relay channel so `spawn_process` sends argv over
    /// the Nerve (brain mode) rather than calling `Command::spawn` directly.
    pub fn with_spawn_tx(mut self, tx: UnboundedSender<(Vec<String>, bool)>) -> Self {
        self.spawn_tx = Some(tx);
        self
    }

    /// Builder: attach a shared log mirror that `record()` appends to in remote mode.
    pub fn with_log_mirror(mut self, mirror: Arc<Mutex<Vec<Event>>>) -> Self {
        self.log_mirror = Some(mirror);
        self
    }

    /// The shared log mirror, if this recorder has one (remote/brain mode).
    pub fn log_mirror(&self) -> Option<Arc<Mutex<Vec<Event>>>> {
        self.log_mirror.clone()
    }

    /// Fire-and-forget process launch. In remote (brain) mode, sends argv to
    /// the spine via a Spawn signal. In live mode, calls `Command::spawn` directly.
    /// `argv` = subcommand + args (e.g. `["run", "--session", id, ...]`); the caller
    /// (brain relay or this fallback) prepends `current_exe()` as the program.
    pub fn spawn_process(&self, argv: Vec<String>, detached: bool) {
        if let Some(tx) = &self.spawn_tx {
            let _ = tx.send((argv, detached));
            return;
        }
        let exe = std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("gait"));
        let _ = std::process::Command::new(&exe)
            .args(&argv)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
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

    /// A recorder that buffers full `Event`s in memory instead of writing the
    /// log/channel. The caller drains it with [`take_buffer`](Self::take_buffer).
    /// Holds `Vec<Event>` (not `Vec<event::Body>`) so Phase 2a can serialize and
    /// pipe events over the wire.
    pub fn buffering() -> Self {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        Self {
            tx_path: std::path::PathBuf::new(),
            sender: tx,
            position: Arc::new(Mutex::new((0, 0))),
            buffer: Some(Arc::new(Mutex::new(Vec::new()))),
            remote: None,
            spawn_tx: None,
            log_mirror: None,
        }
    }

    /// Drain the buffered events. Empty for non-buffering recorders.
    pub fn take_buffer(&self) -> Vec<Event> {
        match &self.buffer {
            Some(b) => std::mem::take(&mut *b.lock().expect("buffer mutex poisoned")),
            None => Vec::new(),
        }
    }

    /// Durable: write Event to the log AND send StreamItem{event}. Always both.
    /// `UnboundedSender::send` is sync + non-blocking — safe from anywhere.
    ///
    /// Three modes, checked in priority order:
    /// 1. **Buffer** (`Sink::Buffer`): appends to in-memory Vec, skips file + channel.
    /// 2. **Remote** (`Sink::Remote`): sends Event to the spine-relay channel (no file write).
    ///    A brain-side relay task reads from the channel and sends `Append` Nerve signals.
    /// 3. **Live** (default): writes to `tx_path` and sends `StreamItem{event}` on the channel.
    pub fn record(&self, body: event::Body) -> anyhow::Result<Event> {
        let (turn, step) = *self.position.lock().expect("position mutex poisoned");

        // Mode 1: buffering (hand-side recorder in Phase 2).
        if let Some(buf) = &self.buffer {
            let evt = Event {
                turn,
                step,
                body: Some(body),
                ..Default::default()
            };
            buf.lock().expect("buffer mutex poisoned").push(evt.clone());
            return Ok(evt);
        }

        // Mode 2: remote sink (brain in Phase 3 — DISK-WRITE-FREE).
        if let Some(remote_tx) = &self.remote {
            // Mint a local event so callers can use event.id for Replace targeting.
            // The brain has no log file, so mint with prev=None (seq=1, no parents).
            // The spine's append_with_id() will update seq/parents when persisting.
            let evt = crate::proto::envelope::mint(body, None, turn, step);
            if let Some(mirror) = &self.log_mirror {
                mirror
                    .lock()
                    .expect("log mirror poisoned")
                    .push(evt.clone());
            }
            let _ = remote_tx.send(evt.clone()); // channel receiver is the relay task
            let _ = self.sender.send(StreamItem {
                item: Some(stream_item::Item::Event(evt.clone())),
            });
            return Ok(evt);
        }

        // Mode 3: live file write (default).
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

    #[test]
    fn remote_sink_sends_event_without_writing_file() {
        let (stream_tx, _stream_rx) = mpsc::unbounded_channel::<StreamItem>();
        let (event_tx, mut event_rx) = mpsc::unbounded_channel::<Event>();
        let rec = Recorder::remote_sink(stream_tx, event_tx);

        let ev = rec
            .record(crate::proto::events::assistant_text("hello".into()))
            .unwrap();
        assert!(
            rec.tx_path.as_os_str().is_empty(),
            "remote sink must not have a file path"
        );
        assert!(!ev.id.is_empty(), "record must return an event with a UUID");

        let received = event_rx
            .try_recv()
            .expect("event must reach remote channel");
        assert_eq!(
            received.id, ev.id,
            "channel event must match returned event"
        );
    }

    #[test]
    fn spawn_process_uses_channel_when_spawn_tx_set() {
        let (stream_tx, _) = mpsc::unbounded_channel::<StreamItem>();
        let (event_tx, _) = mpsc::unbounded_channel::<Event>();
        let (spawn_tx, mut spawn_rx) = mpsc::unbounded_channel::<(Vec<String>, bool)>();
        let rec = Recorder::remote_sink(stream_tx, event_tx).with_spawn_tx(spawn_tx);
        rec.spawn_process(
            vec![
                "run".to_string(),
                "--session".to_string(),
                "s12345678".to_string(),
            ],
            false,
        );
        let (argv, detached) = spawn_rx
            .try_recv()
            .expect("spawn argv must arrive in channel");
        assert_eq!(argv[0], "run");
        assert_eq!(argv[1], "--session");
        assert_eq!(argv[2], "s12345678");
        assert!(!detached, "default spawn must not be detached");
    }

    #[test]
    fn buffering_recorder_collects_bodies_and_writes_nothing() {
        let rec = Recorder::buffering();
        let _ = rec.record(crate::proto::events::cwd("/tmp/x")).unwrap();
        let _ = rec.record(crate::proto::events::cwd("/tmp/y")).unwrap();
        assert!(
            rec.tx_path.as_os_str().is_empty(),
            "buffering recorder must not point at a file"
        );
        let events = rec.take_buffer();
        assert_eq!(events.len(), 2);
        assert!(
            events[0].body.is_some(),
            "buffered event must carry its body"
        );
        assert!(rec.take_buffer().is_empty(), "take_buffer drains");
    }

    #[test]
    fn remote_record_appends_to_log_mirror() {
        let (stream_tx, _srx) = mpsc::unbounded_channel();
        let (event_tx, _erx) = mpsc::unbounded_channel();
        let mirror = Arc::new(Mutex::new(Vec::new()));
        let rec = Recorder::remote_sink(stream_tx, event_tx).with_log_mirror(mirror.clone());
        rec.record(crate::proto::events::session_start(
            "s1234567".to_string(),
            None,
            None,
        ))
        .unwrap();
        assert_eq!(
            mirror.lock().unwrap().len(),
            1,
            "recorded event mirrored in memory"
        );
    }
}

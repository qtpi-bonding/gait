use crate::proto::gait::log::v1::Event;
use crate::session::{log, retrace};
use crate::types::Message;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub struct Ingestor {
    inner: IngestorInner,
}

enum IngestorInner {
    File(PathBuf),
    Shared(Arc<Mutex<Vec<Event>>>),
}

impl Ingestor {
    pub fn new(path: PathBuf) -> Self {
        Self {
            inner: IngestorInner::File(path),
        }
    }

    pub fn from_session(gait_dir: &Path, session_id: &str) -> Self {
        Self::new(gait_dir.join("sessions").join(session_id).join("log.jsonl"))
    }

    /// In-memory ingestor backed by a shared log mirror (the brain under a spine).
    /// The `Recorder` appends to the same `Arc` as it records, so reads see live state.
    pub fn from_shared(events: Arc<Mutex<Vec<Event>>>) -> Self {
        Self {
            inner: IngestorInner::Shared(events),
        }
    }

    /// Read all events (from the log file, or a snapshot of the shared mirror).
    pub fn events(&self) -> anyhow::Result<Vec<Event>> {
        match &self.inner {
            IngestorInner::File(p) => Ok(log::read_events(p)?),
            IngestorInner::Shared(m) => Ok(m.lock().expect("log mirror poisoned").clone()),
        }
    }

    /// Reconstruct the LLM message history from the log.
    pub fn retrace(&self) -> anyhow::Result<Vec<Message>> {
        match &self.inner {
            IngestorInner::File(p) => Ok(retrace::messages_from_path(p)?),
            IngestorInner::Shared(_) => Ok(retrace::messages(&self.events()?)?),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn from_session_reads_the_session_log() {
        use crate::proto::gait::log::v1::{payload, AssistantText};
        use crate::proto::wire::{append, body};
        let dir = tempdir().unwrap();
        let log_path = dir.path().join("sessions/s1234567/log.jsonl");
        std::fs::create_dir_all(log_path.parent().unwrap()).unwrap();
        append(
            &log_path,
            body(payload::Kind::AssistantText(AssistantText {
                content: "hi".into(),
            })),
        )
        .unwrap();
        let ingestor = Ingestor::from_session(dir.path(), "s1234567");
        assert_eq!(ingestor.events().unwrap().len(), 1);
    }

    #[test]
    fn from_shared_reads_the_mirror_and_sees_appends() {
        use std::sync::{Arc, Mutex};
        let mirror = Arc::new(Mutex::new(Vec::new()));
        let ingestor = Ingestor::from_shared(mirror.clone());
        assert!(ingestor.events().unwrap().is_empty());
        mirror
            .lock()
            .unwrap()
            .push(crate::proto::gait::log::v1::Event::default());
        assert_eq!(
            ingestor.events().unwrap().len(),
            1,
            "ingestor sees live appends"
        );
    }

    #[test]
    fn events_empty_for_missing_file() {
        let dir = tempdir().unwrap();
        let ingestor = Ingestor::from_session(dir.path(), "s0000000");
        assert!(ingestor.events().unwrap().is_empty());
    }

    #[test]
    fn retrace_empty_for_missing_file() {
        let dir = tempdir().unwrap();
        let ingestor = Ingestor::from_session(dir.path(), "s0000000");
        assert!(ingestor.retrace().unwrap().is_empty());
    }
}

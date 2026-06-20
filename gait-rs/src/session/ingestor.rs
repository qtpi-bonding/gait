use crate::proto::gait::log::v1::Event;
use crate::session::{log, retrace};
use crate::types::Message;
use std::path::{Path, PathBuf};

pub struct Ingestor {
    pub path: PathBuf,
}

impl Ingestor {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn from_session(gait_dir: &Path, session_id: &str) -> Self {
        Self::new(gait_dir.join("sessions").join(session_id).join("log.jsonl"))
    }

    /// Read all events from the log file.
    pub fn events(&self) -> anyhow::Result<Vec<Event>> {
        Ok(log::read_events(&self.path)?)
    }

    /// Reconstruct the LLM message history (Transcript) from the log.
    pub fn retrace(&self) -> anyhow::Result<Vec<Message>> {
        Ok(retrace::messages_from_path(&self.path)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn from_session_builds_correct_path() {
        let dir = tempdir().unwrap();
        let ingestor = Ingestor::from_session(dir.path(), "s1234567");
        assert_eq!(
            ingestor.path,
            dir.path().join("sessions/s1234567/log.jsonl")
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

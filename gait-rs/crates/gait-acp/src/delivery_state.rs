//! Process-level delivery state for one agent: the replay sequence store and
//! the `(author, message_id)` seen-set. It lives under gait-acp's own state
//! directory, never inside the agent identity folder, so a refreshed
//! device-only copy of that folder cannot reset it and a read-only folder
//! cannot refuse it.
//!
//! Layout: `<state_dir>/attestation/<agent_id>/{replay.json,seen.jsonl}`.
//! The seen-set is append-only, one JSON line per key. A crash between the
//! log append and [`DeliveryState::mark_seen`] can redeliver one message
//! once (at-least-once, accepted).
use std::collections::HashSet;
use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use anyhow::{Context, Result};
use message_attestation::FileReplayStore;

const REPLAY_FILE: &str = "replay.json";
const SEEN_FILE: &str = "seen.jsonl";

pub struct DeliveryState {
    replay: std::sync::Arc<FileReplayStore>,
    seen_path: PathBuf,
    seen: Mutex<HashSet<(String, String)>>,
}

impl DeliveryState {
    /// Opens (creating) the state for `agent_id` under `state_dir`.
    pub fn open(state_dir: &Path, agent_id: &str) -> Result<Self> {
        let dir = state_dir.join("attestation").join(agent_id);
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("create delivery state {}", dir.display()))?;
        let replay = FileReplayStore::open(&dir.join(REPLAY_FILE))
            .map_err(|error| anyhow::anyhow!("open attestation replay store: {error}"))?;
        let seen_path = dir.join(SEEN_FILE);
        let seen = match std::fs::read_to_string(&seen_path) {
            Ok(text) => text
                .lines()
                .filter(|line| !line.is_empty())
                .map(|line| {
                    serde_json::from_str::<(String, String)>(line)
                        .with_context(|| format!("corrupt seen-set line in {}", seen_path.display()))
                })
                .collect::<Result<HashSet<_>>>()?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => HashSet::new(),
            Err(error) => {
                return Err(error).with_context(|| format!("read {}", seen_path.display()));
            }
        };
        Ok(Self { replay: std::sync::Arc::new(replay), seen_path, seen: Mutex::new(seen) })
    }

    pub fn replay(&self) -> std::sync::Arc<FileReplayStore> {
        self.replay.clone()
    }

    fn lock(&self) -> MutexGuard<'_, HashSet<(String, String)>> {
        self.seen.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn seen(&self, author: &str, message_id: &str) -> bool {
        self.lock().contains(&(author.to_owned(), message_id.to_owned()))
    }

    /// Records keys as delivered. Called after the batch is in the log.
    pub fn mark_seen(&self, keys: &[(String, String)]) -> Result<()> {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.seen_path)
            .with_context(|| format!("open {}", self.seen_path.display()))?;
        let mut set = self.lock();
        for key in keys {
            if set.insert(key.clone()) {
                writeln!(file, "{}", serde_json::to_string(key)?)
                    .with_context(|| format!("write {}", self.seen_path.display()))?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use message_attestation::ReplayStore;

    #[test]
    fn state_survives_a_rewritten_identity_folder() {
        let state = tempfile::tempdir().unwrap();
        let identity = tempfile::tempdir().unwrap();
        std::fs::write(identity.path().join("device.jwk.json"), b"one").unwrap();
        let first = DeliveryState::open(state.path(), "p256:aa").unwrap();
        assert!(first.replay().record("p256:aa", "p256:dd", 5).unwrap());
        std::fs::remove_dir_all(identity.path()).unwrap();
        std::fs::create_dir_all(identity.path()).unwrap();
        let second = DeliveryState::open(state.path(), "p256:aa").unwrap();
        assert_eq!(second.replay().last_seen("p256:aa", "p256:dd").unwrap(), Some(5));
    }

    #[test]
    fn seen_set_persists_across_reopen() {
        let state = tempfile::tempdir().unwrap();
        let first = DeliveryState::open(state.path(), "p256:aa").unwrap();
        assert!(!first.seen("p256:bb", "m1"));
        first.mark_seen(&[("p256:bb".into(), "m1".into())]).unwrap();
        first.mark_seen(&[("p256:bb".into(), "m1".into())]).unwrap();
        let second = DeliveryState::open(state.path(), "p256:aa").unwrap();
        assert!(second.seen("p256:bb", "m1"));
        assert!(!second.seen("p256:bb", "m2"));
        assert!(!second.seen("p256:cc", "m1"));
        let lines = std::fs::read_to_string(
            state.path().join("attestation/p256:aa").join(SEEN_FILE),
        )
        .unwrap();
        assert_eq!(lines.lines().count(), 1, "a repeated key is written once");
    }

    #[test]
    fn different_agent_ids_do_not_share_state() {
        let state = tempfile::tempdir().unwrap();
        let a = DeliveryState::open(state.path(), "p256:aa").unwrap();
        let b = DeliveryState::open(state.path(), "p256:bb").unwrap();
        a.mark_seen(&[("p256:x".into(), "m".into())]).unwrap();
        a.replay().record("p256:aa", "p256:d", 3).unwrap();
        assert!(!b.seen("p256:x", "m"));
        assert_eq!(b.replay().last_seen("p256:bb", "p256:d").unwrap(), None);
    }

    #[test]
    fn works_when_the_identity_folder_is_read_only() {
        use std::os::unix::fs::PermissionsExt;
        let state = tempfile::tempdir().unwrap();
        let identity = tempfile::tempdir().unwrap();
        std::fs::set_permissions(identity.path(), std::fs::Permissions::from_mode(0o500)).unwrap();
        let s = DeliveryState::open(state.path(), "p256:aa").unwrap();
        s.mark_seen(&[("p256:x".into(), "m".into())]).unwrap();
        assert!(s.replay().record("p256:aa", "p256:d", 1).unwrap());
        std::fs::set_permissions(identity.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    }
}

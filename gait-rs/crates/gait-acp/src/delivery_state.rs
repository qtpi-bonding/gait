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
use identity_keyfile::private_fs::create_private_dir;
use message_attestation::FileReplayStore;

use crate::error::AttestationStateError;

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
        let coded = |error| coded_error::into_anyhow(error);
        create_private_dir(&dir)
            .map_err(|error| coded(AttestationStateError::from_keyfile(&dir, &error)))?;
        let replay_path = dir.join(REPLAY_FILE);
        let replay = FileReplayStore::open(&replay_path)
            .map_err(|error| coded(AttestationStateError::from_store(&replay_path, &error)))?;
        let seen_path = dir.join(SEEN_FILE);
        let seen = match std::fs::read_to_string(&seen_path) {
            Ok(text) => text
                .lines()
                .filter(|line| !line.is_empty())
                .map(|line| {
                    serde_json::from_str::<(String, String)>(line).with_context(|| {
                        format!("corrupt seen-set line in {}", seen_path.display())
                    })
                })
                .collect::<Result<HashSet<_>>>()?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => HashSet::new(),
            Err(error) => {
                return Err(error).with_context(|| format!("read {}", seen_path.display()));
            }
        };
        Ok(Self {
            replay: std::sync::Arc::new(replay),
            seen_path,
            seen: Mutex::new(seen),
        })
    }

    /// The directory holding this agent's delivery state.
    pub fn dir(&self) -> &Path {
        self.seen_path.parent().unwrap_or(Path::new("."))
    }

    /// The replay store's file, the path to name when its failure carries none.
    pub fn replay_path(&self) -> PathBuf {
        self.dir().join(REPLAY_FILE)
    }

    pub fn replay(&self) -> std::sync::Arc<FileReplayStore> {
        self.replay.clone()
    }

    fn lock(&self) -> MutexGuard<'_, HashSet<(String, String)>> {
        self.seen
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn seen(&self, author: &str, message_id: &str) -> bool {
        self.lock()
            .contains(&(author.to_owned(), message_id.to_owned()))
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
        assert!(
            first
                .replay()
                .record("p256:aa", "p256:dd", 5, 0, None)
                .unwrap()
        );
        std::fs::remove_dir_all(identity.path()).unwrap();
        std::fs::create_dir_all(identity.path()).unwrap();
        let second = DeliveryState::open(state.path(), "p256:aa").unwrap();
        assert_eq!(
            second.replay().last_seen("p256:aa", "p256:dd").unwrap(),
            Some(5)
        );
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
        let lines =
            std::fs::read_to_string(state.path().join("attestation/p256:aa").join(SEEN_FILE))
                .unwrap();
        assert_eq!(lines.lines().count(), 1, "a repeated key is written once");
    }

    #[test]
    fn different_agent_ids_do_not_share_state() {
        let state = tempfile::tempdir().unwrap();
        let a = DeliveryState::open(state.path(), "p256:aa").unwrap();
        let b = DeliveryState::open(state.path(), "p256:bb").unwrap();
        a.mark_seen(&[("p256:x".into(), "m".into())]).unwrap();
        a.replay().record("p256:aa", "p256:d", 3, 0, None).unwrap();
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
        assert!(s.replay().record("p256:aa", "p256:d", 1, 0, None).unwrap());
        std::fs::set_permissions(identity.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    fn loose_attestation_dir(state: &Path, agent_id: &str, mode: u32) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let dir = state.join("attestation").join(agent_id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(mode)).unwrap();
        dir
    }

    fn code_of(error: &anyhow::Error) -> &'static str {
        coded_error::facts_anyhow(error).code
    }

    #[test]
    fn a_loose_attestation_dir_fails_closed_with_a_coded_error() {
        let state = tempfile::tempdir().unwrap();
        let dir = loose_attestation_dir(state.path(), "p256:aa", 0o755);
        std::fs::write(dir.join(REPLAY_FILE), br#"{"p256:aa":{"p256:dd":5}}"#).unwrap();
        let error = DeliveryState::open(state.path(), "p256:aa")
            .err()
            .expect("must refuse");
        assert!(
            coded_error::downcast_ref::<crate::error::AttestationStateError>(&error).is_some(),
            "coded, got {error:#}"
        );
        assert_eq!(code_of(&error), "gait.acp.attestation_state.insecure");
        let text = format!("{error:#}");
        assert!(
            text.contains(&dir.display().to_string()),
            "names the path: {text}"
        );
        assert!(text.contains("0755"), "names the octal mode: {text}");
        assert!(
            text.contains("tighten the permissions (chmod 700"),
            "names the fix: {text}"
        );
        let mode = std::fs::metadata(&dir).unwrap();
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(mode.permissions().mode() & 0o777, 0o755, "never chmods");
    }

    #[test]
    fn a_loose_replay_file_fails_closed_with_its_mode() {
        use std::os::unix::fs::PermissionsExt;
        let state = tempfile::tempdir().unwrap();
        let dir = loose_attestation_dir(state.path(), "p256:aa", 0o700);
        let file = dir.join(REPLAY_FILE);
        std::fs::write(&file, b"{}").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        let error = DeliveryState::open(state.path(), "p256:aa")
            .err()
            .expect("must refuse");
        assert_eq!(code_of(&error), "gait.acp.attestation_state.insecure");
        let text = format!("{error:#}");
        assert!(text.contains(&file.display().to_string()), "{text}");
        assert!(
            text.contains("0644") && text.contains("tighten the permissions (chmod 700"),
            "{text}"
        );
    }

    #[test]
    fn an_old_bare_number_replay_file_fails_closed() {
        use std::os::unix::fs::PermissionsExt;
        let state = tempfile::tempdir().unwrap();
        let dir = loose_attestation_dir(state.path(), "p256:aa", 0o700);
        let file = dir.join(REPLAY_FILE);
        std::fs::write(&file, br#"{"p256:aa":{"p256:dd":5}}"#).unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        let error = DeliveryState::open(state.path(), "p256:aa")
            .err()
            .expect("must refuse");
        assert_eq!(code_of(&error), "gait.acp.attestation_state.old_layout");
        let text = format!("{error:#}");
        assert!(
            text.contains(&file.display().to_string())
                && text.contains("old format and must be converted"),
            "{text}"
        );
    }

    #[test]
    fn keyfile_failures_map_to_the_coded_state_error() {
        use crate::error::AttestationStateError as E;
        use identity_keyfile::KeyfileError as K;
        let fallback = Path::new("/state/attestation/p256:aa/replay.json");
        let wrong = E::from_keyfile(fallback, &K::WrongOwner("/state/x".into()));
        assert!(matches!(wrong, E::Insecure { .. }));
        let text = wrong.to_string();
        assert!(
            text.contains("/state/x")
                && text.contains("owned by")
                && text.contains("the path must be owned by the current user"),
            "{text}"
        );
        for key in [
            K::NotAFile("/state/x".into()),
            K::TooLarge {
                path: "/state/x".into(),
                cap: 1,
            },
            K::InsecureFile("/state/x".into()),
        ] {
            assert!(
                matches!(E::from_keyfile(fallback, &key), E::Insecure { .. }),
                "{key}"
            );
        }
        let other = E::from_keyfile(fallback, &K::Missing("/state/y".into()));
        assert!(matches!(other, E::Unavailable { .. }));
        assert!(
            other
                .to_string()
                .contains("/state/attestation/p256:aa/replay.json")
        );
    }
}

//! Durable replay protection for multimatrix DeliveryAttestations. See
//! an internal design note §3a.
//! Keyed only by multimatrix's key_id -- once gait checks membership of
//! its own device pubkey in the attestation's recipient list (gait-core's
//! attestation module), membership alone already establishes "this is for
//! me": a single gait_dir has exactly one identity, so there is no
//! separate recipient dimension for this LOCAL store to track (unlike
//! multimatrix's own counter, which is scoped per-agent on their side).
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub struct AttestationReplayStore {
    file_path: PathBuf,
    state: Mutex<HashMap<String, u64>>, // key: key_id
}

impl AttestationReplayStore {
    /// Fails closed on a corrupted/unparseable state file -- only a
    /// genuinely *absent* file (fresh gait_dir, nothing to lose) starts
    /// from an empty store.
    pub fn new(gait_dir: &Path) -> Result<Self> {
        let file_path = gait_dir.join("attestation_replay.json");
        let state = match std::fs::read(&file_path) {
            Ok(bytes) => serde_json::from_slice(&bytes).with_context(|| {
                format!(
                    "attestation replay state at {} is corrupted -- refusing to silently reset replay protection",
                    file_path.display()
                )
            })?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
            Err(e) => return Err(e).context("reading attestation replay state"),
        };
        Ok(Self {
            file_path,
            state: Mutex::new(state),
        })
    }

    pub fn check_and_record(&self, key_id: &str, batch_sequence: u64) -> Result<bool> {
        let mut state = self.state.lock().unwrap();
        let accept = match state.get(key_id) {
            Some(&last) => batch_sequence > last,
            None => true,
        };
        if accept {
            state.insert(key_id.to_string(), batch_sequence);
            let bytes = serde_json::to_vec(&*state).context("serialize attestation replay state")?;
            // Atomic + crash-durable write: temp file in the same
            // directory (same-filesystem rename is atomic), fsync'd
            // before the rename so the write is actually on disk (not
            // just handed to the OS page cache) before we depend on it,
            // then rename over the real path, then fsync the containing
            // directory too -- on most POSIX filesystems the rename
            // itself isn't guaranteed durable across a crash until the
            // directory entry change is synced, not just the file's
            // contents. A crash before the rename leaves the OLD file
            // intact; a crash after leaves the NEW file intact -- never a
            // partial write in place of either.
            let tmp_path = self.file_path.with_extension("json.tmp");
            {
                let mut f = std::fs::File::create(&tmp_path)
                    .context("creating temp attestation replay state")?;
                use std::io::Write as _;
                f.write_all(&bytes)
                    .context("writing temp attestation replay state")?;
                f.sync_all()
                    .context("fsyncing temp attestation replay state")?;
            }
            std::fs::rename(&tmp_path, &self.file_path)
                .context("renaming attestation replay state into place")?;
            if let Some(parent) = self.file_path.parent() {
                if let Ok(dir) = std::fs::File::open(parent) {
                    let _ = dir.sync_all(); // best-effort; not every platform requires this, but it never hurts
                }
            }
        }
        Ok(accept)
    }
}

// Known, accepted gap (gpt-sol finding, not fixed here): no cross-process
// locking. Two gait processes sharing the same gait_dir and racing on
// check_and_record could both read the same "last" value and both accept
// what should have been a reject. Today's actual usage is one gait
// process per gait_dir at a time for ACP sessions, so this is a real but
// currently-inert gap -- worth a flock (or equivalent) on file_path if
// that assumption ever stops holding, not something this task needs to
// solve speculatively now.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_ever_sequence_for_a_key_is_accepted_unconditionally() {
        let dir = tempfile::tempdir().unwrap();
        let store = AttestationReplayStore::new(dir.path()).unwrap();
        assert!(store.check_and_record("mm-key-1", 5).unwrap());
    }

    #[test]
    fn a_non_increasing_sequence_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let store = AttestationReplayStore::new(dir.path()).unwrap();
        assert!(store.check_and_record("mm-key-1", 5).unwrap());
        assert!(
            !store.check_and_record("mm-key-1", 5).unwrap(),
            "equal must be rejected"
        );
        assert!(
            !store.check_and_record("mm-key-1", 3).unwrap(),
            "lower must be rejected"
        );
    }

    #[test]
    fn a_gap_is_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let store = AttestationReplayStore::new(dir.path()).unwrap();
        assert!(store.check_and_record("mm-key-1", 5).unwrap());
        assert!(
            store.check_and_record("mm-key-1", 100).unwrap(),
            "skipped numbers are fine, only non-increasing is rejected"
        );
    }

    #[test]
    fn distinct_key_ids_are_independent() {
        let dir = tempfile::tempdir().unwrap();
        let store = AttestationReplayStore::new(dir.path()).unwrap();
        assert!(store.check_and_record("mm-key-1", 5).unwrap());
        assert!(
            store.check_and_record("mm-key-2", 1).unwrap(),
            "a different multimatrix key_id (e.g. after rotation) starts fresh"
        );
    }

    #[test]
    fn state_survives_a_new_store_instance_over_the_same_gait_dir() {
        let dir = tempfile::tempdir().unwrap();
        {
            AttestationReplayStore::new(dir.path())
                .unwrap()
                .check_and_record("mm-key-1", 5)
                .unwrap();
        }
        let reopened = AttestationReplayStore::new(dir.path()).unwrap();
        assert!(
            !reopened.check_and_record("mm-key-1", 5).unwrap(),
            "must be durable, not in-memory"
        );
    }

    #[test]
    fn a_missing_file_starts_empty_but_a_corrupted_file_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        // No file at all yet -- legitimate fresh gait_dir, must succeed empty.
        assert!(AttestationReplayStore::new(dir.path()).is_ok());

        // A file that EXISTS but isn't valid state -- must fail loudly, never
        // silently reset to an empty store (that would erase real replay
        // protection without anyone noticing).
        std::fs::write(dir.path().join("attestation_replay.json"), b"not valid json{{{").unwrap();
        assert!(
            AttestationReplayStore::new(dir.path()).is_err(),
            "a corrupted state file must fail closed, not silently reset"
        );
    }
}

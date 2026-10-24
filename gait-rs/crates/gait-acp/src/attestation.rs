//! ACP-edge verification of delivery attestations (design section 4) and
//! conversion into Gait's generic verified incoming-message representation.
//! This protocol-specific code lives at the ACP integration boundary; shared
//! log folding only sees generic verified-author attribution.
//!
//! `VerifiedRoomMessage` carries `body_text`/`room_id`/`message_id`
//! alongside its `Provenance` -- NOT just the provenance. `body_text` lives
//! inside the signed transcript, so a valid signature proves it was not
//! substituted, but only for whoever reads `body_text` FROM the verified list
//! this module returns. The handler must build both the logged event body and
//! the model-facing rendering exclusively from that list.
//!
//! Verification and confirmation are split. [`Attestor::verify`] checks the
//! signature, the recipient list and the sequence against the replay store but
//! records nothing. The returned [`VerifiedBatch`] carries the obligation to
//! confirm: its [`VerifiedBatch::into_parts`] hands back an `OnDurable` closure
//! that the turn source runs once the batch is in the log. Until then the
//! `(key_id, sequence)` pair is claimed in a process-wide in-flight set, so a
//! second session delivering the same batch is rejected like a replay.
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use gait_core::port::input::OnDurable;
use gait_proto::gait::log::v1::Provenance;
use identity_crypto::jwk_duo::{KeyDuoSerializer, ValidationService};
use identity_crypto::{PrincipalId, PublicKey};
use message_attestation::{
    AttestationError, AttestationPolicy, DEFAULT_META_KEY, FileReplayStore, PendingSequence,
    ReplayStore, Verified, confirm, verify_attestation,
};

/// The per-agent replay store, inside the agent directory. One file per agent,
/// shared by every session this process serves.
pub const REPLAY_FILE: &str = "attestation-replay.json";

/// One verified message from a delivery batch: the authoritative body, its
/// room and message id, and the provenance the log records for it. `body_text`
/// here is the ONLY body a caller may use for this message.
pub struct VerifiedRoomMessage {
    pub provenance: Provenance,
    pub body_text: String,
    pub room_id: String,
    pub message_id: String,
}

/// Process-wide attestation state for the agent this process runs as. The
/// own-device set is NOT cached: each [`Attestor::verify`] reloads the agent
/// keys, so a device renewed or rotated after startup is recognised.
pub struct Attestor {
    agent_dir: PathBuf,
    trusted: Vec<(String, PublicKey)>,
    scope: String,
    replay: Arc<FileReplayStore>,
    in_flight: Arc<Mutex<HashSet<(String, u64)>>>,
}

/// A verified batch: its messages, and the confirmation to run once durable.
pub struct VerifiedBatch {
    pub messages: Vec<VerifiedRoomMessage>,
    confirmation: Confirmation,
}

struct Confirmation {
    pending: PendingSequence,
    replay: Arc<FileReplayStore>,
    claim: InFlight,
}

/// A `(key_id, sequence)` claimed by one verified batch. Released on drop, so
/// an abandoned batch (its append failed, its turn never started) frees the
/// sequence for a retry.
struct InFlight {
    set: Arc<Mutex<HashSet<(String, u64)>>>,
    key: (String, u64),
}

impl Drop for InFlight {
    fn drop(&mut self) {
        lock(&self.set).remove(&self.key);
    }
}

fn lock(set: &Mutex<HashSet<(String, u64)>>) -> MutexGuard<'_, HashSet<(String, u64)>> {
    set.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Attestor {
    /// Opens the attestation state for the agent in `agent_dir`, checking at
    /// startup that it has a device to accept attestations for.
    pub fn open(agent_dir: &Path, trusted: Vec<(String, PublicKey)>) -> Result<Self> {
        let public = identity_keyfile::DeviceKeys::load(agent_dir)
            .with_context(|| format!("load agent identity from {}", agent_dir.display()))?
            .public();
        let own_devices = public.device_keys_at(now_unix());
        Self::with_own_devices(agent_dir, trusted, own_devices, public.agent_id.to_string())
    }

    /// The startup constructor behind [`Attestor::open`], taking the initial
    /// own-device list and replay scope. An empty list is a startup error: with
    /// no device to recognise, no attestation could ever name this agent.
    pub fn with_own_devices(
        agent_dir: &Path,
        trusted: Vec<(String, PublicKey)>,
        own_devices: Vec<PublicKey>,
        scope: String,
    ) -> Result<Self> {
        ensure_own_devices(agent_dir, &own_devices)?;
        let replay = FileReplayStore::open(&agent_dir.join(REPLAY_FILE))
            .map_err(|error| anyhow!("open attestation replay store: {error}"))?;
        Ok(Self {
            agent_dir: agent_dir.to_owned(),
            trusted,
            scope,
            replay: Arc::new(replay),
            in_flight: Arc::new(Mutex::new(HashSet::new())),
        })
    }

    /// Verifies the attestation in a prompt's `_meta` object. `Ok(None)` when
    /// there is none. Any rejection is an error, and records nothing. The own
    /// devices are read from the agent directory now, as of the system clock.
    pub fn verify(
        &self,
        meta: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<Option<VerifiedBatch>> {
        self.verify_with_gap(meta, || {})
    }

    /// [`Attestor::verify`] with a hook that runs between reading the replay
    /// store and claiming the sequence, so a test can interleave another
    /// session's confirmation exactly there.
    fn verify_with_gap(
        &self,
        meta: &serde_json::Map<String, serde_json::Value>,
        between_read_and_claim: impl FnOnce(),
    ) -> Result<Option<VerifiedBatch>> {
        let own_devices = identity_keyfile::DeviceKeys::load(&self.agent_dir)
            .with_context(|| format!("load agent identity from {}", self.agent_dir.display()))?
            .public()
            .device_keys_at(now_unix());
        ensure_own_devices(&self.agent_dir, &own_devices)?;
        let policy = AttestationPolicy {
            meta_key: DEFAULT_META_KEY,
            trusted: &self.trusted,
            own_devices: Some(own_devices.as_slice()),
            max_age_secs: None,
            now_unix: None,
        };
        let meta = serde_json::Value::Object(meta.clone());
        let Some(Verified { delivery, pending }) =
            verify_attestation(&meta, &policy, self.replay.as_ref(), &self.scope)?
        else {
            return Ok(None);
        };
        between_read_and_claim();
        let claim = self.claim(&delivery.key_id, delivery.batch_sequence)?;
        // Another session may have confirmed this sequence (and dropped its claim)
        // after the replay read above and before our claim. Re-read under the
        // claim: from here on no other session can confirm it concurrently.
        if let Some(last) = self.replay.last_seen(&self.scope, &delivery.key_id)? {
            if delivery.batch_sequence <= last {
                bail!(
                    "attested batch {}#{} was already confirmed (last {last}); replay rejected",
                    delivery.key_id,
                    delivery.batch_sequence
                );
            }
        }
        let messages = delivery
            .messages
            .into_iter()
            .map(|body| VerifiedRoomMessage {
                provenance: Provenance {
                    verified_author_id: Some(
                        PrincipalId::from_genesis(body.author_genesis).to_string(),
                    ),
                    context: body.room_id.clone(),
                    ..Default::default()
                },
                body_text: body.body_text,
                room_id: body.room_id,
                message_id: body.message_id,
            })
            .collect();
        Ok(Some(VerifiedBatch {
            messages,
            confirmation: Confirmation {
                pending,
                replay: self.replay.clone(),
                claim,
            },
        }))
    }

    fn claim(&self, key_id: &str, sequence: u64) -> Result<InFlight> {
        let key = (key_id.to_owned(), sequence);
        if !lock(&self.in_flight).insert(key.clone()) {
            bail!("attested batch {key_id}#{sequence} is already in flight (possible replay)");
        }
        Ok(InFlight {
            set: self.in_flight.clone(),
            key,
        })
    }
}

fn ensure_own_devices(agent_dir: &Path, own_devices: &[PublicKey]) -> Result<()> {
    if own_devices.is_empty() {
        bail!(
            "the agent at {} has no device signing key to accept attestations for",
            agent_dir.display()
        );
    }
    Ok(())
}

impl VerifiedBatch {
    /// Splits the batch into its messages and the closure that confirms it.
    /// The turn source runs the closure right after the batch's events are
    /// appended. If the append fails the closure is dropped unrun, and dropping
    /// it releases the in-flight claim without advancing the replay store.
    pub fn into_parts(self) -> (Vec<VerifiedRoomMessage>, OnDurable) {
        let Confirmation {
            pending,
            replay,
            claim,
        } = self.confirmation;
        let on_durable: OnDurable = Box::new(move || {
            let _claim = claim;
            match confirm(replay.as_ref(), pending) {
                Ok(()) => Ok(()),
                Err(AttestationError::Replay { key_id, last, got }) => {
                    eprintln!(
                        "gait-acp: attested batch {key_id}#{got} was already confirmed (last {last}); its message is already logged, continuing"
                    );
                    Ok(())
                }
                Err(error) => Err(anyhow!("confirm attested batch: {error}")),
            }
        });
        (self.messages, on_durable)
    }
}

/// Verifies the attestation (if any) in a prompt's `_meta`. Without an agent
/// identity there is no own-device list to verify against, so an attestation
/// that is present is an error.
pub fn verify_prompt(
    attestor: Option<&Attestor>,
    meta: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Result<Option<VerifiedBatch>> {
    let Some(meta) = meta else {
        return Ok(None);
    };
    if !meta.contains_key(DEFAULT_META_KEY) {
        return Ok(None);
    }
    let attestor = attestor.ok_or_else(|| {
        anyhow!("attestation present but this process has no agent identity to verify it against")
    })?;
    attestor.verify(meta)
}

/// Reads a `--trusted-multimatrix-key` file: the PUBLIC key-duo JWK of a
/// delivery key, as `export_public_key_duo_jwk` writes it. Returns the key id
/// attestations carry (the RFC 7638 thumbprint of the signing half) and the
/// signing key. A private JWK is refused rather than having its private
/// members silently dropped.
pub fn load_trusted_key(path: &Path) -> Result<(String, PublicKey)> {
    let json = std::fs::read_to_string(path)
        .with_context(|| format!("read trusted multimatrix key {}", path.display()))?;
    let value: serde_json::Value = serde_json::from_str(&json)
        .with_context(|| format!("{} is not JSON", path.display()))?;
    ValidationService::validate_no_private_key_material(&value).map_err(|error| {
        anyhow!(
            "{} holds private key material; --trusted-multimatrix-key takes the public key-duo JWK: {error}",
            path.display()
        )
    })?;
    let duo = KeyDuoSerializer::import_public_key_duo_jwk(&json)
        .map_err(|error| anyhow!("{} is not a key-duo JWK: {error}", path.display()))?;
    let key = PublicKey::from(duo.signing_key_pair());
    Ok((key.thumbprint(), key))
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use identity_crypto::jwk_duo::{GenerationService, KeyDuo};
    use identity_crypto::pb;
    use identity_keyfile::testkit::TempAgentDir;
    use message_attestation::{UnsignedAttestation, attach, sign_attestation};

    fn public_jwk_file(dir: &Path, duo: &KeyDuo) -> std::path::PathBuf {
        let path = dir.join("trusted.jwk.json");
        let json = KeyDuoSerializer::export_public_key_duo_jwk(duo).unwrap();
        std::fs::write(&path, json).unwrap();
        path
    }

    #[test]
    fn empty_own_devices_is_a_startup_error() {
        let dir = tempfile::tempdir().unwrap();
        let result = Attestor::with_own_devices(
            dir.path(),
            Vec::new(),
            Vec::new(),
            "p256:scope".to_owned(),
        );
        let error = result.err().expect("no own device must refuse to start");
        assert!(
            error.to_string().contains("no device signing key"),
            "the error names the missing device set: {error}"
        );
        assert!(
            !dir.path().join(REPLAY_FILE).exists(),
            "the replay store is not created when startup is refused"
        );
    }

    #[test]
    fn private_jwk_is_rejected_as_trusted_key() {
        let dir = tempfile::tempdir().unwrap();
        let duo = GenerationService::generate_key_duo();

        let private_path = dir.path().join("private.jwk.json");
        std::fs::write(
            &private_path,
            KeyDuoSerializer::export_key_duo_jwk(&duo).unwrap(),
        )
        .unwrap();
        let error = load_trusted_key(&private_path)
            .err()
            .expect("a private JWK must not be accepted as a trusted key");
        assert!(
            error.to_string().contains("private key material"),
            "the refusal says why: {error}"
        );

        let public_path = public_jwk_file(dir.path(), &duo);
        assert!(
            load_trusted_key(&public_path).is_ok(),
            "the same key's public half is accepted"
        );
    }

    #[test]
    fn trusted_key_thumbprint_matches_attestation_key_id() {
        let dir = tempfile::tempdir().unwrap();
        let central = GenerationService::generate_key_duo();
        let path = public_jwk_file(dir.path(), &central);
        let (key_id, key) = load_trusted_key(&path).unwrap();

        assert_eq!(key_id, key.thumbprint(), "key id is the signing half's thumbprint");
        assert_eq!(key, PublicKey::from(central.signing_key_pair()));

        let author = pb::PublicKey::from(&key);
        let session = pb::PublicKey::from(&PublicKey::from(
            GenerationService::generate_key_duo().signing_key_pair(),
        ));
        let attestation = sign_attestation(
            central.signing_key_pair(),
            UnsignedAttestation {
                batch_sequence: 1,
                attested_at: identity_crypto::Timestamp {
                    seconds: 100,
                    nanos: 0,
                },
                messages: vec![pb::AttestedMessage {
                    message_id: "m1".to_owned(),
                    room_id: "room-general".to_owned(),
                    body_text: "hello".to_owned(),
                    author_genesis: Some(author.clone()),
                    delegation_cert: Some(pb::DelegationCert {
                        not_before_unix_seconds: 1_700_000_000,
                        not_after_unix_seconds: 1_800_000_000,
                        signature: vec![7; 64],
                        principal_genesis: Some(author.clone()),
                        mode: 0,
                        device: Some(author.clone()),
                        session: Some(session),
                        host_genesis: None,
                        subject: None,
                    }),
                }],
                recipient_devices: vec![author],
            },
        )
        .unwrap();
        assert_eq!(
            attestation.key_id, key_id,
            "an attestation signed by this key names the key id the trusted table is keyed by"
        );
    }

    /// A batch of one message, signed by `central` and addressed to `recipient`.
    fn batch_meta(
        central: &KeyDuo,
        recipient: &PublicKey,
        sequence: u64,
    ) -> serde_json::Map<String, serde_json::Value> {
        let author = pb::PublicKey::from(&PublicKey::from(
            GenerationService::generate_key_duo().signing_key_pair(),
        ));
        let session = pb::PublicKey::from(&PublicKey::from(
            GenerationService::generate_key_duo().signing_key_pair(),
        ));
        let attestation = sign_attestation(
            central.signing_key_pair(),
            UnsignedAttestation {
                batch_sequence: sequence,
                attested_at: identity_crypto::Timestamp {
                    seconds: 100,
                    nanos: 0,
                },
                messages: vec![pb::AttestedMessage {
                    message_id: "m1".to_owned(),
                    room_id: "room-general".to_owned(),
                    body_text: "hello".to_owned(),
                    author_genesis: Some(author.clone()),
                    delegation_cert: Some(pb::DelegationCert {
                        not_before_unix_seconds: 1_700_000_000,
                        not_after_unix_seconds: 1_800_000_000,
                        signature: vec![7; 64],
                        principal_genesis: Some(author.clone()),
                        mode: 0,
                        device: Some(author),
                        session: Some(session),
                        host_genesis: None,
                        subject: None,
                    }),
                }],
                recipient_devices: vec![pb::PublicKey::from(recipient)],
            },
        )
        .unwrap();
        let mut meta = serde_json::Map::new();
        attach(&mut meta, DEFAULT_META_KEY, &attestation).unwrap();
        meta
    }

    fn trusted_central() -> (KeyDuo, (String, PublicKey)) {
        let central = GenerationService::generate_key_duo();
        let key = PublicKey::from(central.signing_key_pair());
        (central, (key.thumbprint(), key))
    }

    #[test]
    fn device_rotated_after_startup_is_accepted() {
        let now = now_unix();
        // The agent was created, then its device rotated a while ago: the old
        // device retired before `now`, the new one is current.
        let agent = TempAgentDir::new(now - 1_000, 86_400);
        let (central, trusted) = trusted_central();
        let attestor = Attestor::open(agent.path(), vec![trusted]).unwrap();
        let old_device =
            identity_keyfile::device_keys_at(&agent.load().unwrap(), now)[0].clone();

        identity_keyfile::AgentKeys::rotate_device(agent.path(), now - 500, 86_400).unwrap();
        let new_device =
            identity_keyfile::device_keys_at(&agent.load().unwrap(), now)[0].clone();
        assert_ne!(old_device, new_device, "the rotation produced a new device");

        assert!(
            attestor
                .verify(&batch_meta(&central, &new_device, 1))
                .unwrap()
                .is_some(),
            "a batch addressed to the device rotated in after startup is accepted"
        );
        assert!(
            attestor
                .verify(&batch_meta(&central, &old_device, 2))
                .is_err(),
            "a batch addressed to a device retired before now is rejected"
        );
    }

    #[test]
    fn a_batch_confirmed_between_the_replay_read_and_the_claim_is_rejected() {
        let now = now_unix();
        let agent = TempAgentDir::new(now, 86_400);
        let (central, trusted) = trusted_central();
        let attestor = Attestor::open(agent.path(), vec![trusted]).unwrap();
        let device = identity_keyfile::device_keys_at(&agent.load().unwrap(), now)[0].clone();
        let meta = batch_meta(&central, &device, 1);

        // Session A claims the batch and will confirm it.
        let (_messages, a_confirm) = attestor
            .verify(&meta)
            .unwrap()
            .expect("the batch verifies")
            .into_parts();
        let mut a_confirm = Some(a_confirm);

        // Session B has read the replay store (nothing confirmed yet) when A
        // confirms and releases its claim; only then does B try to claim.
        let error = attestor
            .verify_with_gap(&meta, || {
                (a_confirm.take().unwrap())().unwrap();
            })
            .err()
            .expect("a batch confirmed after the replay read must not be accepted again");
        assert!(
            error.to_string().contains("already confirmed"),
            "the rejection names the replay: {error}"
        );
        assert!(
            attestor.verify(&meta).is_err(),
            "the claim B took is released, and the sequence stays burned"
        );
    }

    #[test]
    fn in_flight_claim_rejects_a_concurrent_second_claimant() {
        let now = now_unix();
        let agent = TempAgentDir::new(now, 86_400);
        let (central, trusted) = trusted_central();
        let attestor = Attestor::open(agent.path(), vec![trusted]).unwrap();
        let device = identity_keyfile::device_keys_at(&agent.load().unwrap(), now)[0].clone();
        let meta = batch_meta(&central, &device, 1);

        let first = attestor
            .verify(&meta)
            .unwrap()
            .expect("the batch verifies");
        // The replay store still holds nothing, so only the in-flight claim of
        // the first, unconfirmed batch can reject this second claimant.
        let error = attestor
            .verify(&meta)
            .err()
            .expect("a second claimant of an in-flight batch is rejected");
        assert!(
            error.to_string().contains("in flight"),
            "the rejection names the in-flight claim: {error}"
        );

        // Dropping the first batch's confirmation releases the claim unconfirmed.
        let (_messages, on_durable) = first.into_parts();
        drop(on_durable);
        assert!(
            attestor.verify(&meta).unwrap().is_some(),
            "once released, the same batch claims and verifies again"
        );
    }
}

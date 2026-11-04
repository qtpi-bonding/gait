//! ACP-edge verification of `Delivery`s of author-signed `Message`s (messages
//! and ids design) and conversion into Gait's generic verified
//! incoming-message representation. This protocol-specific code lives at the
//! ACP integration boundary; shared log folding only sees the structured
//! fields it logs.
//!
//! [`VerifiedIncoming`] carries the whole verified `Message` (id, targets,
//! thread, mentions, body) and the `via` targets alongside its author. The
//! body lives inside the author's signed transcript, so a valid signature
//! proves it was not substituted, but only for whoever reads it FROM the
//! verified list this module returns. The handler must build both the logged
//! event body and the model-facing rendering exclusively from that list.
//!
//! Verification and confirmation are split. [`Attestor::verify`] checks the
//! signature, the deliverer's trust, the recipient and the sequence against
//! the replay store but records nothing. The returned [`VerifiedBatch`]
//! carries the obligation to confirm: its [`VerifiedBatch::into_parts`] hands
//! back an `OnDurable` closure that the turn source runs once the batch is in
//! the log. Until then the `(deliverer, sequence)` pair is claimed in a
//! process-wide in-flight set, so a second session delivering the same batch
//! is rejected like a replay. A Message whose `(author, message_id)` was
//! already delivered is dropped from the batch even inside a fresh Delivery.
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use gait_core::port::input::OnDurable;
use identity_crypto::{PrincipalId, PublicKey, pb};
use message_attestation::{
    AttestationError, DEFAULT_META_KEY, DeliveryPolicy, PendingSequence, ReplayStore, confirm,
    verify_delivery_meta,
};

use crate::delivery_state::DeliveryState;
use crate::error::PromptError;

/// One verified message from a delivery: the authoritative `Message`, the
/// targets that caused this recipient to get it, and its verified author.
/// The message body is the ONLY body a caller may use for this message.
pub struct VerifiedIncoming {
    pub author_id: String,
    pub message: pb::Message,
    pub via: Vec<pb::Target>,
}

/// Process-wide attestation state for the agent this process runs as. The
/// own-device set is NOT cached: each [`Attestor::verify`] reloads the agent
/// keys, so a device renewed or rotated after startup is recognised.
pub struct Attestor {
    agent_dir: PathBuf,
    self_id: PrincipalId,
    trusted: Vec<PrincipalId>,
    state: Arc<DeliveryState>,
    in_flight: Arc<Mutex<HashSet<(String, u64)>>>,
}

/// A verified batch: its messages, and the confirmation to run once durable.
pub struct VerifiedBatch {
    pub messages: Vec<VerifiedIncoming>,
    confirmation: Confirmation,
}

struct Confirmation {
    pending: PendingSequence,
    state: Arc<DeliveryState>,
    seen: Vec<(String, String)>,
    claim: InFlight,
}

/// A `(deliverer, sequence)` claimed by one verified batch. Released on drop,
/// so an abandoned batch (its append failed, its turn never started) frees the
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
    /// Opens the attestation state for the agent in `agent_dir`, keeping its
    /// replay and dedupe state under `state_dir`, and checking at startup that
    /// the agent has a device to accept deliveries for.
    pub fn open(agent_dir: &Path, state_dir: &Path, trusted: Vec<PrincipalId>) -> Result<Self> {
        let public = identity_keyfile::DeviceKeys::load(agent_dir)
            .with_context(|| format!("load agent identity from {}", agent_dir.display()))?
            .public();
        ensure_own_devices(agent_dir, &public.device_keys_at(now_unix()))?;
        let self_id = public.principal_id.clone();
        let state = DeliveryState::open(state_dir, &self_id.to_string())?;
        Ok(Self {
            agent_dir: agent_dir.to_owned(),
            self_id,
            trusted,
            state: Arc::new(state),
            in_flight: Arc::new(Mutex::new(HashSet::new())),
        })
    }

    /// The id this process runs as; also the replay scope.
    pub fn self_id(&self) -> &PrincipalId {
        &self.self_id
    }

    /// Verifies the delivery in a prompt's `_meta` object. `Ok(None)` when
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
        let now = now_unix();
        let own_devices = identity_keyfile::DeviceKeys::load(&self.agent_dir)
            .with_context(|| format!("load agent identity from {}", self.agent_dir.display()))?
            .public()
            .device_keys_at(now);
        ensure_own_devices(&self.agent_dir, &own_devices)?;
        let policy = DeliveryPolicy {
            trusted: &self.trusted,
            self_id: &self.self_id,
            own_devices: &own_devices,
            max_age_secs: None,
            now_unix: now,
        };
        let replay = self.state.replay();
        let meta = serde_json::Value::Object(meta.clone());
        let Some(verified) =
            verify_delivery_meta(&meta, DEFAULT_META_KEY, &policy, replay.as_ref())?
        else {
            return Ok(None);
        };
        let (delivery, pending) = (verified.delivery, verified.pending);
        between_read_and_claim();
        let deliverer = delivery.deliverer().to_string();
        let sequence = delivery.sequence();
        let claim = self.claim(&deliverer, sequence)?;
        // Another session may have confirmed this sequence (and dropped its claim)
        // after the replay read above and before our claim. Re-read under the
        // claim: from here on no other session can confirm it concurrently.
        if let Some(last) = replay
            .last_seen(&self.self_id.to_string(), &deliverer)
            .map_err(|error| anyhow!("read attestation replay store: {error}"))?
        {
            if sequence <= last {
                bail!(
                    "attested batch {deliverer}#{sequence} was already confirmed (last {last}); replay rejected"
                );
            }
        }
        let mut seen = Vec::new();
        let mut messages = Vec::new();
        for message in delivery.messages() {
            let key = message.key();
            let key = (key.author.to_string(), key.message_id);
            seen.push(key.clone());
            if self.state.seen(&key.0, &key.1) {
                continue;
            }
            messages.push(VerifiedIncoming {
                author_id: message.author().to_string(),
                message: message.message().clone(),
                via: message.via().to_vec(),
            });
        }
        Ok(Some(VerifiedBatch {
            messages,
            confirmation: Confirmation { pending, state: self.state.clone(), seen, claim },
        }))
    }

    fn claim(&self, deliverer: &str, sequence: u64) -> Result<InFlight> {
        let key = (deliverer.to_owned(), sequence);
        if !lock(&self.in_flight).insert(key.clone()) {
            bail!("attested batch {deliverer}#{sequence} is already in flight (possible replay)");
        }
        Ok(InFlight { set: self.in_flight.clone(), key })
    }
}

fn ensure_own_devices(agent_dir: &Path, own_devices: &[PublicKey]) -> Result<()> {
    if own_devices.is_empty() {
        bail!(
            "the agent at {} has no device signing key to accept deliveries for",
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
    /// `messages` is empty when every message was already delivered; the
    /// caller still runs the closure so the sequence advances.
    pub fn into_parts(self) -> (Vec<VerifiedIncoming>, OnDurable) {
        let Confirmation { pending, state, seen, claim } = self.confirmation;
        let on_durable: OnDurable = Box::new(move || {
            let _claim = claim;
            match confirm(state.replay().as_ref(), pending) {
                Ok(()) => {}
                Err(AttestationError::Replay { principal, last, got }) => {
                    eprintln!(
                        "gait-acp: attested batch {principal}#{got} was already confirmed (last {last}); its message is already logged, continuing"
                    );
                }
                Err(error) => return Err(anyhow!("confirm attested batch: {error}")),
            }
            state.mark_seen(&seen)
        });
        (self.messages, on_durable)
    }
}

/// Verifies the delivery (if any) in a prompt's `_meta`. Without an agent
/// identity there is no own-device list to verify against, so a delivery
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
        coded_error::into_anyhow(PromptError::AttestationMissingOwnDevices)
    })?;
    attestor.verify(meta).map_err(|error| {
        let typed = match coded_error::downcast_ref::<AttestationError>(&error) {
            Some(AttestationError::UnknownDeliverer(who)) => {
                PromptError::AttestationUnknownDeliverer(who.clone())
            }
            Some(
                AttestationError::BadSignature
                | AttestationError::DelivererDevice(_)
                | AttestationError::MessageSignature { .. }
                | AttestationError::WrongBodyType { .. },
            ) => PromptError::AttestationBadSignature,
            Some(AttestationError::NotARecipient) => PromptError::AttestationNotRecipient,
            Some(AttestationError::WrongRecipient) => PromptError::AttestationRecipientIdMismatch,
            Some(AttestationError::WrongAudience { index }) => {
                PromptError::AttestationAudienceMismatch { index: *index }
            }
            Some(
                AttestationError::InvalidMessage { .. }
                | AttestationError::InvalidDeliveryId
                | AttestationError::InvalidVia { .. },
            ) => PromptError::AttestationInvalidMessage(
                coded_error::downcast_ref::<AttestationError>(&error)
                    .map(|e| e.to_string())
                    .unwrap_or_default(),
            ),
            Some(AttestationError::Stale { age, max }) => {
                PromptError::AttestationStale(format!("age {age}s, max {max}s"))
            }
            Some(AttestationError::Replay { principal, last, got }) => {
                PromptError::AttestationReplay(format!("replay for {principal}: last {last}, got {got}"))
            }
            Some(AttestationError::EmptyMessages | AttestationError::NoRecipients) => {
                PromptError::AttestationEmpty
            }
            Some(AttestationError::OwnDevicesMissing) => PromptError::AttestationMissingOwnDevices,
            _ => return error,
        };
        coded_error::into_anyhow(typed)
    })
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}

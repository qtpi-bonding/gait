//! A session key is minted fresh, purely in memory, per session -- never
//! written to gait_dir, never registered with grorg. It's delegated by
//! whichever device key is currently active (the existing per-gait_dir
//! keyring), and the delegation certificate travels with every event this
//! session key signs. The transcript is built by `identity-crypto`, the one
//! implementation grorg/gait/multimatrix all share -- never reimplemented
//! here. See an internal design note §3-4.
//!
//! Gait never holds, derives, or reasons about an `agent_id` (revised
//! 2026-09-02 -- see the plan's Architecture section). A cert minted here
//! is a pure, agent-agnostic statement: "this device key vouches for this
//! session key, for this window." The device-key-to-agent link, if one
//! ever exists, is entirely grorg-side data (an `AgentKey` row); a
//! verifier who cares which agent a device key belongs to already knows
//! which agent they're checking before they look at this cert. So
//! `DelegationCert.agent_id` is always sent empty here -- honest, not a
//! shortcut, since gait genuinely has nothing to put there.

use crate::{Ed25519Signer, Signer};
use identity_crypto::DelegationCert;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub struct SessionKey {
    signer: Ed25519Signer,
    cert: DelegationCert,
}

impl SessionKey {
    /// Mints a fresh session keypair and signs a delegation certificate for
    /// it using `device`, via `identity_crypto::transcripts::session_delegation_transcript`
    /// -- the canonical encoding shared with grorg/multimatrix (see Global
    /// Constraints). Rejects a zero or overflowing `valid_for` outright
    /// rather than emitting a structurally invalid (`not_before == not_after`,
    /// or wrapped) window a verifier would otherwise have to catch.
    pub fn mint(device: &dyn Signer, valid_for: Duration) -> anyhow::Result<Self> {
        anyhow::ensure!(
            !valid_for.is_zero(),
            "session key validity window must be nonzero"
        );
        let signer = Ed25519Signer::generate();
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;
        let not_after = now
            .checked_add(i64::try_from(valid_for.as_secs())?)
            .ok_or_else(|| anyhow::anyhow!("valid_for overflows a unix-seconds timestamp"))?;

        let device_public_key: [u8; 32] = device
            .public_key()
            .try_into()
            .map_err(|_| anyhow::anyhow!("device public key must be 32 bytes"))?;
        let session_public_key: [u8; 32] = signer
            .public_key()
            .try_into()
            .map_err(|_| anyhow::anyhow!("session public key must be 32 bytes"))?;

        // agent_id is always empty -- see the module doc comment.
        let transcript = identity_crypto::transcripts::session_delegation_transcript(
            "",
            &device_public_key,
            &session_public_key,
            now,
            not_after,
        );
        let signature = device.sign(&transcript);
        let cert = DelegationCert {
            agent_id: String::new(),
            device_public_key: device_public_key.to_vec(),
            session_public_key: session_public_key.to_vec(),
            not_before_unix_seconds: now,
            not_after_unix_seconds: not_after,
            signature,
        };
        Ok(Self { signer, cert })
    }

    pub fn signer(&self) -> &Ed25519Signer {
        &self.signer
    }

    pub fn delegation_cert(&self) -> &DelegationCert {
        &self.cert
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_valid_for_is_rejected() {
        let device = Ed25519Signer::generate();
        assert!(SessionKey::mint(&device, Duration::from_secs(0)).is_err());
    }

    #[test]
    fn minted_session_key_has_a_delegation_cert_signed_by_the_device() {
        let device = Ed25519Signer::generate();
        let session = SessionKey::mint(&device, Duration::from_secs(3600)).unwrap();
        assert_eq!(session.delegation_cert().device_public_key, device.public_key());
        assert_eq!(
            session.delegation_cert().session_public_key,
            session.signer().public_key()
        );
        assert!(
            session.delegation_cert().not_after_unix_seconds
                > session.delegation_cert().not_before_unix_seconds
        );
    }

    #[test]
    fn delegation_cert_agent_id_is_always_empty() {
        // Gait never holds or derives an agent_id -- see the module doc comment.
        let device = Ed25519Signer::generate();
        let session = SessionKey::mint(&device, Duration::from_secs(60)).unwrap();
        assert_eq!(session.delegation_cert().agent_id, "");
    }

    #[test]
    fn two_mints_produce_different_session_keys() {
        let device = Ed25519Signer::generate();
        let a = SessionKey::mint(&device, Duration::from_secs(60)).unwrap();
        let b = SessionKey::mint(&device, Duration::from_secs(60)).unwrap();
        assert_ne!(a.signer().public_key(), b.signer().public_key());
    }
}

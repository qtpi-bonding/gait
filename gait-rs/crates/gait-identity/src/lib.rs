//! Agent identity: pluggable signing schemes for `Provenance.from`/`signature`.

pub mod keyring;
pub use keyring::{Keyring, KeyringEntry};
pub mod session_key;
pub use session_key::SessionKey;
pub mod verify_event;
pub use verify_event::verify_provenance;

pub use identity_crypto::KeyScheme;

/// Anything that can sign on behalf of one agent identity.
pub trait Signer {
    fn public_key(&self) -> &[u8];
    fn scheme(&self) -> KeyScheme;
    fn sign(&self, msg: &[u8]) -> Vec<u8>;
}

/// The only real scheme implemented today.
pub struct Ed25519Signer {
    signing_key: ed25519_dalek::SigningKey,
    public_key_bytes: [u8; 32],
}

impl Ed25519Signer {
    pub fn generate() -> Self {
        let signing_key = ed25519_dalek::SigningKey::generate(&mut rand::rngs::OsRng);
        let public_key_bytes = signing_key.verifying_key().to_bytes();
        Self {
            signing_key,
            public_key_bytes,
        }
    }

    pub fn from_bytes(secret: &[u8]) -> anyhow::Result<Self> {
        let arr: [u8; 32] = secret
            .try_into()
            .map_err(|_| anyhow::anyhow!("ed25519 secret key must be exactly 32 bytes"))?;
        let signing_key = ed25519_dalek::SigningKey::from_bytes(&arr);
        let public_key_bytes = signing_key.verifying_key().to_bytes();
        Ok(Self {
            signing_key,
            public_key_bytes,
        })
    }

    pub fn secret_bytes(&self) -> [u8; 32] {
        self.signing_key.to_bytes()
    }
}

impl Signer for Ed25519Signer {
    fn public_key(&self) -> &[u8] {
        &self.public_key_bytes
    }

    fn scheme(&self) -> KeyScheme {
        KeyScheme::Ed25519
    }

    fn sign(&self, msg: &[u8]) -> Vec<u8> {
        use ed25519_dalek::Signer as _;
        self.signing_key.sign(msg).to_bytes().to_vec()
    }
}

/// Dispatch verification by scheme. `Secp256k1Schnorr` has no implementation
/// yet — returns `Err`, never panics, never silently accepts.
pub fn verify(scheme: KeyScheme, pubkey: &[u8], msg: &[u8], sig: &[u8]) -> anyhow::Result<bool> {
    match scheme {
        KeyScheme::Ed25519 => {
            use ed25519_dalek::Verifier as _;
            let key_bytes: [u8; 32] = pubkey
                .try_into()
                .map_err(|_| anyhow::anyhow!("ed25519 public key must be 32 bytes"))?;
            let verifying_key = ed25519_dalek::VerifyingKey::from_bytes(&key_bytes)
                .map_err(|e| anyhow::anyhow!("invalid ed25519 public key: {e}"))?;
            let sig_bytes: [u8; 64] = sig
                .try_into()
                .map_err(|_| anyhow::anyhow!("ed25519 signature must be 64 bytes"))?;
            let signature = ed25519_dalek::Signature::from_bytes(&sig_bytes);
            Ok(verifying_key.verify(msg, &signature).is_ok())
        }
        KeyScheme::Secp256k1Schnorr => {
            anyhow::bail!("KeyScheme::Secp256k1Schnorr has no implementation yet")
        }
        KeyScheme::Unspecified => anyhow::bail!("KeyScheme::Unspecified cannot be verified"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ed25519_sign_and_verify_round_trips() {
        let signer = Ed25519Signer::generate();
        let msg = b"hello provenance";
        let sig = signer.sign(msg);
        let ok = verify(KeyScheme::Ed25519, signer.public_key(), msg, &sig).unwrap();
        assert!(ok, "a genuine signature must verify");
    }

    #[test]
    fn ed25519_tampered_message_fails_verify() {
        let signer = Ed25519Signer::generate();
        let sig = signer.sign(b"hello provenance");
        let ok = verify(
            KeyScheme::Ed25519,
            signer.public_key(),
            b"hello PROVENANCE",
            &sig,
        )
        .unwrap();
        assert!(!ok, "a signature over a different message must not verify");
    }

    #[test]
    fn ed25519_tampered_signature_fails_verify() {
        let signer = Ed25519Signer::generate();
        let msg = b"hello provenance";
        let mut sig = signer.sign(msg);
        sig[0] ^= 0xFF;
        let ok = verify(KeyScheme::Ed25519, signer.public_key(), msg, &sig).unwrap();
        assert!(!ok, "a flipped signature byte must not verify");
    }

    #[test]
    fn secp256k1_schnorr_verify_returns_err_not_panic() {
        let result = verify(KeyScheme::Secp256k1Schnorr, &[0u8; 32], b"msg", &[0u8; 64]);
        assert!(
            result.is_err(),
            "unimplemented scheme must error, not panic or silently pass"
        );
    }

    #[test]
    fn from_bytes_round_trips_through_secret() {
        let original = Ed25519Signer::generate();
        let restored = Ed25519Signer::from_bytes(&original.secret_bytes()).unwrap();
        assert_eq!(original.public_key(), restored.public_key());
    }
}

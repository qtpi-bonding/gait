//! Local, per-gait-dir keyring — schema supports multiple independently
//! revocable keys (matching grorg's `AgentKey` shape); v1 generates and uses
//! exactly one, non-revocable. NOT a proto message: a struct that carries
//! `private_key` must never be reachable from the log/wire codegen surface —
//! see the design spec's §4.

use crate::{Ed25519Signer, KeyScheme, Signer};
use serde::{Deserialize, Serialize};
use std::path::Path;

pub const KEYRING_FILE_NAME: &str = "keyring.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyringEntry {
    pub id: String,
    pub scheme: KeyScheme,
    #[serde(with = "hex_bytes")]
    pub public_key: Vec<u8>,
    #[serde(with = "hex_bytes")]
    pub private_key: Vec<u8>,
    pub created_at: String,
    pub revoked_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Keyring {
    pub keys: Vec<KeyringEntry>,
}

mod hex_bytes {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&hex::encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(d)?;
        hex::decode(&s).map_err(serde::de::Error::custom)
    }
}

impl Keyring {
    /// A fresh keyring with exactly one ed25519 entry.
    pub fn generate() -> Self {
        let signer = Ed25519Signer::generate();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs().to_string())
            .unwrap_or_default();
        Self {
            keys: vec![KeyringEntry {
                id: hex::encode(&signer.public_key()[..8]),
                scheme: signer.scheme(),
                public_key: signer.public_key().to_vec(),
                private_key: signer.secret_bytes().to_vec(),
                created_at: now,
                revoked_at: None,
            }],
        }
    }

    fn path(gait_dir: &Path) -> std::path::PathBuf {
        gait_dir.join(KEYRING_FILE_NAME)
    }

    pub fn load(gait_dir: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(Self::path(gait_dir))?;
        Ok(serde_json::from_str(&text)?)
    }

    pub fn save(&self, gait_dir: &Path) -> anyhow::Result<()> {
        let text = serde_json::to_string_pretty(self)?;
        std::fs::write(Self::path(gait_dir), text)?;
        Ok(())
    }

    /// The active signer: the first non-revoked entry. Trivially correct for
    /// a keyring of one; multi-key selection logic doesn't exist yet.
    pub fn signer(&self) -> anyhow::Result<Ed25519Signer> {
        let entry = self
            .keys
            .iter()
            .find(|k| k.revoked_at.is_none())
            .ok_or_else(|| anyhow::anyhow!("keyring has no active (non-revoked) key"))?;
        match entry.scheme {
            KeyScheme::Ed25519 => Ed25519Signer::from_bytes(&entry.private_key),
            other => anyhow::bail!("no signer implementation for scheme {other:?}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_produces_one_active_ed25519_entry() {
        let kr = Keyring::generate();
        assert_eq!(kr.keys.len(), 1);
        assert_eq!(kr.keys[0].scheme, KeyScheme::Ed25519);
        assert!(kr.keys[0].revoked_at.is_none());
    }

    #[test]
    fn save_then_load_round_trips_and_signer_works() {
        let dir = tempfile::TempDir::new().unwrap();
        let kr = Keyring::generate();
        kr.save(dir.path()).unwrap();

        let loaded = Keyring::load(dir.path()).unwrap();
        assert_eq!(loaded.keys[0].public_key, kr.keys[0].public_key);

        let signer = loaded.signer().unwrap();
        let sig = signer.sign(b"msg");
        assert!(crate::verify(KeyScheme::Ed25519, signer.public_key(), b"msg", &sig).unwrap());
    }

    #[test]
    fn load_missing_file_errors() {
        let dir = tempfile::TempDir::new().unwrap();
        assert!(Keyring::load(dir.path()).is_err());
    }
}

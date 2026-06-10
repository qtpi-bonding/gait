//! `content_hash` pipeline — body integrity given a trusted `version`.
//!
//! Per the design spec (§7), the hash is **SHA-256** over **JCS (RFC 8785) of the
//! canonical proto3-JSON object of the event `body`** — the externally-tagged
//! object `{"<arm>":{…}}` (e.g. `{"payload":{"userTurn":{…}}}`). `version` is
//! prefixed into the hash input so a flipped `version` cannot verify under a
//! different scheme. JCS makes the digest reproducible across languages.
//!
//! Frozen scheme (part of the `version` contract — changing it changes every
//! hash): `SHA-256( "v" || version_decimal || "." || JCS(body_json) )`, lowercase
//! hex. This attests body integrity given a trusted version; it is NOT a
//! standalone tamper-evidence chain (that is the deferred Merkle level-C).

use sha2::{Digest, Sha256};

/// Compute the `content_hash` for an event body.
///
/// `body` is the externally-tagged proto3-JSON object of the `Event.body` oneof
/// (e.g. `{"payload":{…}}` / `{"redact":{…}}` / `{"replace":{…}}`).
pub fn content_hash(version: u32, body: &serde_json::Value) -> String {
    let canonical = serde_jcs::to_string(body).expect("JCS-canonicalize body");
    let mut hasher = Sha256::new();
    hasher.update(b"v");
    hasher.update(version.to_string().as_bytes());
    hasher.update(b".");
    hasher.update(canonical.as_bytes());
    hex::encode(hasher.finalize())
}

//! Verifies `Event.provenance.signature` against `Event.content_hash`.
//! Lives in `gait-identity`, not `gait-proto`: `gait-proto` cannot depend on
//! this crate's scheme-dispatched crypto (would cycle).

use gait_proto::gait::log::v1::Event;
use identity_crypto::KeyScheme;

/// True only if `event` carries provenance with a signature that verifies
/// against `event.content_hash` under the declared scheme. Any failure mode
/// (no provenance, unknown scheme, malformed key/signature bytes, bad
/// signature) returns `false` — never panics. Callers must treat `false` as
/// "untrusted," not as "corrupt" — see the design spec §7.
pub fn verify_provenance(event: &Event) -> bool {
    let Some(provenance) = &event.provenance else {
        return false;
    };
    let Ok(scheme) = KeyScheme::try_from(provenance.scheme) else {
        return false;
    };
    crate::verify(
        scheme,
        &provenance.from,
        event.content_hash.as_bytes(),
        &provenance.signature,
    )
    .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Ed25519Signer, Signer};
    use gait_proto::envelope::{mint, EventScope};
    use gait_proto::gait::log::v1::{event, payload, Payload, Provenance, UserText};

    fn user_text_event() -> Event {
        let body = event::Body::Payload(Payload {
            kind: Some(payload::Kind::UserText(UserText {
                content: "hi".into(),
            })),
        });
        mint(body, None, EventScope::Session)
    }

    #[test]
    fn genuine_signature_verifies() {
        let signer = Ed25519Signer::generate();
        let mut ev = user_text_event();
        let signature = signer.sign(ev.content_hash.as_bytes());
        ev.provenance = Some(Provenance {
            from: signer.public_key().to_vec(),
            to: vec![],
            context: String::new(),
            signature,
            scheme: signer.scheme() as i32,
            delegation_cert: None,
        });
        assert!(verify_provenance(&ev));
    }

    #[test]
    fn tampered_content_hash_fails_verification() {
        let signer = Ed25519Signer::generate();
        let mut ev = user_text_event();
        let signature = signer.sign(ev.content_hash.as_bytes());
        ev.provenance = Some(Provenance {
            from: signer.public_key().to_vec(),
            to: vec![],
            context: String::new(),
            signature,
            scheme: signer.scheme() as i32,
            delegation_cert: None,
        });
        ev.content_hash = "tampered".to_string(); // simulate a corrupted/edited event
        assert!(!verify_provenance(&ev));
    }

    #[test]
    fn missing_provenance_is_not_verified() {
        let ev = user_text_event();
        assert!(!verify_provenance(&ev));
    }

    #[test]
    fn empty_signature_placeholder_events_are_not_verified() {
        // Today's on-disk placeholder shape: from = "agent:xxx" text, no
        // signature, scheme unspecified. Must surface as untrusted, not panic.
        let mut ev = user_text_event();
        ev.provenance = Some(Provenance {
            from: b"agent:s1234abcd".to_vec(),
            to: vec![],
            context: String::new(),
            signature: vec![],
            scheme: 0, // KeyScheme::Unspecified
            delegation_cert: None,
        });
        assert!(!verify_provenance(&ev));
    }
}

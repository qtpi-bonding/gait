//! Verifies a direct `Provenance.signature` against an event's
//! `content_hash`. Lives in `gait-identity`, not `gait-proto`: `gait-proto`
//! cannot depend on this crate's scheme-dispatched crypto (would cycle).

use gait_proto::gait::log::v1::Provenance;
use identity_crypto::KeyScheme;

/// True only if `provenance` is a direct signature that verifies against
/// `content_hash` under its declared scheme. Any failure mode (unknown
/// scheme, malformed key/signature bytes, bad signature) returns `false` —
/// never panics. Callers must treat `false` as "untrusted," not as
/// "corrupt" — see the design spec §7.
///
/// Must NEVER be called with an attested `Provenance` (one where
/// `attested_author_agent_id`/`attested_verification_outcome` are set) —
/// there is no per-message embedded signature to check for those; their
/// authenticity was already established once, by verifying the enclosing
/// `DeliveryAttestation`'s single signature before the event was ever
/// appended. Calling this on an attested entry always returns `false`, so a
/// caller mixing up the two paths fails loudly (as "unverified") rather than
/// silently misreporting a meaningless check as passed or failed.
pub fn verify_provenance(provenance: &Provenance, content_hash: &str) -> bool {
    if provenance.attested_author_agent_id.is_some()
        || provenance.attested_verification_outcome.is_some()
    {
        return false;
    }
    let Ok(scheme) = KeyScheme::try_from(provenance.scheme) else {
        return false;
    };
    crate::verify(
        scheme,
        &provenance.from,
        content_hash.as_bytes(),
        &provenance.signature,
    )
    .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Ed25519Signer, Signer};
    use gait_proto::envelope::{mint, EventScope};
    use gait_proto::gait::log::v1::{event, payload, Event, Payload, Provenance, UserText};

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
        let ev = user_text_event();
        let signature = signer.sign(ev.content_hash.as_bytes());
        let provenance = Provenance {
            from: signer.public_key().to_vec(),
            to: vec![],
            context: String::new(),
            signature,
            scheme: signer.scheme() as i32,
            delegation_cert: None,
            attested_author_agent_id: None,
            attested_verification_outcome: None,
        };
        assert!(verify_provenance(&provenance, &ev.content_hash));
    }

    #[test]
    fn tampered_content_hash_fails_verification() {
        let signer = Ed25519Signer::generate();
        let ev = user_text_event();
        let signature = signer.sign(ev.content_hash.as_bytes());
        let provenance = Provenance {
            from: signer.public_key().to_vec(),
            to: vec![],
            context: String::new(),
            signature,
            scheme: signer.scheme() as i32,
            delegation_cert: None,
            attested_author_agent_id: None,
            attested_verification_outcome: None,
        };
        assert!(!verify_provenance(&provenance, "tampered")); // simulate a corrupted/edited event
    }

    #[test]
    fn empty_signature_placeholder_events_are_not_verified() {
        // Today's on-disk placeholder shape: from = "agent:xxx" text, no
        // signature, scheme unspecified. Must surface as untrusted, not panic.
        let ev = user_text_event();
        let provenance = Provenance {
            from: b"agent:s1234abcd".to_vec(),
            to: vec![],
            context: String::new(),
            signature: vec![],
            scheme: 0, // KeyScheme::Unspecified
            delegation_cert: None,
            attested_author_agent_id: None,
            attested_verification_outcome: None,
        };
        assert!(!verify_provenance(&provenance, &ev.content_hash));
    }

    #[test]
    fn attested_provenance_is_never_treated_as_verified() {
        // There is no per-message embedded signature to check for an
        // attested Provenance -- calling this function on one at all is a
        // caller bug, and it must fail loudly (false), never silently
        // report a meaningless check as passed.
        let ev = user_text_event();
        let provenance = Provenance {
            attested_author_agent_id: Some("agent-2".into()),
            attested_verification_outcome: Some(
                identity_crypto::VerificationOutcome::DirectKey as i32,
            ),
            ..Default::default()
        };
        assert!(!verify_provenance(&provenance, &ev.content_hash));
    }
}

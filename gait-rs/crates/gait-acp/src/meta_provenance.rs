//! Parses the `"gait/provenance"` key out of ACP's `PromptRequest._meta`
//! into a typed provenance claim. Absence remains ordinary human input;
//! malformed or unsupported claims are retained as rejected input so they
//! cannot be mistaken for provenance-free text during log replay.
use base64::Engine as _;
use gait_core::port::input::ProvenanceClaim;
use gait_core::proto::gait::log::v1::Provenance;
use identity_crypto::KeyScheme;

const META_KEY: &str = "gait/provenance";
const SUPPORTED_VERSION: u64 = 2;
const SUPPORTED_SCHEME: &str = "ed25519";

pub fn provenance_from_meta(
    meta: &Option<serde_json::Map<String, serde_json::Value>>,
) -> ProvenanceClaim {
    let Some(entry) = meta.as_ref().and_then(|meta| meta.get(META_KEY)) else {
        return ProvenanceClaim::Absent;
    };
    let Some(obj) = entry.as_object() else {
        return ProvenanceClaim::Rejected;
    };

    if obj.get("v").and_then(serde_json::Value::as_u64) != Some(SUPPORTED_VERSION) {
        return ProvenanceClaim::Rejected;
    }
    if obj.get("scheme").and_then(serde_json::Value::as_str) != Some(SUPPORTED_SCHEME) {
        return ProvenanceClaim::Rejected;
    }

    let Some(from_b64) = obj.get("from").and_then(serde_json::Value::as_str) else {
        return ProvenanceClaim::Rejected;
    };
    let Some(sig_b64) = obj.get("signature").and_then(serde_json::Value::as_str) else {
        return ProvenanceClaim::Rejected;
    };
    let engine = base64::engine::general_purpose::STANDARD;
    let Ok(from) = engine.decode(from_b64) else {
        return ProvenanceClaim::Rejected;
    };
    let Ok(signature) = engine.decode(sig_b64) else {
        return ProvenanceClaim::Rejected;
    };

    let Some(context) = optional_string(obj, "context") else {
        return ProvenanceClaim::Rejected;
    };
    let Some(to) = optional_string_array(obj, "to") else {
        return ProvenanceClaim::Rejected;
    };

    ProvenanceClaim::Direct(Provenance {
        from,
        to,
        context,
        signature,
        scheme: KeyScheme::Ed25519 as i32,
        delegation_cert: None,
        verified_author_id: None,
        verification_outcome: None,
    })
}

fn optional_string(obj: &serde_json::Map<String, serde_json::Value>, key: &str) -> Option<String> {
    match obj.get(key) {
        None => Some(String::new()),
        Some(value) => value.as_str().map(str::to_string),
    }
}

fn optional_string_array(
    obj: &serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Option<Vec<String>> {
    match obj.get(key) {
        None => Some(Vec::new()),
        Some(value) => value
            .as_array()?
            .iter()
            .map(|item| item.as_str().map(str::to_string))
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b64(bytes: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    #[test]
    fn absent_meta_yields_none() {
        assert!(matches!(
            provenance_from_meta(&None),
            ProvenanceClaim::Absent
        ));
    }

    #[test]
    fn meta_without_the_key_yields_none() {
        let meta = serde_json::Map::from_iter([(
            "some-other-key".to_string(),
            serde_json::json!("irrelevant"),
        )]);
        assert!(matches!(
            provenance_from_meta(&Some(meta)),
            ProvenanceClaim::Absent
        ));
    }

    #[test]
    fn well_formed_entry_parses_into_provenance() {
        let from = vec![1u8; 32];
        let sig = vec![2u8; 64];
        let meta = serde_json::Map::from_iter([(
            "gait/provenance".to_string(),
            serde_json::json!({
                "v": 2,
                "scheme": "ed25519",
                "from": b64(&from),
                "signature": b64(&sig),
                "context": "room:general",
                "to": ["session-abc"],
            }),
        )]);
        let ProvenanceClaim::Direct(p) = provenance_from_meta(&Some(meta)) else {
            panic!("must parse")
        };
        assert_eq!(p.from, from);
        assert_eq!(p.signature, sig);
        assert_eq!(p.context, "room:general");
        assert_eq!(p.to, vec!["session-abc".to_string()]);
        assert_eq!(p.scheme, gait_identity::KeyScheme::Ed25519 as i32);
    }

    #[test]
    fn missing_optional_fields_default() {
        let meta = serde_json::Map::from_iter([(
            "gait/provenance".to_string(),
            serde_json::json!({
                "v": 2,
                "scheme": "ed25519",
                "from": b64(&[1u8; 32]),
                "signature": b64(&[2u8; 64]),
            }),
        )]);
        let ProvenanceClaim::Direct(p) = provenance_from_meta(&Some(meta)) else {
            panic!("must parse")
        };
        assert_eq!(p.context, "");
        assert!(p.to.is_empty());
    }

    #[test]
    fn malformed_base64_is_preserved_as_a_rejected_claim() {
        let meta = serde_json::Map::from_iter([(
            "gait/provenance".to_string(),
            serde_json::json!({
                "v": 2, "scheme": "ed25519",
                "from": "not-valid-base64!", "signature": "also-bad",
            }),
        )]);
        assert!(matches!(
            provenance_from_meta(&Some(meta)),
            ProvenanceClaim::Rejected
        ));
    }

    #[test]
    fn wrong_shape_is_preserved_as_a_rejected_claim() {
        let meta = serde_json::Map::from_iter([(
            "gait/provenance".to_string(),
            serde_json::json!("just a string, not an object"),
        )]);
        assert!(matches!(
            provenance_from_meta(&Some(meta)),
            ProvenanceClaim::Rejected
        ));
    }

    #[test]
    fn legacy_version_is_preserved_as_a_rejected_claim() {
        let meta = serde_json::Map::from_iter([(
            "gait/provenance".to_string(),
            serde_json::json!({
                "v": 1, "scheme": "ed25519",
                "from": b64(&[1u8; 32]), "signature": b64(&[2u8; 64]),
            }),
        )]);
        assert!(matches!(
            provenance_from_meta(&Some(meta)),
            ProvenanceClaim::Rejected
        ));
    }

    #[test]
    fn unknown_scheme_is_preserved_as_a_rejected_claim() {
        let meta = serde_json::Map::from_iter([(
            "gait/provenance".to_string(),
            serde_json::json!({
                "v": 2, "scheme": "secp256k1_schnorr",
                "from": b64(&[1u8; 32]), "signature": b64(&[2u8; 64]),
            }),
        )]);
        assert!(matches!(
            provenance_from_meta(&Some(meta)),
            ProvenanceClaim::Rejected
        ));
    }
}

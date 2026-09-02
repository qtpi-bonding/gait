//! Parses the `"gait/provenance"` key out of ACP's `PromptRequest._meta`
//! into a `Provenance`. Fails open (returns `None`) on anything malformed —
//! a plain human ACP client sends no such key at all, and a badly-formed
//! one from a misbehaving peer must not block the prompt.
use base64::Engine as _;
use gait_core::proto::gait::log::v1::Provenance;
use identity_crypto::KeyScheme;

const META_KEY: &str = "gait/provenance";
const SUPPORTED_VERSION: u64 = 1;
const SUPPORTED_SCHEME: &str = "ed25519";

pub fn provenance_from_meta(
    meta: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Option<Provenance> {
    let entry = meta.as_ref()?.get(META_KEY)?;
    let obj = entry.as_object()?;

    if obj.get("v")?.as_u64()? != SUPPORTED_VERSION {
        return None;
    }
    if obj.get("scheme")?.as_str()? != SUPPORTED_SCHEME {
        return None;
    }

    let from_b64 = obj.get("from")?.as_str()?;
    let sig_b64 = obj.get("signature")?.as_str()?;
    let engine = base64::engine::general_purpose::STANDARD;
    let from = engine.decode(from_b64).ok()?;
    let signature = engine.decode(sig_b64).ok()?;

    let context = obj
        .get("context")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let to = obj
        .get("to")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    Some(Provenance {
        from,
        to,
        context,
        signature,
        scheme: KeyScheme::Ed25519 as i32,
        delegation_cert: None,
        attested_author_agent_id: None,
        attested_verification_outcome: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b64(bytes: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    #[test]
    fn absent_meta_yields_none() {
        assert!(provenance_from_meta(&None).is_none());
    }

    #[test]
    fn meta_without_the_key_yields_none() {
        let meta = serde_json::Map::from_iter([(
            "some-other-key".to_string(),
            serde_json::json!("irrelevant"),
        )]);
        assert!(provenance_from_meta(&Some(meta)).is_none());
    }

    #[test]
    fn well_formed_entry_parses_into_provenance() {
        let from = vec![1u8; 32];
        let sig = vec![2u8; 64];
        let meta = serde_json::Map::from_iter([(
            "gait/provenance".to_string(),
            serde_json::json!({
                "v": 1,
                "scheme": "ed25519",
                "from": b64(&from),
                "signature": b64(&sig),
                "context": "room:general",
                "to": ["session-abc"],
            }),
        )]);
        let p = provenance_from_meta(&Some(meta)).expect("must parse");
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
                "v": 1,
                "scheme": "ed25519",
                "from": b64(&[1u8; 32]),
                "signature": b64(&[2u8; 64]),
            }),
        )]);
        let p = provenance_from_meta(&Some(meta)).expect("must parse");
        assert_eq!(p.context, "");
        assert!(p.to.is_empty());
    }

    #[test]
    fn malformed_base64_fails_open_to_none() {
        let meta = serde_json::Map::from_iter([(
            "gait/provenance".to_string(),
            serde_json::json!({
                "v": 1, "scheme": "ed25519",
                "from": "not-valid-base64!", "signature": "also-bad",
            }),
        )]);
        assert!(provenance_from_meta(&Some(meta)).is_none());
    }

    #[test]
    fn wrong_shape_fails_open_to_none() {
        let meta = serde_json::Map::from_iter([(
            "gait/provenance".to_string(),
            serde_json::json!("just a string, not an object"),
        )]);
        assert!(provenance_from_meta(&Some(meta)).is_none());
    }

    #[test]
    fn wrong_version_fails_open_to_none() {
        let meta = serde_json::Map::from_iter([(
            "gait/provenance".to_string(),
            serde_json::json!({
                "v": 2, "scheme": "ed25519",
                "from": b64(&[1u8; 32]), "signature": b64(&[2u8; 64]),
            }),
        )]);
        assert!(provenance_from_meta(&Some(meta)).is_none());
    }

    #[test]
    fn unknown_scheme_fails_open_to_none() {
        let meta = serde_json::Map::from_iter([(
            "gait/provenance".to_string(),
            serde_json::json!({
                "v": 1, "scheme": "secp256k1_schnorr",
                "from": b64(&[1u8; 32]), "signature": b64(&[2u8; 64]),
            }),
        )]);
        assert!(provenance_from_meta(&Some(meta)).is_none());
    }
}

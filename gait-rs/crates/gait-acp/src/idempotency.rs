//! Dedup for `PromptRequest`s carrying a `"gait/idempotency_key"` `_meta`
//! entry — a generic, gait-owned ACP extension, not tied to any specific
//! client's redelivery semantics. A client that resends the same logical
//! prompt (e.g. after a dropped connection, unsure whether the first attempt
//! was received) tags both attempts with the same key; gait-acp answers the
//! second one with `already_seen_error` instead of running the turn twice.
const META_KEY: &str = "gait/idempotency_key";

/// Extract the idempotency key from `PromptRequest._meta`, if present and
/// well-formed. Fails open (`None`) on anything malformed or absent: a client
/// that doesn't send this key (or sends it malformed) must not be blocked from
/// prompting; dedup simply doesn't apply to that request.
pub fn idempotency_key_from_meta(
    meta: &Option<serde_json::Map<String, serde_json::Value>>,
) -> Option<String> {
    meta.as_ref()?.get(META_KEY)?.as_str().map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn absent_meta_yields_none() {
        assert!(idempotency_key_from_meta(&None).is_none());
    }
    #[test]
    fn meta_without_the_key_yields_none() {
        let meta = serde_json::Map::from_iter([(
            "some-other-key".to_string(),
            serde_json::json!("irrelevant"),
        )]);
        assert!(idempotency_key_from_meta(&Some(meta)).is_none());
    }
    #[test]
    fn well_formed_key_parses() {
        let meta = serde_json::Map::from_iter([(
            "gait/idempotency_key".to_string(),
            serde_json::json!("delivery-abc-123"),
        )]);
        assert_eq!(
            idempotency_key_from_meta(&Some(meta)).as_deref(),
            Some("delivery-abc-123")
        );
    }
    #[test]
    fn non_string_value_fails_open_to_none() {
        let meta = serde_json::Map::from_iter([(
            "gait/idempotency_key".to_string(),
            serde_json::json!({"not": "a string"}),
        )]);
        assert!(idempotency_key_from_meta(&Some(meta)).is_none());
    }
}

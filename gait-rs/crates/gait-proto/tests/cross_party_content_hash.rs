// crates/gait-proto/tests/cross_party_content_hash.rs
use gait_proto::envelope::{mint, EventScope, LOG_VERSION};
use gait_proto::gait::log::v1::{event, payload, Payload, UserText};
use gait_proto::hash::content_hash;

/// Simulates an external, non-Rust sender that only knows the documented
/// wire contract (LOG_VERSION, protobuf-JSON field names, JCS) — not gait's
/// generated types. If this test passes, an external signer really can
/// precompute `content_hash` before gait ever sees the message.
#[test]
fn hand_built_json_matches_gaits_own_content_hash() {
    let text = "hello from an external sender";

    // What gait itself computes when it mints this event.
    let body = event::Body::Payload(Payload {
        kind: Some(payload::Kind::UserText(UserText {
            content: text.to_string(),
        })),
    });
    let gait_event = mint(body, None, EventScope::Session);

    // What an external sender, following only the documented JSON contract,
    // would build with no access to gait's Rust types at all.
    let hand_built_body = serde_json::json!({
        "payload": {
            "userText": {
                "content": text
            }
        }
    });
    let external_hash = content_hash(LOG_VERSION, &hand_built_body);

    assert_eq!(
        gait_event.content_hash, external_hash,
        "hand-built protobuf-JSON body must hash identically to gait's own mint()"
    );
}

#[test]
fn a_single_extra_or_missing_field_changes_the_hash() {
    // Guards against a false-positive reproducibility claim: if omitting a
    // default-valued field silently produced the same hash for the wrong
    // reason, this would catch it changing when content actually differs.
    let a = serde_json::json!({"payload": {"userText": {"content": "hello"}}});
    let b = serde_json::json!({"payload": {"userText": {"content": "hello!"}}});
    assert_ne!(content_hash(LOG_VERSION, &a), content_hash(LOG_VERSION, &b));
}

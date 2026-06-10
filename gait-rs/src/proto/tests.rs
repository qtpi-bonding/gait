//! Phase 0 gate — proves the generated proto types work end-to-end before any
//! real wiring depends on them:
//!   1. JSON round-trips (pbjson serde is live);
//!   2. the wire is proto3-canonical (camelCase, externally-tagged oneof);
//!   3. `content_hash` recomputes stably and is sensitive to body + version.

use super::gait::log::v1::{event, payload, CodebaseState, Event, Payload, UserTurn};
use super::hash::content_hash;
use serde_json::json;

fn sample_event() -> Event {
    Event {
        version: 1,
        id: "00000000-0000-4000-8000-000000000001".into(),
        seq: 1,
        ts: 1_700_000_000_000,
        content_hash: String::new(),
        parents: vec![],
        body: Some(event::Body::Payload(Payload {
            kind: Some(payload::Kind::UserTurn(UserTurn {
                content: "hello".into(),
                from: Some("user".into()),
                codebase: Some(CodebaseState {
                    hash: "abc123".into(),
                    dirty: true,
                    dirty_files: vec!["src/main.rs".into()],
                }),
                injected_files: vec!["README.md".into()],
            })),
        })),
    }
}

/// The externally-tagged body object `{"<arm>":{…}}` that `content_hash` covers.
/// Envelope fields (incl. `content_hash` itself) are excluded by construction.
fn body_object(ev: &Event) -> serde_json::Value {
    let full = serde_json::to_value(ev).expect("serialize event");
    for arm in ["payload", "redact", "replace"] {
        if let Some(b) = full.get(arm) {
            return json!({ arm: b });
        }
    }
    panic!("event has no body arm set: {full}");
}

#[test]
fn event_json_roundtrips() {
    let ev = sample_event();
    let s = serde_json::to_string(&ev).expect("serialize");
    let back: Event = serde_json::from_str(&s).expect("deserialize");
    assert_eq!(ev, back, "round-trip must be lossless");
}

#[test]
fn json_is_canonical_proto3() {
    let mut ev = sample_event();
    // proto3 canonical JSON OMITS default-valued fields (empty string, [], 0).
    // This is the "frozen field-omission" the spec binds into the version
    // contract — flip it and every content_hash changes. Stamp a non-default
    // value so the envelope camelCase field is actually present to assert on.
    ev.content_hash = "deadbeef".into();
    let v = serde_json::to_value(&ev).expect("serialize");

    // Envelope is camelCase.
    assert!(v.get("contentHash").is_some(), "expected camelCase envelope: {v}");
    // `parents: []` is a default → omitted (documents the omission rule).
    assert!(v.get("parents").is_none(), "default repeated is omitted: {v}");

    // Body oneof is externally tagged: {"payload": {"userTurn": {…}}}.
    let ut = &v["payload"]["userTurn"];
    assert_eq!(ut["content"], json!("hello"));

    // Nested message fields are camelCase too.
    assert!(ut.get("injectedFiles").is_some(), "expected nested camelCase: {ut}");
}

#[test]
fn content_hash_recomputes_stably() {
    let mut ev = sample_event();
    let h1 = content_hash(ev.version, &body_object(&ev));
    assert_eq!(h1.len(), 64, "SHA-256 lowercase hex");

    // Stamp it, send through a full JSON round-trip, recompute — must be identical.
    ev.content_hash = h1.clone();
    let back: Event =
        serde_json::from_str(&serde_json::to_string(&ev).unwrap()).unwrap();
    assert_eq!(content_hash(back.version, &body_object(&back)), h1);
}

#[test]
fn content_hash_is_sensitive() {
    let base = sample_event();
    let h = content_hash(base.version, &body_object(&base));

    // A changed body changes the hash.
    let mut mutated = sample_event();
    if let Some(event::Body::Payload(p)) = &mut mutated.body {
        if let Some(payload::Kind::UserTurn(ut)) = &mut p.kind {
            ut.content = "goodbye".into();
        }
    }
    assert_ne!(content_hash(mutated.version, &body_object(&mutated)), h);

    // A changed version changes the hash (version is bound into the input).
    assert_ne!(content_hash(base.version + 1, &body_object(&base)), h);
}

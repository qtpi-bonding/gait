//! Phase 0 gate — proves the generated proto types work end-to-end before any
//! real wiring depends on them:
//!   1. JSON round-trips (pbjson serde is live);
//!   2. the wire is proto3-canonical (camelCase, externally-tagged oneof);
//!   3. `content_hash` recomputes stably and is sensitive to body + version.

use super::envelope::body_json;
use super::gait::log::v1::{event, payload, Event, Payload, UserText};
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
        turn: 0,
        step: 0,
        body: Some(event::Body::Payload(Payload {
            kind: Some(payload::Kind::UserText(UserText {
                content: "hello".into(),
                from: Some("user".into()),
            })),
        })),
    }
}

// The externally-tagged body object that `content_hash` covers lives in
// `super::envelope::body_json` — these tests reuse it rather than duplicating.

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
    assert!(
        v.get("contentHash").is_some(),
        "expected camelCase envelope: {v}"
    );
    // `parents: []` is a default → omitted (documents the omission rule).
    assert!(
        v.get("parents").is_none(),
        "default repeated is omitted: {v}"
    );

    // Body oneof is externally tagged: {"payload": {"userText": {…}}}.
    let ut = &v["payload"]["userText"];
    assert_eq!(ut["content"], json!("hello"));

    // Nested message fields are camelCase too.
    assert!(ut.get("from").is_some(), "expected nested camelCase: {ut}");
}

#[test]
fn content_hash_recomputes_stably() {
    let mut ev = sample_event();
    let h1 = content_hash(ev.version, &body_json(&ev));
    assert_eq!(h1.len(), 64, "SHA-256 lowercase hex");

    // Stamp it, send through a full JSON round-trip, recompute — must be identical.
    ev.content_hash = h1.clone();
    let back: Event = serde_json::from_str(&serde_json::to_string(&ev).unwrap()).unwrap();
    assert_eq!(content_hash(back.version, &body_json(&back)), h1);
}

#[test]
fn content_hash_is_sensitive() {
    let base = sample_event();
    let h = content_hash(base.version, &body_json(&base));

    // A changed body changes the hash.
    let mut mutated = sample_event();
    if let Some(event::Body::Payload(p)) = &mut mutated.body {
        if let Some(payload::Kind::UserText(ut)) = &mut p.kind {
            ut.content = "goodbye".into();
        }
    }
    assert_ne!(content_hash(mutated.version, &body_json(&mutated)), h);

    // A changed version changes the hash (version is bound into the input).
    assert_ne!(content_hash(base.version + 1, &body_json(&base)), h);
}

#[test]
fn agent_event_exists() {
    use super::gait::log::v1::{payload, Agent};
    let _ = payload::Kind::Agent(Agent::default());
}

#[test]
fn scope_session_variant_exists() {
    use super::gait::log::v1::Scope;
    assert_eq!(Scope::Session as i32, 4);
}

#[test]
fn llm_params_round_trips_via_json() {
    use crate::gait::log::v1::{llm_params::ProviderParams, AnthropicLlmParams, LlmParams};

    let params = LlmParams {
        max_tokens: Some(1000),
        temperature: Some(0.7),
        top_p: None,
        // show_thinking is NOT a field on LlmParams — it lives in GaitConfig
        provider_params: Some(ProviderParams::Anthropic(AnthropicLlmParams {
            top_k: Some(10),
            effort: Some("high".into()),
            thinking: None,
        })),
    };

    let json = serde_json::to_string(&params).expect("serialize");
    let back: LlmParams = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back.max_tokens, Some(1000));
    assert_eq!(back.temperature, Some(0.7));
    match back.provider_params {
        Some(ProviderParams::Anthropic(a)) => {
            assert_eq!(a.top_k, Some(10));
            assert_eq!(a.effort, Some("high".into()));
        }
        other => panic!("expected Anthropic params, got {other:?}"),
    }
}

#[test]
fn thinking_block_round_trips_via_json() {
    use crate::gait::log::v1::ThinkingBlock;

    // Both content and signature must survive a round-trip — signature is
    // needed for Anthropic multi-turn replay.
    let block = ThinkingBlock {
        content: "I thought about it".into(),
        signature: "sig_abc123".into(),
    };
    let json = serde_json::to_string(&block).expect("serialize");
    let back: ThinkingBlock = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back.content, "I thought about it");
    assert_eq!(back.signature, "sig_abc123");
}

#[test]
fn agent_carries_llm_params_field() {
    use crate::gait::log::v1::{Agent, LlmParams};

    let agent = Agent {
        model: "anthropic/claude-opus-4-8".into(),
        llm_params: Some(LlmParams {
            max_tokens: Some(512),
            ..Default::default()
        }),
        ..Default::default()
    };
    let json = serde_json::to_string(&agent).expect("serialize");
    let back: Agent = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(
        back.llm_params.as_ref().and_then(|p| p.max_tokens),
        Some(512)
    );
}

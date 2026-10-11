//! Phase 0 gate — proves the generated proto types work end-to-end before any
//! real wiring depends on them:
//!   1. JSON round-trips (pbjson serde is live);
//!   2. the wire is proto3-canonical (camelCase, externally-tagged oneof);
//!   3. `content_hash` recomputes stably and is sensitive to body + version.
//!
//! Phase 2a (Task 1) extends this with binary-protobuf round-trip tests for the
//! new `gait.sandbox.v1` wire types (Impulse/Feedback/Scope/Signal).

use super::envelope::body_json;
use super::gait::log::v1::{event, payload, Event, Payload, UserText};
use super::hash::content_hash;
use serde_json::json;

fn sample_event() -> Event {
    Event {
        version: super::envelope::LOG_VERSION,
        id: "00000000-0000-4000-8000-000000000001".into(),
        seq: 1,
        ts: 1_700_000_000_000,
        content_hash: String::new(),
        parents: vec![],
        run: Some(0),
        turn: Some(0),
        step: None,
        body: Some(event::Body::Payload(Payload {
            kind: Some(payload::Kind::UserText(UserText {
                content: "hello".into(),
                ..Default::default()
            })),
        })),
        provenance: Vec::new(),
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

    // Nested message fields are camelCase too — assert on a genuinely
    // multi-word nested field (UserText.from was removed in favor of
    // Event.provenance, which has no multi-word field of its own at the
    // default value used here, so use an AgentUpdate payload instead).
    let mut au_ev = ev.clone();
    au_ev.body = Some(event::Body::Payload(Payload {
        kind: Some(payload::Kind::AgentUpdate(
            crate::gait::log::v1::AgentUpdate {
                session_id: "s1".into(),
                status: 0,
                summary: String::new(),
                items: None,
                idempotency_key: "s1-evt1".into(),
            },
        )),
    }));
    let au_v = serde_json::to_value(&au_ev).expect("serialize");
    let au = &au_v["payload"]["agentUpdate"];
    assert!(
        au.get("idempotencyKey").is_some(),
        "expected nested camelCase: {au}"
    );
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
            strict_tools: None,
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
fn agent_carries_inference_field() {
    use crate::gait::log::v1::{Agent, Inference, LlmParams};

    let agent = Agent {
        inference: Some(Inference {
            name: "deep".into(),
            model: "anthropic/claude-opus-4-8".into(),
            llm_params: Some(LlmParams {
                max_tokens: Some(512),
                ..Default::default()
            }),
            compaction_threshold_tokens: 128_000,
        }),
        ..Default::default()
    };
    let json = serde_json::to_string(&agent).expect("serialize");
    let back: Agent = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(
        back.inference
            .as_ref()
            .and_then(|i| i.llm_params.as_ref())
            .and_then(|p| p.max_tokens),
        Some(512)
    );
}

// ============================================================================
// Phase 2a — sandbox wire types (Task 1)
// ============================================================================
//
// Proves the binary protobuf hop works end-to-end: an `Impulse` constructed
// in Rust round-trips through `prost::Message::encode_to_vec` / `decode`. This
// is the foundation for the brain↔hand subprocess boundary; everything later
// in Phase 2a (Scope enforcement, NetworkScope) is layered on top of these
// generated types.

#[test]
fn impulse_round_trips_via_binary_prost() {
    use crate::gait::sandbox::v1::Impulse;
    use prost::Message as _;

    let impulse = Impulse {
        tool: "read".into(),
        args: None,
        requested: None,
        prefetch: None,
        shell_whitelist_patterns: vec![],
        output_capture_mode: crate::gait::sandbox::v1::OutputCaptureMode::Returned as i32,
        output_presentation: crate::gait::sandbox::v1::OutputPresentation::Unspecified as i32,
    };

    let bytes = prost::Message::encode_to_vec(&impulse);
    assert!(!bytes.is_empty(), "encoded Impulse must be non-empty");

    let decoded = Impulse::decode(&bytes[..]).expect("decode Impulse");
    assert_eq!(decoded.tool, "read");
    assert_eq!(decoded.args, None);
    assert_eq!(decoded.requested, None);
}

#[test]
fn feedback_carries_value_and_events() {
    use crate::gait::log::v1::Event;
    use crate::gait::sandbox::v1::Feedback;
    use pbjson_types::Value;
    use prost::Message as _;

    // Build a small log::v1::Event so we can verify the cross-package
    // repeated Event field actually decodes (it imports `gait.log.v1.Event`).
    let event = Event {
        version: 1,
        id: "00000000-0000-4000-8000-000000000002".into(),
        seq: 7,
        ts: 1_700_000_000_000,
        ..Default::default()
    };

    let feedback = Feedback {
        result: Some(Value {
            kind: Some(pbjson_types::value::Kind::StringValue("ok".into())),
        }),
        events: vec![event],
        denied: None,
        daemon: None,
        final_content: None,
        whitelist_match_items: vec![],
        sandbox_denied: None,
        is_error: None,
    };

    let bytes = prost::Message::encode_to_vec(&feedback);
    let decoded = Feedback::decode(&bytes[..]).expect("decode Feedback");
    assert_eq!(decoded.events.len(), 1);
    assert_eq!(decoded.events[0].id, "00000000-0000-4000-8000-000000000002");
    assert_eq!(decoded.events[0].seq, 7);
}

#[test]
fn scope_serializes_network_scope() {
    use crate::gait::sandbox::v1::{network_scope::Mode, Impulse, NetworkScope, Scope};
    use prost::Message as _;

    let impulse = Impulse {
        tool: "fetch".into(),
        requested: Some(Scope {
            read_all: true,
            readable: vec!["/repo".into()],
            writable: vec![],
            network: Some(NetworkScope {
                mode: Mode::Hosts as i32,
                hosts: vec!["example.com".into()],
            }),
            local_bind: false,
            read_deny: None,
            write_deny: None,
        }),
        ..Default::default()
    };

    let bytes = prost::Message::encode_to_vec(&impulse);
    let decoded = Impulse::decode(&bytes[..]).expect("decode Impulse with Scope");
    let scope = decoded.requested.expect("scope present");
    assert!(scope.read_all);
    assert_eq!(scope.readable, vec!["/repo".to_string()]);
    assert!(scope.writable.is_empty());
    let net = scope.network.expect("network scope present");
    assert_eq!(net.mode, Mode::Hosts as i32);
    assert_eq!(net.hosts, vec!["example.com".to_string()]);
}

#[test]
fn signal_envelope_discriminates_oneof() {
    use crate::gait::sandbox::v1::{signal::Body, Impulse, Signal};
    use prost::Message as _;

    let signal = Signal {
        corr_id: 42,
        body: Some(Body::Impulse(Impulse {
            tool: "read".into(),
            ..Default::default()
        })),
    };

    let bytes = prost::Message::encode_to_vec(&signal);
    let decoded = Signal::decode(&bytes[..]).expect("decode Signal");
    assert_eq!(decoded.corr_id, 42);
    match decoded.body {
        Some(Body::Impulse(imp)) => assert_eq!(imp.tool, "read"),
        other => panic!("expected Impulse arm, got {other:?}"),
    }
}

#[test]
fn brain_init_signal_round_trips() {
    use crate::gait::sandbox::v1::{signal, BrainInit, GitStatus, Signal};
    use prost::Message;
    let sig = Signal {
        corr_id: 0,
        body: Some(signal::Body::BrainInit(BrainInit {
            config_json: "{}".to_string(),
            events: vec![],
            git: Some(GitStatus {
                head_hash: "abc123".to_string(),
                dirty_files: vec!["src/main.rs".to_string()],
            }),
            profile_json: String::new(),
            daemons: vec![],
            credential_json: String::new(),
        })),
    };
    let bytes = sig.encode_to_vec();
    let back = Signal::decode(bytes.as_slice()).unwrap();
    match back.body {
        Some(signal::Body::BrainInit(b)) => {
            assert_eq!(b.config_json, "{}");
            assert_eq!(b.git.unwrap().head_hash, "abc123");
        }
        other => panic!("expected BrainInit, got {other:?}"),
    }
}

#[test]
fn hand_response_round_trips_both_arms() {
    use crate::gait::sandbox::v1::{hand_response, Feedback, HandError, HandResponse};
    use prost::Message;

    let ok = HandResponse {
        outcome: Some(hand_response::Outcome::Feedback(Feedback::default())),
    };
    let back = HandResponse::decode(ok.encode_to_vec().as_slice()).unwrap();
    assert!(matches!(
        back.outcome,
        Some(hand_response::Outcome::Feedback(_))
    ));

    let err = HandResponse {
        outcome: Some(hand_response::Outcome::Error(HandError {
            message: "boom".into(),
        })),
    };
    let back = HandResponse::decode(err.encode_to_vec().as_slice()).unwrap();
    match back.outcome {
        Some(hand_response::Outcome::Error(e)) => assert_eq!(e.message, "boom"),
        other => panic!("expected Error, got {other:?}"),
    }
}

#[test]
fn impulse_prefetch_and_feedback_final_content_roundtrip() {
    use crate::gait::sandbox::v1::{Feedback, Impulse, Prefetch};
    use prost::Message;

    let imp = Impulse {
        tool: "read".into(),
        args: None,
        requested: None,
        prefetch: Some(Prefetch {
            path: "/ws/a.rs".into(),
            content: Some("buf".into()),
        }),
        shell_whitelist_patterns: vec![],
        output_capture_mode: crate::gait::sandbox::v1::OutputCaptureMode::Returned as i32,
        output_presentation: crate::gait::sandbox::v1::OutputPresentation::Unspecified as i32,
    };
    let round = Impulse::decode(imp.encode_to_vec().as_slice()).unwrap();
    assert_eq!(round.prefetch.unwrap().content.as_deref(), Some("buf"));

    let fb = Feedback {
        result: None,
        events: vec![],
        denied: None,
        daemon: None,
        final_content: Some("new".into()),
        whitelist_match_items: vec![],
        sandbox_denied: None,
        is_error: None,
    };
    let round = Feedback::decode(fb.encode_to_vec().as_slice()).unwrap();
    assert_eq!(round.final_content.as_deref(), Some("new"));
}

#[test]
fn tool_result_checkpoint_field_round_trips() {
    use crate::gait::log::v1::{ProcessResult, ToolResult};
    let tr = ToolResult {
        call_id: "call-1".into(),
        output: "ok".into(),
        duration: None,
        process: Some(ProcessResult { exit_code: 0 }),
        checkpoint: Some("deadbeef".into()),
    };
    let bytes = prost::Message::encode_to_vec(&tr);
    let decoded = <ToolResult as prost::Message>::decode(bytes.as_slice()).unwrap();
    assert_eq!(decoded.checkpoint, Some("deadbeef".to_string()));
}

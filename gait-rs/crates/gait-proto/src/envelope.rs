//! Event envelope — construction + body-integrity verification over the
//! generated [`Event`] type.
//!
//! This is the **log-level** integrity layer (frozen as part of the
//! `LOG_VERSION` contract — see [`hash::content_hash`]): given a trusted
//! `version`, the SHA-256 of JCS(body) attests the body has not been mutated
//! since the event was stamped. It is NOT a Merkle chain (the prev-hash
//! linkage is deferred to a later phase). It covers ONLY the externally-
//! tagged body object `{"<arm>":{…}}`; envelope fields (incl.
//! `content_hash` itself) are excluded by construction.
//!
//! [`hash::content_hash`]: crate::hash::content_hash

use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::json;

use super::gait::log::v1::{event, Event};
use super::hash::content_hash;

/// Current log format generation. Part of the `content_hash` contract — bump
/// on any breaking change to the body encoding or hash scheme.
pub const LOG_VERSION: u32 = 3;

/// Hierarchy coordinates for an event. Presence is semantic: zero is a valid
/// index, so an absent parent can never be represented by a zero sentinel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventScope {
    Session,
    Run { run: u32 },
    Turn { run: u32, turn: u32 },
    Step { run: u32, turn: u32, step: u32 },
}

/// Whether the optional hierarchy coordinates form a complete prefix.
/// A turn cannot exist without a run, and a step cannot exist without both.
pub fn has_valid_scope_prefix(event: &Event) -> bool {
    !matches!(
        (event.run, event.turn, event.step),
        (None, Some(_), _) | (None, None, Some(_)) | (Some(_), None, Some(_))
    )
}

impl EventScope {
    fn coordinates(self) -> (Option<u32>, Option<u32>, Option<u32>) {
        match self {
            Self::Session => (None, None, None),
            Self::Run { run } => (Some(run), None, None),
            Self::Turn { run, turn } => (Some(run), Some(turn), None),
            Self::Step { run, turn, step } => (Some(run), Some(turn), Some(step)),
        }
    }
}

/// The externally-tagged body object `{"<arm>":{…}}` that `content_hash`
/// covers.
///
/// Serializes the whole event, then returns `{ "<arm>": <value> }` for
/// whichever of `payload` / `redact` / `replace` is present. Panics if none
/// is set — an unset body is a malformed event (the proto3 oneof cannot
/// enforce "exactly one arm set" at the wire level, but every well-formed
/// envelope minted via [`mint`] always has one).
pub fn body_json(event: &Event) -> serde_json::Value {
    let full = serde_json::to_value(event).expect("serialize event");
    for arm in ["payload", "redact", "replace"] {
        if let Some(b) = full.get(arm) {
            return json!({ arm: b });
        }
    }
    panic!("event has no body arm set: {full}");
}

/// Compute and stamp `content_hash` on the event in place.
///
/// Must be called AFTER every other field (incl. `body`) is set, and
/// BEFORE serialization — `content_hash` is the integrity attestation
/// of the body as it stands at the moment of stamping.
pub fn stamp_hash(event: &mut Event) {
    let version = event.version;
    event.content_hash = content_hash(version, &body_json(event));
}

/// Recompute the body hash and compare to the stored `content_hash`.
///
/// Returns `false` for any malformed event (no body arm set, or with a
/// `seq == 0` / empty `id` envelope — see [`mint`] invariants).
pub fn verify_hash(event: &Event) -> bool {
    // Defensive: an unset body is malformed; don't panic, just report unverified.
    if event.body.is_none() {
        return false;
    }
    if event.version != LOG_VERSION {
        return false;
    }
    let version = event.version;
    content_hash(version, &body_json(event)) == event.content_hash
}

/// Mint a new envelope around a body, chained after `prev` (`None` = root).
///
/// **Invariants** (guaranteed on every value returned by this fn):
///   * `seq >= 1` — `0` is reserved as the "unset / malformed" sentinel.
///   * `id` is a fresh UUIDv4.
///   * `ts` is wall-clock ms since UNIX epoch (`0` on clock error).
///   * `version == LOG_VERSION`.
///   * `parents` = `[]` for a root, `[prev.id.clone()]` for a child.
///   * hierarchy coordinates are stamped from an explicit [`EventScope`].
///   * `content_hash` is stamped LAST (attests the body as finally encoded).
pub fn mint(body: event::Body, prev: Option<&Event>, scope: EventScope) -> Event {
    let (seq, parents) = match prev {
        None => (1u64, Vec::new()),
        Some(p) => (p.seq + 1, vec![p.id.clone()]),
    };
    let (run, turn, step) = scope.coordinates();
    let mut ev = Event {
        version: LOG_VERSION,
        id: uuid::Uuid::new_v4().to_string(),
        seq,
        ts: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
        content_hash: String::new(),
        parents,
        run,
        turn,
        step,
        body: Some(body),
        provenance: Vec::new(),
    };
    stamp_hash(&mut ev);
    ev
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gait::log::v1::{payload, AssistantText, Payload};

    fn simple_payload() -> event::Body {
        event::Body::Payload(Payload {
            kind: Some(payload::Kind::AssistantText(AssistantText {
                content: "hi".into(),
            })),
        })
    }

    #[test]
    fn mint_root_then_child() {
        let root = mint(simple_payload(), None, EventScope::Session);
        assert_eq!(root.seq, 1, "root seq must be 1");
        assert!(root.parents.is_empty(), "root must have no parents");
        assert!(verify_hash(&root), "root must verify");

        let child = mint(simple_payload(), Some(&root), EventScope::Session);
        assert_eq!(child.seq, 2, "child seq must be prev+1");
        assert_eq!(
            child.parents,
            vec![root.id.clone()],
            "child parents = [prev.id]"
        );
        assert!(verify_hash(&child), "child must verify");
    }

    #[test]
    fn verify_detects_tamper() {
        let mut ev = mint(simple_payload(), None, EventScope::Session);
        assert!(verify_hash(&ev), "freshly-minted event must verify");

        // Mutate a field inside the body payload directly.
        if let Some(event::Body::Payload(p)) = &mut ev.body {
            if let Some(payload::Kind::AssistantText(at)) = &mut p.kind {
                at.content = "tampered".into();
            }
        }
        assert!(
            !verify_hash(&ev),
            "tampered body must fail verification (content_hash no longer matches)"
        );

        // Re-stamping repairs the envelope.
        stamp_hash(&mut ev);
        assert!(verify_hash(&ev), "re-stamped event must verify again");
    }

    #[test]
    fn verify_survives_json_round_trip() {
        let ev = mint(simple_payload(), None, EventScope::Session);
        let s = serde_json::to_string(&ev).expect("serialize");
        let back: Event = serde_json::from_str(&s).expect("deserialize");
        assert!(verify_hash(&back), "round-tripped event must still verify");
    }

    #[test]
    fn hierarchy_coordinates_must_form_a_prefix() {
        let session = mint(simple_payload(), None, EventScope::Session);
        let run = mint(simple_payload(), None, EventScope::Run { run: 0 });
        let turn = mint(simple_payload(), None, EventScope::Turn { run: 0, turn: 0 });
        let step = mint(
            simple_payload(),
            None,
            EventScope::Step {
                run: 0,
                turn: 0,
                step: 0,
            },
        );
        assert!(has_valid_scope_prefix(&session));
        assert!(has_valid_scope_prefix(&run));
        assert!(has_valid_scope_prefix(&turn));
        assert!(has_valid_scope_prefix(&step));

        let mut missing_run = turn;
        missing_run.run = None;
        assert!(!has_valid_scope_prefix(&missing_run));

        let mut missing_turn = step;
        missing_turn.turn = None;
        assert!(!has_valid_scope_prefix(&missing_turn));
    }

    #[test]
    fn mint_stamps_prefix_complete_scopes() {
        let session = mint(simple_payload(), None, EventScope::Session);
        assert_eq!(
            (session.run, session.turn, session.step),
            (None, None, None)
        );

        let run = mint(simple_payload(), None, EventScope::Run { run: 0 });
        assert_eq!((run.run, run.turn, run.step), (Some(0), None, None));

        let turn = mint(simple_payload(), None, EventScope::Turn { run: 0, turn: 0 });
        assert_eq!((turn.run, turn.turn, turn.step), (Some(0), Some(0), None));

        let step = mint(
            simple_payload(),
            None,
            EventScope::Step {
                run: 2,
                turn: 3,
                step: 4,
            },
        );
        assert_eq!(
            (step.run, step.turn, step.step),
            (Some(2), Some(3), Some(4))
        );
        assert!(verify_hash(&step));
    }

    #[test]
    fn verify_rejects_stale_log_version_even_with_matching_hash() {
        let mut event = mint(simple_payload(), None, EventScope::Session);
        event.version = LOG_VERSION - 1;
        stamp_hash(&mut event);
        assert!(!verify_hash(&event));
    }
}

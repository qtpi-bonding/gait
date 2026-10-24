//! Append/read plumbing for the proto [`Event`] log.
//!
//! This is the proto-format replacement for the old `AnyEvent` JSONL path.
//! For now both paths coexist: the legacy path is unchanged, and the new
//! helpers here let callers mint, append, and read [`Event`] values that
//! satisfy the proto log format and its [`envelope::content_hash`] integrity
//! contract.
//!
//! File layout: one canonical-JSON [`Event`] per line (NDJSON). Blank lines
//! are skipped on read.
//!
//! [`envelope::content_hash`]: crate::envelope::body_json

use std::path::Path;

use crate::envelope::{mint, EventScope, LOG_VERSION};
use crate::gait::log::v1::{event, payload, Event, Payload, RawEvent};

/// Wrap a substantive payload kind into an event body.
pub fn body(kind: payload::Kind) -> event::Body {
    event::Body::Payload(Payload { kind: Some(kind) })
}

/// Wrap an opaque / not-yet-modeled event (e.g. deferred workflow events) as a
/// RawEvent body. `payload` MUST be a JSON object; it is stored verbatim.
pub fn raw(event_type: &str, payload: serde_json::Value) -> event::Body {
    let payload = serde_json::from_value(payload).expect("RawEvent payload must be a JSON object");
    body(payload::Kind::Raw(RawEvent {
        r#type: event_type.to_string(),
        payload: Some(payload),
    }))
}

/// Read all events from a transcript JSONL file (one [`Event`] per line).
/// Skips blank lines.
pub fn read_events(path: &Path) -> anyhow::Result<Vec<Event>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let text = std::fs::read_to_string(path)?;
    let mut events = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let event = serde_json::from_str::<Event>(line)
            .map_err(|error| explain_parse_error(error, events.len() + 1))?;
        if event.version != LOG_VERSION {
            anyhow::bail!(
                "unsupported event log version {} at line {}; expected {}",
                event.version,
                events.len() + 1,
                LOG_VERSION
            );
        }
        if !crate::envelope::has_valid_scope_prefix(&event) {
            anyhow::bail!(
                "invalid event hierarchy coordinates at line {}",
                events.len() + 1
            );
        }
        events.push(event);
    }
    Ok(events)
}

/// A log line that names a removed `Provenance` field (`signature`, `scheme`,
/// `from`, ...) was written by the pre-p256 identity format. Say so, and say
/// what to do, instead of surfacing a bare "unknown field" parse error.
fn explain_parse_error(error: serde_json::Error, line: usize) -> anyhow::Error {
    let text = error.to_string();
    let provenance_field = text.contains("signedMessage") || text.contains("verifiedAuthorId");
    if text.contains("unknown field") && provenance_field {
        return anyhow::anyhow!(
            "session log uses a removed identity format (line {line}: {text}); \
             wipe the gait workspace (pre-MVP, no migration)"
        );
    }
    anyhow::Error::new(error)
}

/// The last event in the file, or `None` if empty/absent.
pub fn last_event(path: &Path) -> anyhow::Result<Option<Event>> {
    Ok(read_events(path)?.pop())
}

/// Append one JSON line to `file` and durably flush it (`sync_all`) before
/// returning. This is the single choke point every persisted log write goes
/// through — `write_to_transcript` and `append_with_id` both call it, so a
/// `write()` that only reached the OS page cache can never be reported back
/// to a caller as durably logged. Without this, `kill -9` is safe (the OS
/// still owns the page cache and flushes independently of process death) but
/// power loss / kernel panic between the write and the OS's own background
/// flush would silently lose the event.
fn append_line_durable(file: &mut std::fs::File, json: &str) -> std::io::Result<()> {
    use std::io::Write;
    writeln!(file, "{}", json)?;
    file.sync_all()
}

/// Write `ev` (already minted/chained) to the JSONL transcript with the
/// shared durability semantics. The single choke point for every persisted
/// log write — see [`append_line_durable`].
fn write_event(path: &Path, ev: Event) -> anyhow::Result<Event> {
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    let json = serde_json::to_string(&ev)?;
    append_line_durable(&mut file, &json)?;
    Ok(ev)
}

/// Mint an [`Event`] for `body` chained after the current tail, write its JSON
/// line to the file, and return the minted event. Shared by [`append`] and
/// [`emit`]; also called directly by the `Recorder` sink in gait-core.
///
/// `scope` is stamped onto the envelope. Zero is never used to mean absent.
///
/// Durability: the underlying write+flush is delegated to
/// [`append_line_durable`] so a successful return guarantees the line is on
/// disk, not just in the OS page cache.
pub fn write_to_transcript(
    path: &Path,
    body: event::Body,
    scope: EventScope,
) -> anyhow::Result<Event> {
    let prev = last_event(path)?;
    let ev = mint(body, prev.as_ref(), scope);
    write_event(path, ev)
}

/// Mint an [`Event`] for `body` chained after the current tail (reads
/// [`last_event`] as `prev`, so `seq`/`parents` chain correctly), append its
/// JSON line to the file, and return the minted event.
///
/// Session-level events carry no run/turn/step coordinates.
pub fn append(path: &Path, body: event::Body) -> anyhow::Result<Event> {
    write_to_transcript(path, body, EventScope::Session)
}

/// Mint an event, pass that exact event to `sign`, attach the returned
/// provenance, and durably append it.
pub fn append_with_signed(
    path: &Path,
    body: event::Body,
    scope: EventScope,
    sign: impl FnOnce(&Event) -> anyhow::Result<crate::gait::log::v1::Provenance>,
) -> anyhow::Result<Event> {
    let prev = last_event(path)?;
    let mut ev = mint(body, prev.as_ref(), scope);
    let prov = sign(&ev)?;
    ev.provenance = vec![prov];
    write_event(path, ev)
}

/// Relays one or more provenances
/// already signed/established by someone else (a direct signature, or a
/// verified authorship assertion produced by a trusted ingress). Attaches
/// the values verbatim without
/// signing or re-verifying. One `Provenance` per trigger body — see
/// `TurnStart::Input`'s `Vec<(event::Body, Option<Provenance>)>` pairing for
/// why this is a list rather than a single value.
pub fn append_with_given_provenances(
    path: &Path,
    body: event::Body,
    provenances: Vec<crate::gait::log::v1::Provenance>,
    scope: EventScope,
) -> anyhow::Result<Event> {
    let prev = last_event(path)?;
    let mut ev = mint(body, prev.as_ref(), scope);
    ev.provenance = provenances;
    write_event(path, ev)
}

/// Append a brain-minted event preserving its `id`, `body`, `turn`, `step`,
/// and `content_hash`, but updating `seq`/`parents` from the log's current
/// tail. Used by the spine in Phase 3: the brain mints a UUID for the event
/// (needed for Replace targeting), then sends the event via `Append`; the
/// spine chains it into the log without changing the ID.
///
/// Durability: the underlying write+flush is delegated to
/// [`append_line_durable`] so a successful return guarantees the line is on
/// disk, not just in the OS page cache.
pub fn append_with_id(path: &Path, incoming: &Event) -> anyhow::Result<Event> {
    use crate::envelope::stamp_hash;
    let prev = last_event(path)?;
    let (seq, parents) = match prev.as_ref() {
        None => (1u64, Vec::new()),
        Some(p) => (p.seq + 1, vec![p.id.clone()]),
    };
    let mut ev = incoming.clone();
    ev.seq = seq;
    ev.parents = parents;
    // Re-stamp the content hash (body, version, turn, step are preserved; seq/parents
    // don't affect the content hash since it covers only the body arm).
    stamp_hash(&mut ev);
    write_event(path, ev)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::verify_hash;
    use crate::gait::log::v1::AssistantText;
    use tempfile::TempDir;

    fn simple_body() -> event::Body {
        body(payload::Kind::AssistantText(AssistantText {
            content: "hi".into(),
        }))
    }

    #[test]
    fn append_chains_seq_and_parents() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("log.jsonl");

        let first = append(&path, simple_body()).expect("append first");
        let second = append(&path, simple_body()).expect("append second");

        let events = read_events(&path).expect("read");
        assert_eq!(events.len(), 2);

        assert_eq!(events[0].seq, 1, "root seq must be 1");
        assert!(events[0].parents.is_empty(), "root must have no parents");
        assert!(verify_hash(&events[0]), "root must verify");

        assert_eq!(events[1].seq, 2, "child seq must be 2");
        assert_eq!(
            events[1].parents,
            vec![first.id.clone()],
            "child parents = [prev.id]"
        );
        assert!(verify_hash(&events[1]), "child must verify");

        // `append` returns the same envelope that was written.
        assert_eq!(second.id, events[1].id);
    }

    #[test]
    fn raw_round_trips() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("log.jsonl");

        let written = append(
            &path,
            raw(
                "node_start",
                serde_json::json!({"workflow_id": "w1", "node_id": "n1"}),
            ),
        )
        .expect("append raw");

        let events = read_events(&path).expect("read");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].id, written.id);

        match &events[0].body {
            Some(event::Body::Payload(p)) => match &p.kind {
                Some(payload::Kind::Raw(r)) => {
                    assert_eq!(r.r#type, "node_start");
                    let s = r.payload.as_ref().expect("struct present");
                    let fields = &s.fields;
                    assert!(
                        fields.contains_key("workflow_id"),
                        "expected `workflow_id` in struct fields, got {fields:?}"
                    );
                }
                other => panic!("expected Raw kind, got {other:?}"),
            },
            other => panic!("expected Payload body, got {other:?}"),
        }

        assert!(verify_hash(&events[0]), "raw event must still verify");
    }

    #[test]
    fn read_missing_file_is_empty() {
        let dir = TempDir::new().expect("tempdir");
        let path = dir.path().join("does-not-exist.jsonl");

        let events = read_events(&path).expect("read missing");
        assert!(events.is_empty(), "missing file must yield empty vec");
    }

    #[test]
    fn read_events_parses_modern_log_without_rewrite() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("log.jsonl");
        let body = body(crate::gait::log::v1::payload::Kind::Cwd(
            crate::gait::log::v1::Cwd {
                path: "/tmp/work".into(),
            },
        ));
        append(&path, body).unwrap();
        let events = read_events(&path).unwrap();
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn write_to_transcript_threads_turn_and_step() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("log.jsonl");
        let ev = write_to_transcript(
            &path,
            simple_body(),
            EventScope::Step {
                run: 2,
                turn: 4,
                step: 7,
            },
        )
        .unwrap();
        assert_eq!(ev.run, Some(2));
        assert_eq!(ev.turn, Some(4));
        assert_eq!(ev.step, Some(7));
    }

    #[test]
    fn append_with_id_round_trips_through_shared_write_helper() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("log.jsonl");
        let seeded = append(&path, simple_body()).unwrap();

        let incoming = crate::envelope::mint(simple_body(), None, EventScope::Session);
        let written = append_with_id(&path, &incoming).unwrap();

        // append_with_id preserves the incoming id but re-chains seq/parents.
        assert_eq!(written.id, incoming.id);
        assert_eq!(written.seq, seeded.seq + 1);
        assert_eq!(written.parents, vec![seeded.id.clone()]);

        let events = read_events(&path).unwrap();
        assert_eq!(events.len(), 2, "both writes must be durably readable back");
        assert_eq!(events[1].id, written.id);
    }

    #[test]
    fn append_with_signed_passes_the_minted_event_to_the_signer() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log.jsonl");
        let expected = crate::gait::log::v1::Provenance {
            context: "signed".into(),
            ..Default::default()
        };
        let expected_copy = expected.clone();
        let ev = append_with_signed(&path, simple_body(), EventScope::Session, |minted| {
            assert!(!minted.id.is_empty());
            assert!(minted.ts > 0);
            assert!(!minted.content_hash.is_empty());
            Ok(expected_copy)
        }).unwrap();
        assert_eq!(ev.provenance, vec![expected]);
    }

    #[test]
    fn old_provenance_json_with_removed_fields_fails_to_parse() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log.jsonl");
        let event = append(&path, simple_body()).unwrap();
        let json = serde_json::to_value(&event).unwrap();
        let removed_fields = [
            ("signature", serde_json::json!("b2xk")),
            ("scheme", serde_json::json!(0)),
            ("verificationOutcome", serde_json::json!(0)),
            ("delegationCert", serde_json::json!({})),
            ("from", serde_json::json!("b2xk")),
        ];
        for (field, value) in removed_fields {
            let mut old = json.clone();
            let mut provenance = serde_json::Map::new();
            provenance.insert(field.to_string(), value);
            old["provenance"] = serde_json::Value::Array(vec![serde_json::Value::Object(provenance)]);
            let error = serde_json::from_value::<Event>(old).expect_err(field);
            assert!(!error.to_string().is_empty());
        }

        let mut current = json;
        let mut provenance = serde_json::Map::new();
        provenance.insert("context".into(), serde_json::json!("room"));
        current["provenance"] = serde_json::Value::Array(vec![serde_json::Value::Object(provenance)]);
        serde_json::from_value::<Event>(current).expect("context remains a valid provenance field");
    }

    #[test]
    fn read_events_explains_a_log_with_a_removed_identity_format() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log.jsonl");
        let event = append(&path, simple_body()).unwrap();
        let mut json = serde_json::to_value(&event).unwrap();
        json["provenance"] = serde_json::json!([{ "signature": "b2xk", "scheme": 1 }]);
        std::fs::write(&path, format!("{}\n", json)).unwrap();

        let error = read_events(&path).expect_err("a removed provenance field must not parse");
        let message = error.to_string();
        assert!(
            message.contains("session log uses a removed identity format")
                && message.contains("wipe the gait workspace"),
            "the error must be actionable, got: {message}"
        );

        // An unrelated parse failure keeps its own error.
        std::fs::write(&path, "not json\n").unwrap();
        let message = read_events(&path).unwrap_err().to_string();
        assert!(!message.contains("removed identity format"), "got: {message}");
    }

    #[test]
    fn append_with_given_provenances_preserves_every_entry_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log.jsonl");
        let body = crate::gait::log::v1::event::Body::Payload(crate::gait::log::v1::Payload {
            kind: Some(crate::gait::log::v1::payload::Kind::UserText(
                crate::gait::log::v1::UserText {
                    content: "hi".into(),
                    ..Default::default()
                },
            )),
        });
        let p1 = crate::gait::log::v1::Provenance {
            context: "room-a".into(),
            ..Default::default()
        };
        let p2 = crate::gait::log::v1::Provenance {
            context: "room-b".into(),
            ..Default::default()
        };
        let event = append_with_given_provenances(
            &path,
            body,
            vec![p1.clone(), p2.clone()],
            EventScope::Session,
        )
        .unwrap();
        assert_eq!(event.provenance, vec![p1, p2]);
    }

    #[test]
    fn read_events_rejects_stale_log_versions() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("log.jsonl");
        let mut event = crate::envelope::mint(simple_body(), None, EventScope::Session);
        event.version = LOG_VERSION - 1;
        crate::envelope::stamp_hash(&mut event);
        std::fs::write(
            &path,
            format!("{}\n", serde_json::to_string(&event).unwrap()),
        )
        .unwrap();

        let error = read_events(&path).unwrap_err().to_string();
        assert!(error.contains("unsupported event log version"), "{error}");
    }
}

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
//! [`envelope::content_hash`]: crate::proto::envelope::body_json

use std::io::Write;
use std::path::Path;

use crate::proto::envelope::mint;
use crate::proto::gait::log::v1::{event, payload, Event, Payload, RawEvent};

/// Wrap a substantive payload kind into an event body.
pub fn body(kind: payload::Kind) -> event::Body {
    event::Body::Payload(Payload { kind: Some(kind) })
}

/// Wrap an opaque / not-yet-modeled event (e.g. deferred workflow events) as a
/// RawEvent body. `payload` MUST be a JSON object; it is stored verbatim.
pub fn raw(event_type: &str, payload: serde_json::Value) -> event::Body {
    let payload = serde_json::from_value(payload)
        .expect("RawEvent payload must be a JSON object");
    body(payload::Kind::Raw(RawEvent {
        r#type: event_type.to_string(),
        payload: Some(payload),
    }))
}

/// Rewrite pre-rename JSON keys so old logs (`"manifest"`, `"compaction"`) decode
/// to the new proto names (`"agent"`, `"summary"`). Also strips `outputSchemaRetries`
/// which was dropped from the Agent envelope (M4). Applied before every Event
/// `serde_json::from_str` call. No-ops on modern logs (fast string check first).
pub(crate) fn compat_rewrite_keys(line: &str) -> std::borrow::Cow<'_, str> {
    if !line.contains("\"manifest\":") && !line.contains("\"compaction\":") {
        return std::borrow::Cow::Borrowed(line);
    }
    let Ok(mut v) = serde_json::from_str::<serde_json::Value>(line) else {
        return std::borrow::Cow::Borrowed(line);
    };
    if let Some(payload) = v.get_mut("payload").and_then(|p| p.as_object_mut()) {
        if let Some(mut m) = payload.remove("manifest") {
            if let Some(o) = m.as_object_mut() {
                o.remove("outputSchemaRetries");
            }
            payload.insert("agent".to_string(), m);
        }
        if let Some(c) = payload.remove("compaction") {
            payload.insert("summary".to_string(), c);
        }
    }
    std::borrow::Cow::Owned(serde_json::to_string(&v).unwrap_or_else(|_| line.to_string()))
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
        events.push(serde_json::from_str::<Event>(&compat_rewrite_keys(line))?);
    }
    Ok(events)
}

/// The last event in the file, or `None` if empty/absent.
pub fn last_event(path: &Path) -> anyhow::Result<Option<Event>> {
    Ok(read_events(path)?.pop())
}

/// Mint an [`Event`] for `body` chained after the current tail, write its JSON
/// line to the file, and return the minted event. Shared by [`append`] and
/// [`emit`]; also called directly by [`crate::proto::Emitter`].
pub(crate) fn write_to_transcript(path: &Path, body: event::Body) -> anyhow::Result<Event> {
    let prev = last_event(path)?;
    let ev = mint(body, prev.as_ref());
    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    let json = serde_json::to_string(&ev)?;
    writeln!(file, "{}", json)?;
    Ok(ev)
}

/// Mint an [`Event`] for `body` chained after the current tail (reads
/// [`last_event`] as `prev`, so `seq`/`parents` chain correctly), append its
/// JSON line to the file, and return the minted event.
pub fn append(path: &Path, body: event::Body) -> anyhow::Result<Event> {
    write_to_transcript(path, body)
}

/// Like [`append`], but also echoes the JSON line to stdout (NDJSON stream for
/// the REPL / external runners), matching the old `emit_event` behavior.
pub fn emit(path: &Path, body: event::Body) -> anyhow::Result<Event> {
    let ev = write_to_transcript(path, body)?;
    println!("{}", serde_json::to_string(&ev)?);
    Ok(ev)
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::envelope::verify_hash;
    use crate::proto::gait::log::v1::AssistantText;
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
    fn pre_rename_log_decodes_via_compat_rewrite() {
        use crate::proto::gait::log::v1::{event, payload};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log.jsonl");
        // Two hand-written PRE-RENAME lines, exactly as old sessions persisted them:
        // a "manifest" payload (old Manifest) and a "compaction" payload (old Compaction).
        let old = concat!(
            r#"{"version":1,"id":"11111111-1111-4111-8111-111111111111","seq":1,"ts":1,"contentHash":"x","parents":[],"payload":{"manifest":{"model":"anthropic/x","instructions":"be terse","outputSchemaRetries":7}}}"#,
            "\n",
            r#"{"version":1,"id":"22222222-2222-4222-8222-222222222222","seq":2,"ts":2,"contentHash":"y","parents":["11111111-1111-4111-8111-111111111111"],"payload":{"compaction":{"summary":"did stuff","turnsCovered":3}}}"#,
            "\n",
        );
        std::fs::write(&path, old).unwrap();

        let events = read_events(&path).expect("pre-rename log must still decode");
        assert_eq!(events.len(), 2, "both legacy events decode");

        let k0 = match &events[0].body { Some(event::Body::Payload(p)) => p.kind.clone(), _ => None };
        assert!(matches!(k0, Some(payload::Kind::Agent(_))), "old \"manifest\" -> Kind::Agent");
        let k1 = match &events[1].body { Some(event::Body::Payload(p)) => p.kind.clone(), _ => None };
        assert!(matches!(k1, Some(payload::Kind::Summary(_))), "old \"compaction\" -> Kind::Summary");

        // outputSchemaRetries (dropped from the envelope, M4) is silently ignored, not an error.
        if let Some(payload::Kind::Agent(a)) = k0 { assert_eq!(a.model, "anthropic/x"); }
    }

}

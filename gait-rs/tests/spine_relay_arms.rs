//! Tasks 5 & 6: Relay arms — StreamForward / Diagnostic / Notify characterization.
//!
//! Pins the spine's signal-relay arms (everything except `Append` and `Terminated`,
//! covered by `spine_persistence.rs` and `spine_terminal_failure.rs`) so a later
//! refactor (TurnContext / SessionJournal / DiagnosticSink / BrainProcess) cannot
//! silently regress:
//!
//! - **StreamForward** → `sink.item(&StreamItem)` (live stream → embedder sink).
//!   The brain wraps each `StreamItem` it would have sent into a `StreamForward`
//!   signal; the spine unwraps and calls `sink.item`. Per-phase enrichment
//!   (`enrich_permissions_preview`) happens inline — the StreamItem we observe
//!   on the sink has already been through that step.
//!
//! - **Diagnostic** (per-phase ping) → written to `diagnostic.jsonl`, but ONLY
//!   when `GAIT_DEBUG` is set to a non-empty value
//!   (`src/turn/spine.rs:619-621` gates on
//!   `crate::diagnostic::debug_enabled()`). The terminal-failure record
//!   (Task 7 / `src/turn/spine.rs:528-531`) is a separate code path and is
//!   NOT gated — see `spine_terminal_failure.rs` for that. This test pins the
//!   current gating behavior: per-phase Diagnostic pings do NOT land in
//!   `diagnostic.jsonl` unless `GAIT_DEBUG` is set. A refactor that drops the
//!   gate (or adds one to the terminal record) will fail this test.
//!
//! - **Notify** → spine is sole gait-dir writer. It acquires the target
//!   parent's session lock, appends an `AgentUpdate` event to the parent's
//!   `log.jsonl`, and best-effort commits the trace.
//!   (`src/turn/spine.rs:567-616`)
//!
//! - **Notify failure policy**: when the target session cannot be locked
//!   (`spine.rs:597` swallows the error and just logs to stderr) the child
//!   turn still completes. This is the latent-bug behavior flagged in the
//!   architecture review — today, a missing parent silently no-ops. Pin it
//!   deliberately so a future refactor that promotes it to a hard error
//!   updates this test on purpose.
mod common;
use common::*;

use gait_proto::gait::log::v1::{stream_item, StreamItem, TextDelta};
use gait_proto::gait::sandbox::v1::{signal, Diagnostic, Notify, Signal, StreamForward};

/// Build a `StreamItem` carrying a text delta — the shape the brain uses
/// for streaming assistant output (see `src/proto/emitter.rs:217`).
/// There is no public `stream_item_text` constructor today, so we use the
/// proto enum directly. (Adapting to whatever helper lands later is a
/// one-line change.)
fn stream_item_text(content: &str) -> StreamItem {
    StreamItem {
        item: Some(stream_item::Item::Delta(TextDelta {
            content: content.to_string(),
        })),
    }
}

#[tokio::test]
async fn stream_forward_reaches_sink_items() {
    let tmp = tempfile::tempdir().unwrap();
    let sf = Signal {
        corr_id: 0,
        body: Some(signal::Body::StreamForward(StreamForward {
            item: Some(stream_item_text("hello")),
        })),
    };
    let (sink, _calls) = run_one(tmp.path(), "r1", &[sf, terminated_ok("done")]).await;

    assert_eq!(
        sink.items.len(),
        1,
        "StreamForward must reach sink.item() exactly once; got items={:?}",
        sink.items
    );
    // The text content must survive the round-trip — the spine doesn't
    // inspect Deltas (only Events go through enrich_permissions_preview,
    // see spine.rs:501-521), so the Delta text lands on the sink verbatim.
    match &sink.items[0].item {
        Some(stream_item::Item::Delta(d)) => assert_eq!(d.content, "hello"),
        other => panic!("expected Delta(\"hello\"), got {other:?}"),
    }
    // And of course the turn still terminates cleanly.
    assert_eq!(
        sink.results.len(),
        1,
        "StreamForward must not break the turn"
    );
}

#[tokio::test]
async fn diagnostic_writes_diagnostic_jsonl_only_when_gait_debug_set() {
    // PIN current behavior: per-phase Diagnostic pings are GAIT_DEBUG-gated
    // (src/turn/spine.rs:619-621). We DO NOT set GAIT_DEBUG in this test
    // process — `debug_enabled()` returns false → diagnostic.jsonl must NOT
    // exist for this session.
    //
    // The opposite case (GAIT_DEBUG=1 → line written) is exercised by the
    // diagnostic module's own tests; the integration gate lives here.
    let tmp = tempfile::tempdir().unwrap();
    let d = Signal {
        corr_id: 0,
        body: Some(signal::Body::Diagnostic(Diagnostic {
            record_json: "{\"phase\":\"test\"}".into(),
            phase: "test".into(),
            elapsed_ms: 0,
        })),
    };
    let _ = run_one(tmp.path(), "r2", &[d, terminated_ok("done")]).await;

    let path = tmp.path().join("sessions/r2/diagnostic.jsonl");
    assert!(
        !path.exists(),
        "diagnostic.jsonl must NOT exist for per-phase Diagnostic pings when \
         GAIT_DEBUG is unset (spine.rs:619-621 gate); if you intended to \
         change the gating, update this test deliberately."
    );
}

#[tokio::test]
async fn notify_writes_agent_update_to_parent_session() {
    // Pre-create the parent session by driving a successful turn against it.
    // That gives us a real session dir + log.jsonl (sole writer = spine) for
    // the Notify target to append into.
    let tmp = tempfile::tempdir().unwrap();
    let _ = run_one(tmp.path(), "parent", &[terminated_ok("seed")]).await;

    // Now a child turn that notifies the parent on the way out.
    let n = Signal {
        corr_id: 0,
        body: Some(signal::Body::Notify(Notify {
            target_session: "parent".into(),
            from_session: "child".into(),
            status: "done".into(),
            summary: "child finished".into(),
        })),
    };
    let _ = run_one(tmp.path(), "child", &[n, terminated_ok("done")]).await;

    // The parent log.jsonl must now carry the AgentUpdate event. The AgentUpdate
    // payload carries the child's `from_session` as `sessionId` (pbjson camelCase)
    // and the child's `summary` as `summary`. Status → enum int (1 = Done).
    let parent_log = std::fs::read_to_string(tmp.path().join("sessions/parent/log.jsonl"))
        .expect("parent log.jsonl must exist after the seed turn");
    assert!(
        parent_log.contains("agentUpdate"),
        "Notify must append an AgentUpdate event to the parent's log.jsonl; got:\n{parent_log}"
    );
    assert!(
        parent_log.contains("\"sessionId\":\"child\""),
        "AgentUpdate must carry the child's from_session as sessionId; got:\n{parent_log}"
    );
    assert!(
        parent_log.contains("child finished"),
        "AgentUpdate summary must reach the parent log verbatim; got:\n{parent_log}"
    );
}

#[tokio::test]
async fn notify_to_missing_parent_does_not_fail_the_turn() {
    // PIN current behavior (latent bug flagged in architecture review):
    // spine.rs:597 swallows the lock-acquire error and just logs to stderr.
    // The child turn therefore completes with `Done` even though the notify
    // target was nonexistent. If a future refactor promotes this to a hard
    // error, THIS test must be updated deliberately — that is the point.
    let tmp = tempfile::tempdir().unwrap();
    let n = Signal {
        corr_id: 0,
        body: Some(signal::Body::Notify(Notify {
            target_session: "nonexistent".into(),
            from_session: "child2".into(),
            status: "done".into(),
            summary: "x".into(),
        })),
    };
    let (sink, _calls) = run_one(tmp.path(), "child2", &[n, terminated_ok("done")]).await;
    assert_eq!(
        sink.results.len(),
        1,
        "child turn must complete with one Done result despite missing notify target; \
         got results={:?}",
        sink.results
    );
    // And no Error diagnostic should fire — the swallow is silent.
    assert!(
        !sink
            .diagnostics
            .iter()
            .any(|(d, _)| matches!(d, gait_core::embed::Diag::Error)),
        "missing-parent notify must be swallowed (no Diag::Error); got diagnostics={:?}",
        sink.diagnostics
    );
}

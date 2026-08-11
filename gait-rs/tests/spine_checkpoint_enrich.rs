//! Task 4 (+ Task 5's follow-up): checkpoint enrichment driven through the
//! real spine relay loop (`run_one`), not just the isolated unit tests in
//! `src/turn/spine/checkpoint_enrich.rs`.
//!
//! Pins:
//! 1. A ToolResult for a checkpoint-gated tool (`shell`) gets `checkpoint:
//!    Some(_)` stamped in the persisted log.
//! 2. A ToolResult for a non-gated tool (`read`) does NOT get a checkpoint.
//! 3. A `Replace`-wrapped ToolResult (the shape `apply_output_filter` produces
//!    when a `parse` arg rewrites the tool's output) still gets `checkpoint:
//!    Some(_)` stamped on the replacement — proving the enrichment re-derives
//!    the SHA independently of whatever the brain-side rewrite already set.

mod common;
use common::*;

use gait_core::proto::events;
use gait_proto::envelope;
use gait_proto::gait::sandbox::v1::{signal, Append, Signal};

fn tool_result_checkpoints(log: &str, call_id: &str) -> Vec<Option<String>> {
    use gait_core::proto::gait::log::v1::{event, payload, Event};

    log.lines()
        .filter_map(|line| serde_json::from_str::<Event>(line).ok())
        .filter_map(|event| match event.body? {
            event::Body::Payload(payload) => payload.kind,
            event::Body::Replace(replace) => replace.replacement.and_then(|payload| payload.kind),
            event::Body::Redact(_) => None,
        })
        .filter_map(|kind| match kind {
            payload::Kind::ToolResult(result) if result.call_id == call_id => {
                Some(result.checkpoint)
            }
            _ => None,
        })
        .collect()
}

fn append_tool_call(call_id: &str, name: &str) -> Signal {
    let body = events::tool_call(
        call_id.to_string(),
        name.to_string(),
        &serde_json::json!({}),
    );
    let evt = envelope::mint(
        body,
        None,
        envelope::EventScope::Step {
            run: 0,
            turn: 0,
            step: 0,
        },
    );
    Signal {
        corr_id: 0,
        body: Some(signal::Body::Append(Append { event: Some(evt) })),
    }
}

fn append_tool_result(call_id: &str, output: &str) -> Signal {
    let body = events::tool_result(call_id.to_string(), output.to_string(), None, None);
    let evt = envelope::mint(
        body,
        None,
        envelope::EventScope::Step {
            run: 0,
            turn: 0,
            step: 0,
        },
    );
    Signal {
        corr_id: 0,
        body: Some(signal::Body::Append(Append { event: Some(evt) })),
    }
}

/// Mints the *original* ToolResult event (so its `id` is known), then wraps
/// a second ToolResult (the filtered output) in a `Replace` targeting that
/// id — mirroring `apply_output_filter`'s `parse`-arg rewrite.
fn append_tool_result_then_replace(
    call_id: &str,
    output: &str,
    filtered: &str,
) -> (Signal, Signal) {
    let orig_body = events::tool_result(call_id.to_string(), output.to_string(), None, None);
    let orig_evt = envelope::mint(
        orig_body,
        None,
        envelope::EventScope::Step {
            run: 0,
            turn: 0,
            step: 0,
        },
    );
    let orig_id = orig_evt.id.clone();
    let orig_signal = Signal {
        corr_id: 0,
        body: Some(signal::Body::Append(Append {
            event: Some(orig_evt),
        })),
    };

    let replacement_kind =
        match events::tool_result(call_id.to_string(), filtered.to_string(), None, None) {
            gait_proto::gait::log::v1::event::Body::Payload(p) => p,
            _ => unreachable!("tool_result always produces a Payload body"),
        };
    let replace_body = events::replace(orig_id, replacement_kind, None);
    let replace_evt = envelope::mint(
        replace_body,
        None,
        envelope::EventScope::Step {
            run: 0,
            turn: 0,
            step: 0,
        },
    );
    let replace_signal = Signal {
        corr_id: 0,
        body: Some(signal::Body::Append(Append {
            event: Some(replace_evt),
        })),
    };
    (orig_signal, replace_signal)
}

#[tokio::test]
async fn shell_tool_result_gets_checkpoint_stamped() {
    let tmp = tempfile::tempdir().unwrap();
    let _ = run_one(
        tmp.path(),
        "ce1",
        &[
            append_tool_call("call-shell-1", "shell"),
            append_tool_result("call-shell-1", "ok"),
            terminated_ok("done"),
        ],
    )
    .await;

    let log = std::fs::read_to_string(gait_core::session::live::log_path(tmp.path(), "ce1"))
        .expect("log.jsonl must exist after a completed turn");
    let checkpoints = tool_result_checkpoints(&log, "call-shell-1");
    assert!(checkpoints.iter().any(Option::is_some), "{log}");
}

#[tokio::test]
async fn read_tool_result_is_not_stamped() {
    let tmp = tempfile::tempdir().unwrap();
    let _ = run_one(
        tmp.path(),
        "ce2",
        &[
            append_tool_call("call-read-1", "read"),
            append_tool_result("call-read-1", "contents"),
            terminated_ok("done"),
        ],
    )
    .await;

    let log = std::fs::read_to_string(gait_core::session::live::log_path(tmp.path(), "ce2"))
        .expect("log.jsonl must exist after a completed turn");
    let checkpoints = tool_result_checkpoints(&log, "call-read-1");
    assert_eq!(checkpoints, vec![None], "{log}");
}

#[tokio::test]
async fn replace_wrapped_shell_tool_result_gets_checkpoint_stamped() {
    let tmp = tempfile::tempdir().unwrap();
    let (orig, replace) =
        append_tool_result_then_replace("call-shell-2", "raw output", "parsed output");
    let _ = run_one(
        tmp.path(),
        "ce3",
        &[
            append_tool_call("call-shell-2", "shell"),
            orig,
            replace,
            terminated_ok("done"),
        ],
    )
    .await;

    let log = std::fs::read_to_string(gait_core::session::live::log_path(tmp.path(), "ce3"))
        .expect("log.jsonl must exist after a completed turn");
    // Both the original ToolResult event and the Replace's replacement should
    // have been through enrich_checkpoint (it runs on every Append,
    // unconditionally) — so at least one "checkpoint" field must be present,
    // and specifically the replace's replacement (last occurrence) must carry it.
    let checkpoints = tool_result_checkpoints(&log, "call-shell-2");
    assert_eq!(checkpoints.len(), 2, "{log}");
    assert!(checkpoints.iter().all(Option::is_some), "{log}");
    assert!(
        log.contains("parsed output"),
        "the replacement's filtered output must reach the log: {log}"
    );
}

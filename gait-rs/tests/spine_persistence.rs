//! Task 3: Persistence ordering / sole-writer characterization tests.
//!
//! Pins the spine's persistence invariants so a later refactor (TurnContext /
//! SessionJournal / BrainProcess) cannot silently regress:
//!
//! 1. `SessionStart` lands in `log.jsonl` as the first event of a session.
//! 2. Append events appended via `Signal::Append` arrive in `log.jsonl` in
//!    receive order — the spine is the sole writer (no interleaving).
//! 3. The mock `Repos` records the precise call sequence: session-worktree
//!    creation and checkpoint-init precede the per-turn checkpoint + commit.
//!
//! These assertions ride on Task 2's harness (no real git), so they're
//! deterministic and fast.

mod common;
use common::*;

use gait_proto::envelope;
use gait_proto::gait::sandbox::v1::{signal, Append, Signal};

use gait_core::proto::events;

/// Build one `Signal::Append` carrying an AssistantText event for `text`.
/// The brain mints the event via `envelope::mint(body, None, 0, 0)` before
/// wrapping it in `Append` (see `src/turn/brain.rs:130-140`); the spine then
/// re-stamps seq/parents on append (see `src/turn/spine.rs:496`).
fn append_assistant(text: &str) -> Signal {
    let body = events::assistant_text(text.to_string());
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

/// Build one `Signal::Append` carrying a ToolCall event.
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

/// End-to-end orphan-synthesis: a ToolCall with no matching ToolResult,
/// followed by Terminated, must land a synthesized ToolResult in the log
/// before the turn commits — driven through the real spine relay loop
/// (`run_one`), not the isolated unit test in `src/turn/spine.rs`.
#[tokio::test]
async fn orphan_tool_call_gets_synthesized_result_before_commit() {
    let tmp = tempfile::tempdir().unwrap();
    let _ = run_one(
        tmp.path(),
        "p3",
        &[
            append_tool_call("call-orphan-1", "read"),
            terminated_ok("done"),
        ],
    )
    .await;

    let log = std::fs::read_to_string(gait_core::session::live::log_path(tmp.path(), "p3"))
        .expect("log.jsonl must exist after a completed turn");

    let call_at = log
        .find("call-orphan-1")
        .expect("ToolCall for call-orphan-1 must appear in log");
    // The synthesized ToolResult reuses the same call_id, so pin ordering by
    // finding a second occurrence after the ToolCall.
    let result_at = log[call_at + "call-orphan-1".len()..].find("call-orphan-1");
    assert!(
        result_at.is_some(),
        "a synthesized ToolResult for call-orphan-1 must appear after its ToolCall: {log}"
    );
    // The synthesized result body carries a "toolResult" payload discriminant
    // (pbjson camelCase), distinguishing it from the original "toolCall".
    assert!(
        log.contains("toolResult"),
        "log must contain a synthesized toolResult payload: {log}"
    );
}

#[tokio::test]
async fn appends_persist_in_order_with_session_start_first() {
    // Bare tempdir — no git init. The mock Repos fakes the worktree /
    // checkpoint / commit so the only real on-disk artifact under test is
    // `sessions/<id>/log.jsonl`.
    let tmp = tempfile::tempdir().unwrap();
    let _ = run_one(
        tmp.path(),
        "p1",
        &[
            append_assistant("first"),
            append_assistant("second"),
            terminated_ok("done"),
        ],
    )
    .await;

    // log.jsonl is real FS (not behind Repos) — assert real content + ordering.
    let log = std::fs::read_to_string(gait_core::session::live::log_path(tmp.path(), "p1"))
        .expect("log.jsonl must exist after a completed turn");
    let lines: Vec<&str> = log.lines().collect();
    assert!(
        !lines.is_empty(),
        "log.jsonl must contain at least the SessionStart event"
    );
    assert!(
        lines[0].contains("sessionStart"),
        "first line must be SessionStart (pbjson camelCase): {}",
        lines[0]
    );

    // Substring search across the whole file is sufficient to pin order:
    // every assistant_text writes `"content":"<text>"`, so first appearing
    // before second proves the spine appended them in receive order.
    let first_at = log
        .find("first")
        .expect("'first' AssistantText must appear in log");
    let second_at = log
        .find("second")
        .expect("'second' AssistantText must appear in log");
    assert!(
        first_at < second_at,
        "Append order must be preserved (first at {first_at}, second at {second_at})"
    );
}

#[tokio::test]
async fn persistence_call_sequence_is_pinned() {
    let tmp = tempfile::tempdir().unwrap();
    let (_sink, calls) = run_one(
        tmp.path(),
        "p2",
        &[append_assistant("done"), terminated_ok("done")],
    )
    .await;
    let seq = calls.snapshot();

    // The spine's git interactions, characterized precisely via the mock
    // (no real git): every persistence call lands here in a deterministic
    // order.
    let worktree = seq
        .iter()
        .position(|c| c.starts_with("create_session_worktree:p2"));
    let init = seq.iter().position(|c| c == "checkpoint.init:p2");
    let baseline = seq.iter().position(|c| c == "checkpoint:p2:run 0 baseline");
    let final_checkpoint = seq.iter().position(|c| c == "checkpoint:p2:run 0 final");
    let commit = seq.iter().position(|c| c.starts_with("commit_session:p2:"));

    assert!(
        worktree.is_some()
            && init.is_some()
            && baseline.is_some()
            && final_checkpoint.is_some()
            && commit.is_some(),
        "expected all five persistence calls; got: {seq:?}"
    );

    // worktree + checkpoint init happen during startup (spine-owned writes,
    // before brain spawn — see `src/turn/spine.rs:266-274`). The per-turn
    // final checkpoint + commit_session happen after the brain exits, in the
    // post-turn finalize block.
    assert!(
        worktree.unwrap() < init.unwrap()
            && init.unwrap() < baseline.unwrap()
            && baseline.unwrap() < final_checkpoint.unwrap()
            && final_checkpoint.unwrap() < commit.unwrap(),
        "persistence boundaries must stay ordered: {seq:?}"
    );
}

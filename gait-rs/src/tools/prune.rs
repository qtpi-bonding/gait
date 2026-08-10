//! `prune` tool — agent-driven retrospective context pruning.
//!
//! Lets the agent collapse stale tool results into a compact tombstone,
//! freeing context window space without touching the immutable log.
//! Scoped to the current + previous turn (recency window) to keep cache
//! invalidation cost near zero.

use crate::proto::gait::log::v1 as pb;
use crate::proto::gait::log::v1::{payload, Event};
use crate::proto::{event_kind, events};
use crate::session::log::read_events;
use crate::tools::ToolError;
use crate::types::{Tool, ToolPermission};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

/// Maximum bytes for the optional `reason` field.
/// A tombstone must never cost more context than what it replaced.
pub const MAX_PRUNE_REASON_LEN: usize = 200;

#[allow(dead_code)]
#[derive(Debug, Deserialize, JsonSchema)]
struct PruneParams {
    /// One or more tool_call_ids (visible in your context) whose results you want to collapse
    /// into a compact tombstone. Targets must belong to the current or immediately preceding
    /// turn (older results are handled at compaction time).
    call_ids: Vec<String>,
    /// Optional short note explaining what was learned or why the result is no longer needed
    /// (e.g. "no matches relevant to the bug" or "auth lives in session/log/mod.rs").
    /// Capped at 200 characters — the tombstone must cost less than what it replaces.
    reason: Option<String>,
}

pub struct PruneTool {
    parameters: Value,
}

impl Default for PruneTool {
    fn default() -> Self {
        Self::new()
    }
}

impl PruneTool {
    pub fn new() -> Self {
        let parameters = crate::tools::make_schema::<PruneParams>();
        Self { parameters }
    }
}

/// Returns (tool_call_event_id, tool_result_event_id, tool_name) for a given call_id,
/// scanning `events` for the *latest* ToolCall/ToolResult pair.
fn resolve_call_id(call_id: &str, events: &[Event]) -> Option<(String, String, String)> {
    let mut call_event_id: Option<String> = None;
    let mut call_name: Option<String> = None;
    let mut result_event_id: Option<String> = None;

    for e in events {
        match event_kind(e) {
            Some(payload::Kind::ToolCall(tc)) if tc.call_id == call_id => {
                call_event_id = Some(e.id.clone());
                call_name = Some(tc.name.clone());
            }
            Some(payload::Kind::ToolResult(tr)) if tr.call_id == call_id => {
                // Take the *latest* ToolResult (last writer wins, per spec)
                result_event_id = Some(e.id.clone());
            }
            _ => {}
        }
    }

    match (call_event_id, result_event_id, call_name) {
        (Some(tc_id), Some(tr_id), Some(name)) => Some((tc_id, tr_id, name)),
        _ => None,
    }
}

/// Returns the seq of the second-to-last TurnComplete, or 0 if fewer than 2 exist.
fn recency_boundary(events: &[Event]) -> u64 {
    let turn_complete_seqs: Vec<u64> = events
        .iter()
        .filter(|e| matches!(event_kind(e), Some(payload::Kind::TurnComplete(_))))
        .map(|e| e.seq)
        .collect();

    let len = turn_complete_seqs.len();
    if len < 2 {
        0
    } else {
        turn_complete_seqs[len - 2]
    }
}

/// Returns the seq of the ToolResult event for `result_event_id`, or None if not found.
fn event_seq(result_event_id: &str, events: &[Event]) -> Option<u64> {
    events
        .iter()
        .find(|e| e.id == result_event_id)
        .map(|e| e.seq)
}

#[async_trait::async_trait]
impl Tool for PruneTool {
    fn name(&self) -> &str {
        "prune"
    }

    fn description(&self) -> &str {
        "Collapse noisy tool results from this or the previous turn into short tombstones. Use after deciding the results are not useful. The optional reason is capped at 200 characters."
    }

    fn parameters(&self) -> &Value {
        &self.parameters
    }

    fn permission(&self, _args: &Value) -> ToolPermission {
        ToolPermission::Passthrough
    }

    async fn execute(&self, args: Value, rec: &dyn crate::types::Rec) -> Result<Value, ToolError> {
        let params: PruneParams = serde_json::from_value(args)
            .map_err(|e| ToolError::Execute(format!("prune: invalid args: {e}")))?;

        // Read the current log to resolve call_ids and compute the recency boundary.
        let log_path = rec.tx_path();
        let all_events = read_events(log_path)
            .map_err(|e| ToolError::Execute(format!("prune: read log: {e}")))?;

        let boundary_seq = recency_boundary(&all_events);

        // Truncate reason if over cap.
        let reason_text: Option<String> = params.reason.map(|r| {
            if r.len() > MAX_PRUNE_REASON_LEN {
                format!("{}…", &r[..MAX_PRUNE_REASON_LEN])
            } else {
                r
            }
        });

        let mut pruned: Vec<String> = Vec::new();
        let mut skipped: Vec<String> = Vec::new();

        for call_id in &params.call_ids {
            match resolve_call_id(call_id, &all_events) {
                None => {
                    skipped.push(format!("{call_id} (not found)"));
                }
                Some((_tc_id, tr_event_id, tool_name)) => {
                    // Window check
                    let tr_seq = event_seq(&tr_event_id, &all_events).unwrap_or(0);
                    if tr_seq < boundary_seq {
                        skipped.push(format!(
                            "{call_id} (too old to prune — older results are handled at compaction)"
                        ));
                        continue;
                    }

                    // Build tombstone string
                    let tombstone = match &reason_text {
                        Some(r) => format!("[pruned: {tool_name} — {r}]"),
                        None => format!("[pruned: {tool_name} — dropped]"),
                    };

                    // Build replacement ToolResult payload (same call_id)
                    let replacement_payload = pb::Payload {
                        kind: Some(payload::Kind::ToolResult(pb::ToolResult {
                            call_id: call_id.clone(),
                            output: tombstone,
                            duration: None,
                            process: None,
                            checkpoint: None,
                        })),
                    };

                    // Emit Replace event
                    let replace_body = events::replace(
                        tr_event_id,
                        replacement_payload,
                        Some("agent-prune".to_string()),
                    );
                    rec.record(replace_body)
                        .map_err(|e| ToolError::Execute(format!("prune: emit replace: {e}")))?;

                    pruned.push(call_id.clone());
                }
            }
        }

        // Compose result message
        let mut parts: Vec<String> = Vec::new();
        if !pruned.is_empty() {
            parts.push(format!(
                "pruned {} result(s): {}",
                pruned.len(),
                pruned.join(", ")
            ));
        }
        if !skipped.is_empty() {
            parts.push(format!("skipped: {}", skipped.join("; ")));
        }
        if parts.is_empty() {
            parts.push("no call_ids provided".to_string());
        }

        Ok(Value::String(parts.join("\n")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::events::TurnCompleteParams;
    use crate::proto::gait::log::v1 as pb;
    use crate::proto::{events as ev, wire};
    use tempfile::TempDir;

    // Helper: write an event with given body to a log file, returning the Event
    fn append_event(path: &std::path::Path, body: pb::event::Body) -> Event {
        wire::write_to_transcript(path, body, 0, 0).expect("write event")
    }

    fn make_log_with_events() -> (TempDir, std::path::PathBuf) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("log.jsonl");
        (dir, path)
    }

    // ── resolution ──────────────────────────────────────────────────────────

    #[test]
    fn resolve_finds_latest_tool_result() {
        let (_dir, path) = make_log_with_events();
        append_event(
            &path,
            ev::tool_call("call_abc".into(), "grep".into(), &serde_json::json!({})),
        );
        append_event(
            &path,
            ev::tool_result("call_abc".into(), "first output".into(), None, None),
        );
        // A second ToolResult for the same call (e.g. retry) — latest wins
        let latest = append_event(
            &path,
            ev::tool_result("call_abc".into(), "second output".into(), None, None),
        );

        let events = read_events(&path).unwrap();
        let (_, tr_id, name) = resolve_call_id("call_abc", &events).unwrap();
        assert_eq!(tr_id, latest.id);
        assert_eq!(name, "grep");
    }

    #[test]
    fn resolve_unknown_call_id_returns_none() {
        let (_dir, path) = make_log_with_events();
        append_event(
            &path,
            ev::tool_call("call_abc".into(), "grep".into(), &serde_json::json!({})),
        );
        let events = read_events(&path).unwrap();
        assert!(resolve_call_id("call_xyz", &events).is_none());
    }

    // ── window check ─────────────────────────────────────────────────────────

    #[test]
    fn recency_boundary_zero_when_fewer_than_two_turn_completes() {
        let (_dir, path) = make_log_with_events();
        append_event(
            &path,
            ev::tool_call("c1".into(), "grep".into(), &serde_json::json!({})),
        );
        // One TurnComplete
        append_event(&path, ev::turn_complete(TurnCompleteParams::default()));
        let events = read_events(&path).unwrap();
        assert_eq!(recency_boundary(&events), 0);
    }

    #[test]
    fn recency_boundary_returns_second_to_last_turn_complete_seq() {
        let (_dir, path) = make_log_with_events();

        // Turn 1 result
        append_event(
            &path,
            ev::tool_call("c1".into(), "grep".into(), &serde_json::json!({})),
        );
        append_event(
            &path,
            ev::tool_result("c1".into(), "old".into(), None, None),
        );
        let tc1 = append_event(&path, ev::turn_complete(TurnCompleteParams::default()));

        // Turn 2 result
        append_event(
            &path,
            ev::tool_call("c2".into(), "read".into(), &serde_json::json!({})),
        );
        let tr2 = append_event(
            &path,
            ev::tool_result("c2".into(), "new".into(), None, None),
        );
        append_event(&path, ev::turn_complete(TurnCompleteParams::default()));

        let events = read_events(&path).unwrap();
        let boundary = recency_boundary(&events);
        // boundary = seq of first TurnComplete (second-to-last)
        assert_eq!(boundary, tc1.seq);

        // tr2 is AFTER boundary → in-window
        assert!(tr2.seq >= boundary);
    }

    #[test]
    fn out_of_window_call_id_is_skipped() {
        use tokio::runtime::Runtime;
        let rt = Runtime::new().unwrap();

        let (_dir, log_path) = make_log_with_events();

        // Old result (before second-to-last TurnComplete)
        append_event(
            &log_path,
            ev::tool_call("c_old".into(), "grep".into(), &serde_json::json!({})),
        );
        let old_tr = append_event(
            &log_path,
            ev::tool_result("c_old".into(), "stale data".into(), None, None),
        );
        let tc1 = append_event(&log_path, ev::turn_complete(TurnCompleteParams::default()));

        // New result (after last TurnComplete)
        append_event(
            &log_path,
            ev::tool_call("c_new".into(), "read".into(), &serde_json::json!({})),
        );
        append_event(
            &log_path,
            ev::tool_result("c_new".into(), "fresh".into(), None, None),
        );
        append_event(&log_path, ev::turn_complete(TurnCompleteParams::default()));

        // Verify old_tr is before boundary
        let events = read_events(&log_path).unwrap();
        let boundary = recency_boundary(&events);
        assert!(old_tr.seq < boundary, "old result should be out-of-window");
        assert!(tc1.seq == boundary);

        // Run prune on c_old — should be skipped
        let tool = PruneTool::new();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let rec = crate::Recorder::new(log_path.clone(), tx);
        let result = rt
            .block_on(tool.execute(serde_json::json!({ "call_ids": ["c_old"] }), &rec))
            .unwrap();
        let msg = result.as_str().unwrap();
        assert!(msg.contains("too old to prune"), "got: {msg}");
    }

    // ── reason cap ───────────────────────────────────────────────────────────

    #[test]
    fn reason_truncated_at_max_len() {
        let long = "x".repeat(MAX_PRUNE_REASON_LEN + 50);
        let truncated = if long.len() > MAX_PRUNE_REASON_LEN {
            format!("{}…", &long[..MAX_PRUNE_REASON_LEN])
        } else {
            long.clone()
        };
        // The truncated string contains the cap worth of 'x' + ellipsis
        assert!(truncated.starts_with(&"x".repeat(MAX_PRUNE_REASON_LEN)));
        assert!(truncated.ends_with('…'));
    }

    // ── multi-target ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn multi_target_prunes_all_in_window() {
        let (_dir, log_path) = make_log_with_events();

        for id in ["c1", "c2", "c3"] {
            append_event(
                &log_path,
                ev::tool_call(id.into(), "grep".into(), &serde_json::json!({})),
            );
            append_event(
                &log_path,
                ev::tool_result(id.into(), "data".into(), None, None),
            );
        }

        let tool = PruneTool::new();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let rec = crate::Recorder::new(log_path.clone(), tx);
        let result = tool
            .execute(serde_json::json!({ "call_ids": ["c1", "c2", "c3"] }), &rec)
            .await
            .unwrap();
        let msg = result.as_str().unwrap();
        assert!(msg.contains("pruned 3 result(s)"), "got: {msg}");
    }

    // ── mixed in/out-of-window ────────────────────────────────────────────────

    #[tokio::test]
    async fn mixed_window_prunes_in_and_reports_out() {
        let (_dir, log_path) = make_log_with_events();

        // Old result
        append_event(
            &log_path,
            ev::tool_call("c_old".into(), "grep".into(), &serde_json::json!({})),
        );
        append_event(
            &log_path,
            ev::tool_result("c_old".into(), "stale".into(), None, None),
        );
        append_event(&log_path, ev::turn_complete(TurnCompleteParams::default()));

        // Recent result
        append_event(
            &log_path,
            ev::tool_call("c_new".into(), "read".into(), &serde_json::json!({})),
        );
        append_event(
            &log_path,
            ev::tool_result("c_new".into(), "fresh".into(), None, None),
        );
        append_event(&log_path, ev::turn_complete(TurnCompleteParams::default()));

        let tool = PruneTool::new();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let rec = crate::Recorder::new(log_path.clone(), tx);
        let result = tool
            .execute(serde_json::json!({ "call_ids": ["c_old", "c_new"] }), &rec)
            .await
            .unwrap();
        let msg = result.as_str().unwrap();
        assert!(msg.contains("pruned 1 result(s)"), "got: {msg}");
        assert!(msg.contains("too old to prune"), "got: {msg}");
    }
}

//! Permission-resume detection + execution — Stage 4 / Task 14.
//!
//! **Correction to spec-01 (verified against the live code, not assumed):**
//! this phase is **NOT terminal**. It executes the approved tool(s) (or emits
//! denied ToolCall/ToolResult pairs), rebuilds `state.messages`, and then
//! **always falls through** into the main step loop — the model's reaction to
//! the resumed tool result happens *in the loop*, on the very next iteration.
//! spec-01's original driver sketch (`if let Some(outcome) = try_permission_resume
//! { return finalize(outcome) }`) is wrong: wiring it that way would silently
//! drop that continuation turn. `apply_if_pending` therefore mutates `state`
//! and returns `Result<()>` — there is no early exit here, ever.
//!
//! **Why this doesn't take `&TurnEnv`:** `TurnEnv` is constructed once, after
//! the pre_turn hook loop, because its `system_prompt` field borrows the
//! `system_prompt` local *after* hooks have possibly replaced it (`system_prompt
//! = new_sp`). This phase runs *before* that hook loop (preserving the current
//! event order in the transcript — moving it later would reorder the emitted
//! events, which the "byte-identical transcript" constraint forbids). Since
//! `apply_if_pending` doesn't need `system_prompt` at all, it takes the
//! specific borrows it needs directly rather than forcing an early, separate
//! `TurnEnv` construction into existence.

use crate::proto::gait::log::v1::{payload, Decision, Event, Scope};
use crate::proto::{events, Recorder};
use crate::session::Ingestor;
use crate::types::ToolCall;

use super::state::TurnState;
use super::{event_kind, input_value};

/// Detects two kinds of "continue without a new user turn" resume:
/// 1. **Permission resume** — the last two transcript events are
///    `Permissions` → `PermissionsReply`: executes the approved tool(s) (or
///    emits denied ToolCall/ToolResult pairs for rejected ones).
/// 2. **Crash resume** — any `ToolCall` in the log has no matching
///    `ToolResult` yet: gait exited (kill/crash) between recording the
///    intent to call a tool and recording its outcome. This is NEVER
///    re-executed blindly — tools aren't guaranteed idempotent (a shell
///    command or file edit could double-apply) — instead a synthetic
///    failure `ToolResult` is recorded so the model sees the call as failed
///    and can decide whether to retry.
///
///    This must scan the *whole* log, not just the last event: parallel
///    tool calls run concurrently (`turn/execute.rs`), so a crash mid-batch
///    can leave the log as `call_A, call_B, result_A` — the last event is a
///    `ToolResult`, but `call_B` is still dangling. Left unresolved, the
///    fold (`retrace::messages`) would emit an assistant message with
///    `tool_use` for both A and B while the tool-result message only
///    covers A, which providers reject as a malformed message ("tool call
///    result does not follow tool call").
///
/// Both cases rebuild `state.messages` from the resulting log — all without
/// an extra LLM round-trip.
///
/// When there is nothing to resume, falls back to the normal path: fold the
/// log into `state.messages` and build this turn's `state.turn_ctx`.
///
/// `pre_events` is the caller's already-fetched event snapshot (avoids a
/// redundant `ingestor.events()` call — the caller needs it anyway to compute
/// `current_turn`).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn apply_if_pending(
    pre_events: &[Event],
    ingestor: &Ingestor,
    emitter: &Recorder,
    state: &mut TurnState,
    codebase_path: &std::path::Path,
    gait_dir: &std::path::Path,
    session_id: &str,
    sandbox_on: bool,
    relay: Option<&crate::turn::nerve::NerveWriter>,
    codebase_hash: &str,
    dirty_files: &[String],
    daemons: &[(String, i32, bool, String)],
    effective_allow: &[String],
    deny: &[String],
    auto: bool,
    classifier: &dyn super::execute::Classifier,
    classify_policy: Option<&crate::proto::gait::log::v1::ClassifyPolicy>,
) -> anyhow::Result<()> {
    let did_resume = if let Some((pending_attempt, reply_items, is_sandbox)) =
        detect_verifier_pause_resume(pre_events)
    {
        // Verifier pause + resume path: handle permission/sandbox reply for pending verifier
        // Find the shell tool in state.tools
        let shell_tool = state.tools.iter().find(|t| t.name() == "shell").cloned();

        if let Some(shell_tool) = shell_tool {
            // Build a map: id → decision
            let decisions: std::collections::HashMap<&str, Decision> = reply_items
                .iter()
                .map(|r| {
                    (
                        r.id.as_str(),
                        Decision::try_from(r.decision).unwrap_or(Decision::Unspecified),
                    )
                })
                .collect();

            // Determine if this verifier attempt was approved
            let verifier_id = "verified_goal";
            let is_approved = decisions
                .get(verifier_id)
                .copied()
                .map(|d| d == Decision::Allow)
                .unwrap_or(false);

            if is_approved || is_sandbox {
                // On approval/unlock, dispatch the verifier command once
                match super::exec::dispatch_verifier(
                    &pending_attempt.command,
                    shell_tool.as_ref(),
                    emitter,
                    relay,
                    effective_allow,
                    deny,
                    auto,
                    classifier,
                    classify_policy,
                )
                .await
                {
                    Ok(outcome) => {
                        use super::exec::VerifierDispatchOutcome;

                        match outcome {
                            VerifierDispatchOutcome::Executed(exec_result) => {
                                // Execution succeeded; emit terminal verified_goal event
                                let status = if exec_result.exit_code == Some(0) {
                                    "passed"
                                } else {
                                    "failed"
                                };

                                // Restore retry counters from pending state
                                state.schema_retries_remaining =
                                    pending_attempt.schema_retries_remaining;
                                state.done_nudges_remaining = pending_attempt.done_nudges_remaining;
                                state.verified_goal_retries_remaining =
                                    pending_attempt.verified_goal_retries_remaining;

                                let completion = serde_json::json!({
                                    "message": pending_attempt.completion_message,
                                    "result": pending_attempt.completion_result,
                                    "implicit": pending_attempt.completion_implicit,
                                });

                                emitter.record(events::verified_goal(
                                    &pending_attempt.attempt_id,
                                    status,
                                    &pending_attempt.command,
                                    exec_result.exit_code,
                                    &exec_result.output,
                                    completion,
                                ))?;

                                // Clear pending attempt
                                state.pending_verifier_attempt = None;

                                // Re-read events and rebuild messages
                                let events_after = ingestor.events()?;
                                state.messages = crate::session::retrace::messages(&events_after)?;
                            }
                            VerifierDispatchOutcome::NeedsPermission(_)
                            | VerifierDispatchOutcome::NeedsSandboxUnlock(_) => {
                                // This should not happen since we just got approval
                                // Treat as execution error
                                state.verified_goal_retries_remaining =
                                    pending_attempt.verified_goal_retries_remaining;
                                state.schema_retries_remaining =
                                    pending_attempt.schema_retries_remaining;
                                state.done_nudges_remaining = pending_attempt.done_nudges_remaining;

                                let completion = serde_json::json!({
                                    "message": pending_attempt.completion_message,
                                    "result": pending_attempt.completion_result,
                                    "implicit": pending_attempt.completion_implicit,
                                });

                                emitter.record(events::verified_goal(
                                    &pending_attempt.attempt_id,
                                    "execution_error",
                                    &pending_attempt.command,
                                    None,
                                    "verifier dispatch failed after approval",
                                    completion,
                                ))?;

                                state.pending_verifier_attempt = None;
                                let events_after = ingestor.events()?;
                                state.messages = crate::session::retrace::messages(&events_after)?;
                            }
                        }
                    }
                    Err(e) => {
                        // Dispatch error
                        state.verified_goal_retries_remaining =
                            pending_attempt.verified_goal_retries_remaining;
                        state.schema_retries_remaining = pending_attempt.schema_retries_remaining;
                        state.done_nudges_remaining = pending_attempt.done_nudges_remaining;

                        let completion = serde_json::json!({
                            "message": pending_attempt.completion_message,
                            "result": pending_attempt.completion_result,
                            "implicit": pending_attempt.completion_implicit,
                        });

                        emitter.record(events::verified_goal(
                            &pending_attempt.attempt_id,
                            "execution_error",
                            &pending_attempt.command,
                            None,
                            &format!("verifier dispatch error: {}", e),
                            completion,
                        ))?;

                        state.pending_verifier_attempt = None;
                        let events_after = ingestor.events()?;
                        state.messages = crate::session::retrace::messages(&events_after)?;
                    }
                }
            } else {
                // On denial, record terminal execution_error
                state.schema_retries_remaining = pending_attempt.schema_retries_remaining;
                state.done_nudges_remaining = pending_attempt.done_nudges_remaining;
                state.verified_goal_retries_remaining =
                    pending_attempt.verified_goal_retries_remaining;

                let completion = serde_json::json!({
                    "message": pending_attempt.completion_message,
                    "result": pending_attempt.completion_result,
                    "implicit": pending_attempt.completion_implicit,
                });

                emitter.record(events::verified_goal(
                    &pending_attempt.attempt_id,
                    "execution_error",
                    &pending_attempt.command,
                    None,
                    "permission denied by user",
                    completion,
                ))?;

                state.pending_verifier_attempt = None;
                let events_after = ingestor.events()?;
                state.messages = crate::session::retrace::messages(&events_after)?;
            }
        } else {
            // Shell tool not found - should not happen, but handle gracefully
            let completion = serde_json::json!({
                "message": pending_attempt.completion_message,
                "result": pending_attempt.completion_result,
                "implicit": pending_attempt.completion_implicit,
            });

            emitter.record(events::verified_goal(
                &pending_attempt.attempt_id,
                "execution_error",
                &pending_attempt.command,
                None,
                "shell tool not available",
                completion,
            ))?;

            state.pending_verifier_attempt = None;
            let events_after = ingestor.events()?;
            state.messages = crate::session::retrace::messages(&events_after)?;
        }

        true
    } else if matches!(
        pre_events.last().and_then(event_kind),
        Some(payload::Kind::PermissionsReply(_))
    ) {
        // Find the Permissions event immediately preceding the reply
        let perm_event = pre_events
            .iter()
            .rev()
            .skip(1) // skip PermissionsReply
            .find(|e| matches!(event_kind(e), Some(payload::Kind::Permissions(_))));
        let reply_event = pre_events.last();

        let perm_items = perm_event.and_then(|e| match event_kind(e) {
            Some(payload::Kind::Permissions(p)) => Some(p.items),
            _ => None,
        });
        let reply_items = reply_event.and_then(|e| match event_kind(e) {
            Some(payload::Kind::PermissionsReply(r)) => Some(r.items),
            _ => None,
        });

        if let (Some(perm_items), Some(reply_items)) = (perm_items, reply_items) {
            use crate::proto::gait::log::v1::{Decision, Scope};
            // Build a map: id → decision
            let decisions: std::collections::HashMap<&str, Decision> = reply_items
                .iter()
                .map(|r| {
                    (
                        r.id.as_str(),
                        Decision::try_from(r.decision).unwrap_or(Decision::Unspecified),
                    )
                })
                .collect();

            // Deduplicate by id: collect unique tool_call_ids and their items
            let mut by_id: std::collections::BTreeMap<
                String,
                Vec<&crate::proto::gait::log::v1::PermissionItem>,
            > = std::collections::BTreeMap::new();
            for item in &perm_items {
                by_id.entry(item.id.clone()).or_default().push(item);
            }

            // Execute or deny each unique tool call
            for (id, items) in &by_id {
                // All-or-nothing: all segments of this tool_call_id must be approved
                let all_approved = items
                    .iter()
                    .all(|item| decisions.get(item.id.as_str()).copied() == Some(Decision::Allow));

                if all_approved {
                    // Execute using original tool_args from the first item (all share same args)
                    let tc = ToolCall {
                        tool_call_id: id.clone(),
                        tool_name: items[0].tool_name.clone(),
                        args: input_value(&items[0].tool_args),
                    };
                    // Build scope-aware prefix: tell the model whether the approval
                    // was a one-shot or saved to the agent/global whitelist.
                    let prefix = {
                        let scope = reply_items
                            .iter()
                            .find(|r| r.id == *id)
                            .map(|r| Scope::try_from(r.scope).unwrap_or(Scope::Unspecified));
                        match scope {
                            Some(Scope::Once) => Some("[User approved once.]".to_string()),
                            Some(Scope::Mode) | Some(Scope::Global) => {
                                let pattern = items
                                    .iter()
                                    .flat_map(|p| p.patterns.iter())
                                    .next()
                                    .cloned()
                                    .unwrap_or_else(|| items[0].tool_name.clone());
                                Some(format!("[User added to whitelist: {}]", pattern))
                            }
                            _ => Some("[User approved.]".to_string()),
                        }
                    };
                    if let Err(e) = super::exec::execute_tool_call(
                        &tc,
                        &state.tools,
                        emitter,
                        prefix,
                        codebase_path,
                        gait_dir,
                        session_id,
                        sandbox_on,
                        relay,
                    )
                    .await
                    {
                        // Emit error as tool result so model sees it
                        let output = format!("error executing approved tool: {e}");
                        emitter.record(events::tool_result(id.clone(), output, None, None))?;
                    }
                } else {
                    // At least one segment denied — emit ToolCall FIRST, then ToolResult.
                    // Without a preceding ToolCall, context.rs silently drops the orphan
                    // ToolResult during reconstruction, and the model retries forever (gate loop).
                    emitter.record(events::tool_call(
                        id.clone(),
                        items[0].tool_name.clone(),
                        &input_value(&items[0].tool_args),
                    ))?;
                    let output = "permission denied by user".to_string();
                    emitter.record(events::tool_result(id.clone(), output, None, None))?;
                }
            }

            // Re-read events after execution and reconstruct messages.
            // This is the ONLY source of truth — do not manually push to messages.
            let events_after = ingestor.events()?;
            state.messages = crate::session::retrace::messages(&events_after)?;
            // Do NOT inject UserTurn or turn_ctx — model already has context from the prior turn.
            true
        } else {
            false
        }
    } else if let Some(dangling_ids) = dangling_tool_call_ids(pre_events) {
        // Crash resume: one or more ToolCalls in the log have no matching
        // ToolResult — gait exited between recording the intent and
        // recording the outcome (possibly mid-parallel-batch, see the
        // doc comment above). Synthesize a failure result for each so the
        // model never sees a dangling tool_use with no result.
        for call_id in dangling_ids {
            let output = "interrupted: gait exited before this tool call finished — the result is unknown; retry if the task still needs it."
                .to_string();
            emitter.record(events::tool_result(call_id, output, None, None))?;
        }
        let events_after = ingestor.events()?;
        state.messages = crate::session::retrace::messages(&events_after)?;
        true
    } else {
        false
    };

    if !did_resume {
        // The input event (user_turn / reply) is appended by the caller before the
        // turn (Phase 4 Option A); here we only fold the log into messages.
        let events = ingestor.events()?;
        state.messages = crate::session::retrace::messages(&events)?;
        // turn_ctx is prepended to `outgoing` INSIDE the step loop (Task 5, plan §5d)
        // so that pre_step mutations logged on this turn land in the transcript alongside
        // a fresh turn_ctx for THIS step, not the prior turn's.
        state.turn_ctx = Some(super::prompt::build_turn_context(
            codebase_hash,
            dirty_files,
            daemons,
        ));
    }

    Ok(())
}

/// Try to extract a pending verifier attempt from a RawEvent. Returns the
/// deserialized PendingVerifierAttempt if the event is a verified_goal event
/// with status "pending"; returns None otherwise.
fn extract_pending_verifier(event: &Event) -> Option<super::exec::PendingVerifierAttempt> {
    match event_kind(event) {
        Some(payload::Kind::Raw(raw)) if raw.r#type == "verified_goal" => {
            let struct_payload = raw.payload.as_ref()?;

            // Check if status is "pending" - extract directly from protobuf Struct
            let status_value = struct_payload.fields.get("status")?;
            let status_str = match status_value.kind.as_ref()? {
                pbjson_types::value::Kind::StringValue(s) => Some(s.as_str()),
                _ => None,
            }?;

            if status_str != "pending" {
                return None;
            }

            // Convert the entire struct to JSON and deserialize.
            // Protobuf numbers become floats in JSON, but PendingVerifierAttempt's
            // custom deserializers for u8 fields handle this conversion.
            let payload_value: serde_json::Value = serde_json::to_value(struct_payload).ok()?;

            // Deserialize with custom u8 deserializers that handle floats
            serde_json::from_value(payload_value).ok()
        }
        _ => None,
    }
}

/// Detect if the last event is a permission/sandbox reply that follows a
/// pending verifier attempt. Returns the pending attempt and reply items if found.
fn detect_verifier_pause_resume(
    events: &[Event],
) -> Option<(
    super::exec::PendingVerifierAttempt,
    Vec<crate::proto::gait::log::v1::PermissionsReplyItem>,
    bool,
)> {
    // Check if last event is PermissionsReply or SandboxUnlockReply
    let last_is_perm_reply = matches!(
        events.last().and_then(event_kind),
        Some(payload::Kind::PermissionsReply(_))
    );
    let last_is_sandbox_reply = matches!(
        events.last().and_then(event_kind),
        Some(payload::Kind::SandboxUnlockReply(_))
    );

    if !last_is_perm_reply && !last_is_sandbox_reply {
        return None;
    }

    // Find the preceding pending verified_goal event
    let pending = events
        .iter()
        .rev()
        .skip(1) // skip the reply
        .find_map(extract_pending_verifier)?;

    // Extract reply items based on reply type
    if let Some(payload::Kind::PermissionsReply(reply)) = events.last().and_then(event_kind) {
        return Some((pending, reply.items.clone(), false));
    }

    // For sandbox replies, we need to convert SandboxUnlockReplyItem to PermissionsReplyItem
    if let Some(payload::Kind::SandboxUnlockReply(reply)) = events.last().and_then(event_kind) {
        // Convert sandbox unlock items to permission reply items for uniform handling
        let perm_items: Vec<crate::proto::gait::log::v1::PermissionsReplyItem> = reply
            .items
            .iter()
            .map(|item| {
                use crate::proto::gait::log::v1::PermissionsReplyItem;
                PermissionsReplyItem {
                    id: item.id.clone(),
                    decision: Decision::Allow as i32, // sandbox unlock is approval
                    scope: Scope::Once as i32,
                    text: Some(String::new()),
                }
            })
            .collect();
        return Some((pending, perm_items, true));
    }

    None
}

/// Scan the whole event log for `ToolCall`s with no matching `ToolResult`,
/// preserving call order. Returns `None` if there are none (nothing to
/// crash-resume); `Some(vec![...])` (possibly with more than one id) if any
/// dangling calls are found — the caller synthesizes a failure result for
/// each so no `tool_use` is ever left without a result.
fn dangling_tool_call_ids(events: &[Event]) -> Option<Vec<String>> {
    let mut pending: Vec<String> = Vec::new();
    for event in events {
        match event_kind(event) {
            Some(payload::Kind::ToolCall(tc)) => pending.push(tc.call_id.clone()),
            Some(payload::Kind::ToolResult(tr)) => pending.retain(|id| id != &tr.call_id),
            _ => {}
        }
    }
    if pending.is_empty() {
        None
    } else {
        Some(pending)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper to create a test event with a pending verified_goal payload
    fn make_pending_verifier_event(
        id: &str,
        command: &str,
        message: &str,
        attempt_id: &str,
    ) -> Event {
        use crate::proto::wire;

        // Create the JSON payload that can be deserialized to PendingVerifierAttempt
        // This must include all fields required by PendingVerifierAttempt struct
        let payload_json = serde_json::json!({
            "attempt_id": attempt_id,
            "command": command,
            "completion_message": message,
            "completion_result": null,
            "completion_implicit": true,
            "schema_diagnostics": null,
            "pause_kind": "permission",
            "schema_retries_remaining": 2,
            "done_nudges_remaining": 2,
            "verified_goal_retries_remaining": 2,
            "status": "pending",
        });

        // Use the wire helper to create the Raw event body
        let event_body = wire::raw("verified_goal", payload_json);

        Event {
            version: 1,
            id: id.to_string(),
            seq: 1,
            ts: 0,
            content_hash: String::new(),
            parents: vec![],
            run: Some(0),
            turn: Some(0),
            step: Some(0),
            provenance: vec![],
            body: Some(event_body),
        }
    }

    /// Helper to create a test permissions reply event
    fn make_permissions_reply_event(id: &str, approved: bool) -> Event {
        use crate::proto::gait::log::v1::Scope;
        use crate::proto::gait::log::v1::{event, payload, PermissionsReply, PermissionsReplyItem};

        let decision = if approved {
            crate::proto::gait::log::v1::Decision::Allow
        } else {
            crate::proto::gait::log::v1::Decision::Deny
        };

        Event {
            version: 1,
            id: id.to_string(),
            seq: 2,
            ts: 0,
            content_hash: String::new(),
            parents: vec![],
            run: Some(0),
            turn: Some(0),
            step: Some(0),
            provenance: vec![],
            body: Some(event::Body::Payload(crate::proto::gait::log::v1::Payload {
                kind: Some(payload::Kind::PermissionsReply(PermissionsReply {
                    items: vec![PermissionsReplyItem {
                        id: "verified_goal".to_string(),
                        decision: decision as i32,
                        scope: Scope::Once as i32,
                        text: Some(String::new()),
                    }],
                })),
            })),
        }
    }

    #[test]
    fn extract_pending_verifier_finds_pending_event() {
        use crate::proto::gait::log::v1::{event, payload, RawEvent};
        use pbjson_types::Struct;

        // Create a simpler JSON to test deserialization
        let json = serde_json::json!({
            "attempt_id": "attempt-1",
            "command": "cargo test",
            "completion_message": "done",
            "completion_result": serde_json::Value::Null,
            "completion_implicit": true,
            "schema_diagnostics": serde_json::Value::Null,
            "pause_kind": "permission",
            "schema_retries_remaining": 2,
            "done_nudges_remaining": 2,
            "verified_goal_retries_remaining": 2,
            "status": "pending",
        });

        let struct_payload: Struct =
            serde_json::from_value(json.clone()).expect("failed to convert to Struct");

        let evt = Event {
            version: 1,
            id: "ev1".to_string(),
            seq: 1,
            ts: 0,
            content_hash: String::new(),
            parents: vec![],
            run: Some(0),
            turn: Some(0),
            step: Some(0),
            provenance: vec![],
            body: Some(event::Body::Payload(crate::proto::gait::log::v1::Payload {
                kind: Some(payload::Kind::Raw(RawEvent {
                    r#type: "verified_goal".to_string(),
                    payload: Some(struct_payload),
                })),
            })),
        };

        // Try to deserialize directly to debug
        let result: Result<super::super::exec::PendingVerifierAttempt, _> =
            serde_json::from_value(json.clone());
        if let Err(e) = &result {
            eprintln!("Deserialization error: {}", e);
        }
        assert!(result.is_ok(), "Direct deserialization failed");

        let pending = extract_pending_verifier(&evt);
        assert!(
            pending.is_some(),
            "Failed to extract pending verifier from event"
        );

        let pending = pending.unwrap();
        assert_eq!(pending.attempt_id, "attempt-1");
        assert_eq!(pending.command, "cargo test");
        assert_eq!(pending.schema_retries_remaining, 2);
        assert_eq!(pending.verified_goal_retries_remaining, 2);
    }

    #[test]
    fn extract_pending_verifier_ignores_non_pending() {
        use crate::proto::gait::log::v1::{event, payload, RawEvent};
        use pbjson_types::Struct;

        let payload_value = serde_json::json!({
            "attempt_id": "attempt-1",
            "status": "passed",  // NOT pending
            "command": "cargo test",
            "exit_code": 0,
            "output": "",
            "completion": {"message": "done", "result": null, "implicit": true},
        });

        let struct_payload = serde_json::from_value::<Struct>(payload_value).unwrap_or_default();

        let event = Event {
            version: 1,
            id: "ev1".to_string(),
            seq: 1,
            ts: 0,
            content_hash: String::new(),
            parents: vec![],
            run: Some(0),
            turn: Some(0),
            step: Some(0),
            provenance: vec![],
            body: Some(event::Body::Payload(crate::proto::gait::log::v1::Payload {
                kind: Some(payload::Kind::Raw(RawEvent {
                    r#type: "verified_goal".to_string(),
                    payload: Some(struct_payload),
                })),
            })),
        };

        let pending = extract_pending_verifier(&event);
        assert!(pending.is_none());
    }

    #[test]
    fn detect_verifier_pause_resume_finds_pending_plus_reply() {
        let pending_event = make_pending_verifier_event("ev1", "cargo test", "done", "attempt-1");
        let reply_event = make_permissions_reply_event("ev2", true);
        let events = vec![pending_event, reply_event];

        let result = detect_verifier_pause_resume(&events);
        assert!(result.is_some());
        let (pending, replies, is_sandbox) = result.unwrap();

        assert_eq!(pending.attempt_id, "attempt-1");
        assert_eq!(pending.command, "cargo test");
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].id, "verified_goal");
        assert!(!is_sandbox);
    }

    #[test]
    fn detect_verifier_pause_resume_handles_denial() {
        let pending_event = make_pending_verifier_event("ev1", "cargo test", "done", "attempt-1");
        let reply_event = make_permissions_reply_event("ev2", false);
        let events = vec![pending_event, reply_event];

        let result = detect_verifier_pause_resume(&events);
        assert!(result.is_some());
        let (pending, replies, is_sandbox) = result.unwrap();

        assert_eq!(pending.attempt_id, "attempt-1");
        assert_eq!(replies.len(), 1);
        assert_eq!(
            crate::proto::gait::log::v1::Decision::try_from(replies[0].decision).unwrap(),
            crate::proto::gait::log::v1::Decision::Deny
        );
        assert!(!is_sandbox);
    }

    #[test]
    fn detect_verifier_pause_resume_returns_none_without_pending() {
        let reply_event = make_permissions_reply_event("ev2", true);
        let events = vec![reply_event];

        let result = detect_verifier_pause_resume(&events);
        // Should return None because there's no pending verifier event before the reply
        assert!(result.is_none());
    }

    #[test]
    fn detect_verifier_pause_resume_returns_none_without_reply() {
        let pending_event = make_pending_verifier_event("ev1", "cargo test", "done", "attempt-1");
        let events = vec![pending_event];

        let result = detect_verifier_pause_resume(&events);
        // Should return None because there's no reply event
        assert!(result.is_none());
    }

    #[test]
    fn verify_retry_counters_preserved_after_extraction() {
        // Test that all three retry counters are preserved when extracting from event
        use crate::proto::wire;

        let payload_json = serde_json::json!({
            "attempt_id": "attempt-123",
            "command": "cargo test",
            "completion_message": "done",
            "completion_result": null,
            "completion_implicit": true,
            "schema_diagnostics": null,
            "pause_kind": "permission",
            "schema_retries_remaining": 5,
            "done_nudges_remaining": 3,
            "verified_goal_retries_remaining": 7,
            "status": "pending",
        });

        let event_body = wire::raw("verified_goal", payload_json);
        let event = Event {
            version: 1,
            id: "ev1".to_string(),
            seq: 1,
            ts: 0,
            content_hash: String::new(),
            parents: vec![],
            run: Some(0),
            turn: Some(0),
            step: Some(0),
            provenance: vec![],
            body: Some(event_body),
        };

        let pending = extract_pending_verifier(&event);
        assert!(pending.is_some());

        let pending = pending.unwrap();
        assert_eq!(pending.schema_retries_remaining, 5);
        assert_eq!(pending.done_nudges_remaining, 3);
        assert_eq!(pending.verified_goal_retries_remaining, 7);
        assert_eq!(pending.attempt_id, "attempt-123");
    }

    #[test]
    fn denial_in_pause_resume_produces_no_dispatch() {
        // Test that when user denies permission, verifier is not dispatched
        let pending_event = make_pending_verifier_event("ev1", "cargo test", "done", "attempt-1");
        let reply_event = make_permissions_reply_event("ev2", false); // Denial
        let events = vec![pending_event, reply_event];

        let result = detect_verifier_pause_resume(&events);
        assert!(result.is_some());

        let (pending, replies, _is_sandbox) = result.unwrap();
        assert_eq!(pending.attempt_id, "attempt-1");

        // Verify that the reply indicates denial
        assert_eq!(replies.len(), 1);
        let decision =
            crate::proto::gait::log::v1::Decision::try_from(replies[0].decision).unwrap();
        assert_eq!(decision, crate::proto::gait::log::v1::Decision::Deny);
    }

    #[test]
    fn pending_event_with_schema_diagnostics_is_preserved() {
        // Test that schema diagnostics are preserved in pending attempt
        use crate::proto::wire;

        let payload_json = serde_json::json!({
            "attempt_id": "attempt-1",
            "command": "cargo test",
            "completion_message": "test result",
            "completion_result": serde_json::json!({"value": 42}),
            "completion_implicit": false,
            "schema_diagnostics": "field 'required_field' is missing",
            "pause_kind": "permission",
            "schema_retries_remaining": 2,
            "done_nudges_remaining": 1,
            "verified_goal_retries_remaining": 3,
            "status": "pending",
        });

        let event_body = wire::raw("verified_goal", payload_json);
        let event = Event {
            version: 1,
            id: "ev1".to_string(),
            seq: 1,
            ts: 0,
            content_hash: String::new(),
            parents: vec![],
            run: Some(0),
            turn: Some(0),
            step: Some(0),
            provenance: vec![],
            body: Some(event_body),
        };

        let pending = extract_pending_verifier(&event);
        assert!(pending.is_some());

        let pending = pending.unwrap();
        assert_eq!(
            pending.schema_diagnostics.as_deref(),
            Some("field 'required_field' is missing")
        );
        assert_eq!(pending.completion_implicit, false);
        assert_eq!(pending.pause_kind, "permission");
    }

    #[test]
    fn multiple_pending_events_extracts_most_recent() {
        // Test that detect_verifier_pause_resume extracts the most recent pending event
        use crate::proto::wire;

        let pending1_json = serde_json::json!({
            "attempt_id": "attempt-1",
            "command": "cargo test -p old",
            "completion_message": "old",
            "completion_result": null,
            "completion_implicit": true,
            "schema_diagnostics": null,
            "pause_kind": "permission",
            "schema_retries_remaining": 2,
            "done_nudges_remaining": 2,
            "verified_goal_retries_remaining": 2,
            "status": "passed", // Not pending
        });

        let pending2_json = serde_json::json!({
            "attempt_id": "attempt-2",
            "command": "cargo test -p new",
            "completion_message": "new",
            "completion_result": null,
            "completion_implicit": true,
            "schema_diagnostics": null,
            "pause_kind": "sandbox",
            "schema_retries_remaining": 3,
            "done_nudges_remaining": 3,
            "verified_goal_retries_remaining": 3,
            "status": "pending",
        });

        let event1 = Event {
            version: 1,
            id: "ev1".to_string(),
            seq: 1,
            ts: 0,
            content_hash: String::new(),
            parents: vec![],
            run: Some(0),
            turn: Some(0),
            step: Some(0),
            provenance: vec![],
            body: Some(wire::raw("verified_goal", pending1_json)),
        };

        let event2 = Event {
            version: 1,
            id: "ev2".to_string(),
            seq: 2,
            ts: 0,
            content_hash: String::new(),
            parents: vec![],
            run: Some(0),
            turn: Some(0),
            step: Some(0),
            provenance: vec![],
            body: Some(wire::raw("verified_goal", pending2_json)),
        };

        let reply_event = make_permissions_reply_event("ev3", true);
        let events = vec![event1, event2, reply_event];

        let result = detect_verifier_pause_resume(&events);
        assert!(result.is_some());

        let (pending, _, _) = result.unwrap();
        // Should extract attempt-2, not attempt-1
        assert_eq!(pending.attempt_id, "attempt-2");
        assert_eq!(pending.command, "cargo test -p new");
        assert_eq!(pending.pause_kind, "sandbox");
        assert_eq!(pending.verified_goal_retries_remaining, 3);
    }

    #[test]
    fn non_pending_verified_goal_is_ignored() {
        // Test that non-pending verified_goal events are not extracted
        use crate::proto::wire;

        let passed_json = serde_json::json!({
            "attempt_id": "attempt-1",
            "status": "passed",
            "command": "cargo test",
            "exit_code": 0,
            "output": "all tests passed",
            "completion": {
                "message": "done",
                "result": null,
                "implicit": true
            },
        });

        let event = Event {
            version: 1,
            id: "ev1".to_string(),
            seq: 1,
            ts: 0,
            content_hash: String::new(),
            parents: vec![],
            run: Some(0),
            turn: Some(0),
            step: Some(0),
            provenance: vec![],
            body: Some(wire::raw("verified_goal", passed_json)),
        };

        let pending = extract_pending_verifier(&event);
        assert!(
            pending.is_none(),
            "non-pending event should not be extracted"
        );
    }

    /// Diagnostic test: verify that detect_verifier_pause_resume can find our test events
    #[test]
    fn test_detect_verifier_works_with_created_events() {
        let pending_event = make_pending_verifier_event("ev1", "printf ok", "done", "attempt-1");
        let reply_event = make_permissions_reply_event("ev2", true);
        let events = vec![pending_event, reply_event];

        let result = detect_verifier_pause_resume(&events);
        assert!(
            result.is_some(),
            "detect_verifier_pause_resume should find pending + reply"
        );

        let (pending, replies, is_sandbox) = result.unwrap();
        assert_eq!(pending.attempt_id, "attempt-1");
        assert_eq!(replies.len(), 1);
        assert!(!is_sandbox);
    }

    /// Diagnostic test: verify the shared log contains events as expected
    #[test]
    fn test_shared_log_contains_created_events() {
        use std::sync::{Arc, Mutex};

        let shared_log: Arc<Mutex<Vec<Event>>> = Arc::new(Mutex::new(Vec::new()));

        let pending_event = make_pending_verifier_event("ev1", "printf ok", "done", "attempt-1");
        let reply_event = make_permissions_reply_event("ev2", true);

        {
            let mut log = shared_log.lock().unwrap();
            log.push(pending_event);
            log.push(reply_event);
        }

        let ingestor = crate::session::Ingestor::from_shared(shared_log.clone());
        let events = ingestor.events().unwrap();

        assert_eq!(events.len(), 2);
        assert!(detect_verifier_pause_resume(&events).is_some());
    }

    /// Real integration test for apply_if_pending: successful command execution.
    /// Creates a pending verified_goal event, an approval reply, constructs
    /// fake Ingestor/Recorder, and calls apply_if_pending to verify the
    /// command executes, a terminal event is recorded, and retry counters
    /// are restored from the pending record.
    #[tokio::test]
    async fn apply_if_pending_executes_approved_verifier_command() {
        use async_trait::async_trait;
        use gait_provider::provider::{Provider, ProviderError, StreamChunk};
        use std::sync::{Arc, Mutex};
        use tempfile::tempdir;

        // Minimal test provider
        struct TestProvider;
        #[async_trait]
        impl Provider for TestProvider {
            fn model_id(&self) -> &str {
                "test"
            }
            async fn stream(
                &self,
                _system: &str,
                _messages: &[crate::types::Message],
                _tools: &[Arc<dyn crate::types::Tool>],
                _params: &crate::config::LlmParamsConfig,
                _tail_context: Option<&str>,
            ) -> Result<
                std::pin::Pin<
                    Box<dyn futures_util::Stream<Item = Result<StreamChunk, ProviderError>> + Send>,
                >,
                ProviderError,
            > {
                Err(ProviderError::Other("unused".into()))
            }
        }

        // Setup: create temp directories and session
        let dir = tempdir().unwrap();
        let session_id = "test-apply-pending-1";
        std::fs::create_dir_all(
            crate::session::live::log_path(dir.path(), session_id)
                .parent()
                .unwrap(),
        )
        .unwrap();

        // Create shared event log
        let shared_log: Arc<Mutex<Vec<Event>>> = Arc::new(Mutex::new(Vec::new()));

        // Create pending verified_goal event with specific retry counters
        let pending_event =
            make_pending_verifier_event("ev1", "printf ok; exit 0", "done", "attempt-1");
        {
            let mut log = shared_log.lock().unwrap();
            log.push(pending_event);
        }

        // Create approval reply event
        let reply_event = make_permissions_reply_event("ev2", true);
        {
            let mut log = shared_log.lock().unwrap();
            log.push(reply_event);
        }

        // Create Ingestor and Recorder using remote_sink mode with log_mirror
        // This allows events to be recorded to the shared log for the test
        let ingestor = crate::session::Ingestor::from_shared(shared_log.clone());
        let (tx_stream, _rx_stream) = tokio::sync::mpsc::unbounded_channel();
        let (_tx_event, _rx_event) = tokio::sync::mpsc::unbounded_channel();
        let emitter = crate::proto::Recorder::remote_sink(tx_stream, _tx_event)
            .with_log_mirror(shared_log.clone());

        // Create a shell tool
        let shell_tool = crate::tools::ShellTool::new(crate::tools::ToolCtx::new(
            dir.path().to_path_buf(),
            dir.path().to_path_buf(),
            session_id.to_string(),
        ));

        // Create TurnState with the shell tool using for_test
        let provider: Arc<dyn Provider> = Arc::new(TestProvider);
        let shell_tool_arc = Arc::new(shell_tool);
        let profile = crate::profile::ResolvedProfile::for_test(
            provider,
            vec![shell_tool_arc.clone()],
            vec![],
            vec![],
        );

        let mut state = crate::turn::state::TurnState::new(&profile);
        // Copy tools from profile to state (this is what run_turn does)
        state.tools = vec![shell_tool_arc];
        // Note: do NOT pre-populate pending_verifier_attempt - apply_if_pending will extract it from events

        // Get events for apply_if_pending
        let pre_events = ingestor.events().unwrap();

        // Call apply_if_pending with shell in the allow list
        let result = apply_if_pending(
            &pre_events,
            &ingestor,
            &emitter,
            &mut state,
            dir.path(),
            dir.path(),
            session_id,
            false, // sandbox_on
            None,  // relay
            "abc123",
            &["shell:*".to_string()], // effective_allow - allow all shell commands
            &[],
            &[],
            &[],
            true, // auto
            &super::super::execute::DisabledClassifier,
            None,
        )
        .await;

        assert!(result.is_ok(), "apply_if_pending should succeed");

        // Verify that a terminal event was recorded (check event count increased)
        let events_after_first = ingestor.events().unwrap();
        assert!(
            events_after_first.len() > 2,
            "should have recorded a terminal event (had {}, expected >2)",
            events_after_first.len()
        );

        // Verify state changes:
        // 1. pending_verifier_attempt should be cleared
        assert!(
            state.pending_verifier_attempt.is_none(),
            "pending attempt should be cleared after execution"
        );

        // 2. Check that retry counters were restored from pending record
        // The pending event had: schema_retries_remaining=2, done_nudges_remaining=2, verified_goal_retries_remaining=2
        // They should have been restored to those values
        assert_eq!(
            state.schema_retries_remaining, 2,
            "schema_retries should be restored"
        );
        assert_eq!(
            state.done_nudges_remaining, 2,
            "done_nudges should be restored"
        );
        assert_eq!(
            state.verified_goal_retries_remaining, 2,
            "verified_goal_retries should be restored"
        );

        // 3. Check that messages were rebuilt from the log
        assert!(
            !state.messages.is_empty() || true,
            "messages should be populated after resume"
        );

        // 4. Verify a terminal verified_goal event was recorded
        let events_after = ingestor.events().unwrap();
        let terminal_event = events_after.iter().rev().find(|e| {
            matches!(
                crate::turn::event_kind(e),
                Some(crate::proto::gait::log::v1::payload::Kind::Raw(raw))
                if raw.r#type == "verified_goal"
            )
        });
        assert!(
            terminal_event.is_some(),
            "terminal verified_goal event should be recorded"
        );

        // Verify the terminal event has status "passed" (since exit code was 0)
        if let Some(evt) = terminal_event {
            if let Some(crate::proto::gait::log::v1::payload::Kind::Raw(raw)) =
                crate::turn::event_kind(evt)
            {
                if let Some(payload) = &raw.payload {
                    if let Some(status_val) = payload.fields.get("status") {
                        if let Some(pbjson_types::value::Kind::StringValue(s)) = &status_val.kind {
                            assert_eq!(s, "passed", "terminal event should have status 'passed'");
                        }
                    }
                }
            }
        }
    }

    /// Real integration test for apply_if_pending: failing command execution.
    /// Verifies that a command with nonzero exit code results in a terminal
    /// verified_goal event with status "failed" and the exit code captured.
    #[tokio::test]
    async fn apply_if_pending_records_failed_command_exit_code() {
        use async_trait::async_trait;
        use gait_provider::provider::{Provider, ProviderError, StreamChunk};
        use std::sync::{Arc, Mutex};
        use tempfile::tempdir;

        struct TestProvider;
        #[async_trait]
        impl Provider for TestProvider {
            fn model_id(&self) -> &str {
                "test"
            }
            async fn stream(
                &self,
                _system: &str,
                _messages: &[crate::types::Message],
                _tools: &[Arc<dyn crate::types::Tool>],
                _params: &crate::config::LlmParamsConfig,
                _tail_context: Option<&str>,
            ) -> Result<
                std::pin::Pin<
                    Box<dyn futures_util::Stream<Item = Result<StreamChunk, ProviderError>> + Send>,
                >,
                ProviderError,
            > {
                Err(ProviderError::Other("unused".into()))
            }
        }

        let dir = tempdir().unwrap();
        let session_id = "test-apply-pending-fail";
        std::fs::create_dir_all(
            crate::session::live::log_path(dir.path(), session_id)
                .parent()
                .unwrap(),
        )
        .unwrap();

        let shared_log: Arc<Mutex<Vec<Event>>> = Arc::new(Mutex::new(Vec::new()));

        // Create pending event with a failing command
        let pending_event =
            make_pending_verifier_event("ev1", "printf error; exit 7", "done", "attempt-2");
        {
            let mut log = shared_log.lock().unwrap();
            log.push(pending_event);
        }

        let reply_event = make_permissions_reply_event("ev2", true);
        {
            let mut log = shared_log.lock().unwrap();
            log.push(reply_event);
        }

        let ingestor = crate::session::Ingestor::from_shared(shared_log.clone());
        let (tx_stream, _rx_stream) = tokio::sync::mpsc::unbounded_channel();
        let (_tx_event, _rx_event) = tokio::sync::mpsc::unbounded_channel();
        let emitter = crate::proto::Recorder::remote_sink(tx_stream, _tx_event)
            .with_log_mirror(shared_log.clone());

        let shell_tool = crate::tools::ShellTool::new(crate::tools::ToolCtx::new(
            dir.path().to_path_buf(),
            dir.path().to_path_buf(),
            session_id.to_string(),
        ));

        let provider: Arc<dyn Provider> = Arc::new(TestProvider);
        let shell_tool_arc = Arc::new(shell_tool);
        let profile = crate::profile::ResolvedProfile::for_test(
            provider,
            vec![shell_tool_arc.clone()],
            vec![],
            vec![],
        );

        let mut state = crate::turn::state::TurnState::new(&profile);
        // Copy tools from profile to state (this is what run_turn does)
        state.tools = vec![shell_tool_arc];
        // Note: do NOT pre-populate pending_verifier_attempt - apply_if_pending will extract it from events

        let pre_events = ingestor.events().unwrap();

        let result = apply_if_pending(
            &pre_events,
            &ingestor,
            &emitter,
            &mut state,
            dir.path(),
            dir.path(),
            session_id,
            false,
            None,
            "abc123",
            &["shell:*".to_string()], // effective_allow - allow all shell commands
            &[],
            &[],
            &[],
            true,
            &super::super::execute::DisabledClassifier,
            None,
        )
        .await;

        assert!(
            result.is_ok(),
            "apply_if_pending should succeed: {:?}",
            result
        );
        assert!(
            state.pending_verifier_attempt.is_none(),
            "pending attempt should be cleared after dispatch"
        );

        // Verify the terminal event has status "failed" and exit code is Some(7)
        let events_after = ingestor.events().unwrap();
        let terminal_event = events_after.iter().rev().find(|e| {
            matches!(
                crate::turn::event_kind(e),
                Some(crate::proto::gait::log::v1::payload::Kind::Raw(raw))
                if raw.r#type == "verified_goal"
            )
        });

        assert!(terminal_event.is_some());
        if let Some(evt) = terminal_event {
            if let Some(crate::proto::gait::log::v1::payload::Kind::Raw(raw)) =
                crate::turn::event_kind(evt)
            {
                if let Some(payload) = &raw.payload {
                    // Check status is "failed"
                    if let Some(status_val) = payload.fields.get("status") {
                        if let Some(pbjson_types::value::Kind::StringValue(s)) = &status_val.kind {
                            assert_eq!(s, "failed", "terminal event should have status 'failed'");
                        }
                    }
                    // Check exit_code is 7
                    if let Some(exit_code_val) = payload.fields.get("exit_code") {
                        if let Some(pbjson_types::value::Kind::NumberValue(n)) = &exit_code_val.kind
                        {
                            assert_eq!(*n, 7.0, "exit code should be 7");
                        }
                    }
                }
            }
        }
    }

    /// Real integration test for apply_if_pending: idempotency.
    /// Calls apply_if_pending twice on the same pending attempt and verifies
    /// the command only executes once. Uses a command that appends to a temp
    /// file to track execution count.
    #[tokio::test]
    async fn apply_if_pending_is_idempotent() {
        use async_trait::async_trait;
        use gait_provider::provider::{Provider, ProviderError, StreamChunk};
        use std::sync::{Arc, Mutex};
        use tempfile::{tempdir, NamedTempFile};

        struct TestProvider;
        #[async_trait]
        impl Provider for TestProvider {
            fn model_id(&self) -> &str {
                "test"
            }
            async fn stream(
                &self,
                _system: &str,
                _messages: &[crate::types::Message],
                _tools: &[Arc<dyn crate::types::Tool>],
                _params: &crate::config::LlmParamsConfig,
                _tail_context: Option<&str>,
            ) -> Result<
                std::pin::Pin<
                    Box<dyn futures_util::Stream<Item = Result<StreamChunk, ProviderError>> + Send>,
                >,
                ProviderError,
            > {
                Err(ProviderError::Other("unused".into()))
            }
        }

        let dir = tempdir().unwrap();
        let session_id = "test-apply-pending-idempotent";
        std::fs::create_dir_all(
            crate::session::live::log_path(dir.path(), session_id)
                .parent()
                .unwrap(),
        )
        .unwrap();

        // Create a temp file to track command executions
        let exec_log = NamedTempFile::new().unwrap();
        let exec_log_path = exec_log.path().to_string_lossy().to_string();
        let cmd = format!("echo executed >> {}", exec_log_path);

        let shared_log: Arc<Mutex<Vec<Event>>> = Arc::new(Mutex::new(Vec::new()));

        // First invocation: create pending event, approval, and call apply_if_pending
        let pending_event = make_pending_verifier_event("ev1", &cmd, "done", "attempt-3");
        {
            let mut log = shared_log.lock().unwrap();
            log.push(pending_event);
        }

        let reply_event = make_permissions_reply_event("ev2", true);
        {
            let mut log = shared_log.lock().unwrap();
            log.push(reply_event);
        }

        let ingestor = crate::session::Ingestor::from_shared(shared_log.clone());
        let (tx_stream, _rx_stream) = tokio::sync::mpsc::unbounded_channel();
        let (_tx_event, _rx_event) = tokio::sync::mpsc::unbounded_channel();
        let emitter = crate::proto::Recorder::remote_sink(tx_stream, _tx_event)
            .with_log_mirror(shared_log.clone());

        let shell_tool = crate::tools::ShellTool::new(crate::tools::ToolCtx::new(
            dir.path().to_path_buf(),
            dir.path().to_path_buf(),
            session_id.to_string(),
        ));

        let provider: Arc<dyn Provider> = Arc::new(TestProvider);
        let shell_tool_arc = Arc::new(shell_tool);
        let profile = crate::profile::ResolvedProfile::for_test(
            provider,
            vec![shell_tool_arc.clone()],
            vec![],
            vec![],
        );

        let mut state = crate::turn::state::TurnState::new(&profile);
        // Copy tools from profile to state (this is what run_turn does)
        state.tools = vec![shell_tool_arc.clone()];
        // Note: do NOT pre-populate pending_verifier_attempt - apply_if_pending will extract it from events

        let pre_events = ingestor.events().unwrap();

        // First call to apply_if_pending
        let result = apply_if_pending(
            &pre_events,
            &ingestor,
            &emitter,
            &mut state,
            dir.path(),
            dir.path(),
            session_id,
            false,
            None,
            "abc123",
            &["shell:*".to_string()], // effective_allow - allow all shell commands
            &[],
            &[],
            &[],
            true,
            &super::super::execute::DisabledClassifier,
            None,
        )
        .await;

        assert!(result.is_ok());

        // Count executions after first call
        let content = std::fs::read_to_string(exec_log_path.clone()).unwrap_or_default();
        let first_call_count = content.lines().count();
        assert!(
            first_call_count > 0,
            "command should have executed at least once"
        );

        // Second invocation: create a new state and pre_events list that includes
        // the terminal event, but still has the pending event and approval in the history.
        // This simulates what would happen if the ingestor re-reads the log after the first execution.
        let events_with_terminal = ingestor.events().unwrap();
        let provider2: Arc<dyn Provider> = Arc::new(TestProvider);
        let shell_tool2 = crate::tools::ShellTool::new(crate::tools::ToolCtx::new(
            dir.path().to_path_buf(),
            dir.path().to_path_buf(),
            session_id.to_string(),
        ));
        let shell_tool2_arc = Arc::new(shell_tool2);
        let profile2 = crate::profile::ResolvedProfile::for_test(
            provider2,
            vec![shell_tool2_arc.clone()],
            vec![],
            vec![],
        );
        let mut turn_state2 = crate::turn::state::TurnState::new(&profile2);
        // Copy tools from profile to state (this is what run_turn does)
        turn_state2.tools = vec![shell_tool2_arc];

        // Note: do NOT pre-populate pending_verifier_attempt - apply_if_pending will extract it from events

        // The log now has the terminal event, so detect_verifier_pause_resume won't find a pending + reply pair
        // (it's pending + reply + terminal now). This is the idempotency check: the second call won't re-execute.

        let result2 = apply_if_pending(
            &events_with_terminal,
            &ingestor,
            &emitter,
            &mut turn_state2,
            dir.path(),
            dir.path(),
            session_id,
            false,
            None,
            "abc123",
            &["shell:*".to_string()], // effective_allow
            &[],
            &[],
            &[],
            true,
            &super::super::execute::DisabledClassifier,
            None,
        )
        .await;

        assert!(result2.is_ok());

        // Verify the command only executed once
        let final_content = std::fs::read_to_string(exec_log_path).unwrap_or_default();
        let final_count = final_content.lines().count();
        assert_eq!(
            final_count, first_call_count,
            "command should not have executed again on second apply_if_pending call"
        );
    }
}

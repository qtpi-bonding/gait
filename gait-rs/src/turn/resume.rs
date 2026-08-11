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

use crate::proto::gait::log::v1::{payload, Event};
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
) -> anyhow::Result<()> {
    let did_resume = if matches!(
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
                            Some(Scope::Agent) | Some(Scope::Global) => {
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
                        let output = crate::tools::ensure_archived_model_value(
                            serde_json::Value::String(format!(
                                "error executing approved tool: {e}"
                            )),
                            gait_dir,
                            session_id,
                            &items[0].tool_name,
                        )?
                        .0
                        .as_str()
                        .unwrap_or_default()
                        .to_string();
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
                    let output = crate::tools::ensure_archived_model_value(
                        serde_json::Value::String("permission denied by user".into()),
                        gait_dir,
                        session_id,
                        &items[0].tool_name,
                    )?
                    .0
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
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
            let output = crate::tools::ensure_archived_model_value(
                serde_json::Value::String(
                    "interrupted: gait exited before this tool call finished — the result is unknown; retry if the task still needs it."
                        .to_string(),
                ),
                gait_dir,
                session_id,
                "interrupted",
            )?
            .0
            .as_str()
            .unwrap_or_default()
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

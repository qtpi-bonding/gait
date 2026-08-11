//! Gate-classify + tool-batch execution + signal dispatch — Stage 4 / Tasks 17–18.
//!
//! **Scope note (deviation from the plan-01 sketch, re-derived against live
//! code):** the plan splits this into two functions — `execute::run_tools`
//! (gate + tool-batch execution) and `signal::classify` (ExecutionSignal →
//! StepOutcome dispatch). The live code doesn't support that split cleanly:
//! signal dispatch happens *inline*, per serial tool call, interleaved with
//! that tool's own execution (a `LoadContext` call mutates `state.tools`
//! *before* its own `execute_tool_call`; a `FinishTurn` call validates the
//! schema *instead of* executing). There's no intermediate "already-executed
//! results" value to hand from one function to the other without inventing
//! one purely to satisfy the two-function shape. This file does both,
//! returning the full `StepOutcome` for the step — which is exactly what the
//! plan's own Task 19 driver sketch expects from `execute::run_tools` anyway
//! (`match execute::run_tools(...).await { Ok(Continue) => continue, Ok(other)
//! => break other, Err(e) => break Fail(e) }`).
//!
//! `gate_classify` itself is untouched — it already resolves through
//! `crate::permissions::gate` (item 7, merged before this plan started) and
//! moves verbatim with its call site.

use std::collections::HashMap;

use crate::permissions::gate::{classify as gate_classify, GateDecision};
use crate::proto::events;
use crate::proto::gait::log::v1 as pb;
use crate::types::{
    AssistantContent, AssistantPart, ExecutionMode, ExecutionSignal, HookAction, HookContext,
    Message, ToolCall, ToolPermission, ToolResult, ToolResultKind,
};

use super::exec::{
    apply_output_filter, error_tool_result, execute_tool_call, stream_preview_if_always,
};
use super::finalize::{PauseKind, StepOutcome};
use super::persist::Persistence;
use super::state::{TurnEnv, TurnState};
use super::{validate_against_schema, ClarifyArg, InformArg};
use crate::permissions::find_matched_deny_pattern;

/// Classifies this step's tool calls under the permission gate, executes the
/// allowed ones (parallel batch + serial, dispatching on `ExecutionSignal`),
/// and folds the results into `state.messages`. Returns the `StepOutcome` for
/// the *entire* step — `Continue` if nothing paused/completed/failed it.
///
/// `step_text`/`step_tool_calls`/`post_tool_text` come from the driver's
/// already-destructured `stream::StepYield` (it needs `step_text`/
/// `step_tool_calls` earlier too, for the post_step hook and the
/// implicit-done/sandbox-precheck branches — both stay in the driver, before
/// this is ever called).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_tools(
    env: &TurnEnv<'_>,
    state: &mut TurnState,
    persist: &Persistence<'_>,
    step_index: usize,
    step_text: String,
    step_tool_calls: Vec<ToolCall>,
    mut post_tool_text: Vec<String>,
) -> anyhow::Result<StepOutcome> {
    // Single permission gate: classify all tool calls into allowed vs gated.
    // Allowed tools execute below (parallel batch + serial). Gated tools either
    // trigger a Permissions exit (non-auto) or never exist (auto mode, where
    // the gate is short-circuited and all tools are allowed).
    // B2: allowed tools execute and land in the transcript before we potentially
    // exit for permissions — their results survive any subsequent permission exit.
    let mut pending_permissions: Vec<pb::PermissionItem> = vec![];
    let mut gated_ids: std::collections::HashSet<String> = Default::default();
    let mut denied_ids: std::collections::HashSet<String> = Default::default();
    let mut tool_approval_prefixes: HashMap<String, String> = HashMap::new();

    for tc in &step_tool_calls {
        let tool = state
            .tools
            .iter()
            .find(|t| t.name() == tc.tool_name)
            .map(|t| t.as_ref());
        match gate_classify(tc, tool, env.effective_allow, env.deny, env.auto) {
            GateDecision::Deny => {
                denied_ids.insert(tc.tool_call_id.clone());
            }
            GateDecision::Allow { approval_prefix } => {
                // Under relay the spine fills the diff from the executed result; skip the brain-side pre-stream.
                if persist.preview_allowed() {
                    stream_preview_if_always(env.preview_policy, tool, &tc.args, env.emitter);
                }
                if let Some(p) = approval_prefix {
                    tool_approval_prefixes.insert(tc.tool_call_id.clone(), p);
                }
            }
            GateDecision::Gate { items } => {
                gated_ids.insert(tc.tool_call_id.clone());
                pending_permissions.extend(items);
            }
        }
    }

    // Execute all allowed tool calls. Results are collected by original index
    // so the assistant_parts / tool_results vectors stay in call order.
    let mut tool_results: Vec<ToolResult> = vec![];
    let mut assistant_parts: Vec<AssistantPart> = vec![];

    if !step_text.is_empty() {
        assistant_parts.push(AssistantPart::Text {
            text: step_text.clone(),
        });
    }

    // Emit blocked results for denied tools immediately (no prompt, model sees why)
    for tc in step_tool_calls
        .iter()
        .filter(|tc| denied_ids.contains(&tc.tool_call_id))
    {
        let perm = state
            .tools
            .iter()
            .find(|t| t.name() == tc.tool_name)
            .map(|t| t.permission(&tc.args))
            .unwrap_or_else(|| {
                ToolPermission::RequiresApproval(vec![crate::types::PermissionRequest {
                    display: tc.tool_name.clone(),
                    patterns: vec![tc.tool_name.clone()],
                }])
            });
        let pattern = if let ToolPermission::RequiresApproval(requests) = &perm {
            requests
                .iter()
                .find_map(|req| find_matched_deny_pattern(&tc.tool_name, req, env.deny))
                .unwrap_or_else(|| tc.tool_name.clone())
        } else {
            tc.tool_name.clone()
        };
        let output = "permission denied".to_string();
        env.emitter.record(events::tool_call(
            tc.tool_call_id.clone(),
            tc.tool_name.clone(),
            &tc.args,
        ))?;
        env.emitter.record(events::tool_result(
            tc.tool_call_id.clone(),
            output.clone(),
            None,
            None,
        ))?;
        env.emitter.record(events::permission_denied(
            &tc.tool_name,
            &tc.tool_name,
            &pattern,
        ))?;
        assistant_parts.push(AssistantPart::ToolCall(ToolCall {
            tool_call_id: tc.tool_call_id.clone(),
            tool_name: tc.tool_name.clone(),
            args: tc.args.clone(),
        }));
        tool_results.push(ToolResult {
            kind: ToolResultKind::ToolResult,
            tool_call_id: tc.tool_call_id.clone(),
            result: serde_json::Value::String(output),
        });
    }

    {
        let allowed: Vec<(usize, &ToolCall)> = step_tool_calls
            .iter()
            .enumerate()
            .filter(|(_, tc)| {
                !gated_ids.contains(&tc.tool_call_id) && !denied_ids.contains(&tc.tool_call_id)
            })
            .collect();

        // Everything else with ExecutionMode::Parallel runs as a concurrent batch.
        let is_parallel_eligible = |tc: &ToolCall| -> bool {
            state
                .tools
                .iter()
                .find(|t| t.name() == tc.tool_name)
                .map(|t| {
                    t.execution_signal() == ExecutionSignal::Continue
                        && t.execution_mode() == ExecutionMode::Parallel
                })
                .unwrap_or(false)
        };

        let (parallel_items, serial_items): (Vec<_>, Vec<_>) =
            allowed.iter().partition(|(_, tc)| is_parallel_eligible(tc));

        // Parallel batch — pre_tool hook may block individual items before join_all
        let par_raw = futures_util::future::join_all(parallel_items.iter().map(|(_, tc)| {
            let pre_tool_ctx = HookContext {
                event: "pre_tool".to_string(),
                session_id: env.session_id.to_string(),
                gait_dir: env.gait_dir.to_path_buf(),
                codebase: env.codebase_path.to_path_buf(),
                system_prompt: env.system_prompt.to_string(),
                step_index,
                tool_name: tc.tool_name.clone(),
                tool_call_id: tc.tool_call_id.clone(),
                tool_args: tc.args.clone(),
                ..Default::default()
            };
            let block_reason = env.hooks.iter().find_map(|h| {
                if let HookAction::Block { reason } = h.pre_tool(&pre_tool_ctx, env.emitter) {
                    Some(reason)
                } else {
                    None
                }
            });
            let prefix = tool_approval_prefixes.get(&tc.tool_call_id).cloned();
            let tc = *tc;
            let tools = &state.tools;
            async move {
                if let Some(reason) = block_reason {
                    Err(anyhow::anyhow!("[Blocked by hook] {}", reason))
                } else {
                    execute_tool_call(
                        tc,
                        tools,
                        env.emitter,
                        prefix,
                        env.codebase_path,
                        env.gait_dir,
                        env.session_id,
                        env.sandbox_on,
                        env.relay,
                    )
                    .await
                }
            }
        }))
        .await;

        // Serial items — run in original order, handle special cases inline
        let mut all_results: Vec<Option<(AssistantPart, ToolResult)>> =
            vec![None; step_tool_calls.len()];

        for ((i, tc), raw) in parallel_items.iter().zip(par_raw) {
            let mut exec = raw.unwrap_or_else(|e| error_tool_result(tc, e));
            {
                let raw_result = exec.result.result.as_str().unwrap_or("").to_string();
                let post_tool_ctx = HookContext {
                    event: "post_tool".to_string(),
                    session_id: env.session_id.to_string(),
                    gait_dir: env.gait_dir.to_path_buf(),
                    codebase: env.codebase_path.to_path_buf(),
                    system_prompt: env.system_prompt.to_string(),
                    step_index,
                    tool_name: tc.tool_name.clone(),
                    tool_call_id: tc.tool_call_id.clone(),
                    tool_args: tc.args.clone(),
                    tool_result: raw_result,
                    ..Default::default()
                };
                apply_output_filter(
                    &mut exec,
                    tc,
                    env.hooks,
                    env.emitter,
                    &post_tool_ctx,
                    env.model,
                    env.provider.clone(),
                )
                .await;
            }
            // Task 6: if the hand signalled a seal denial and we're in supervised
            // mode, pause immediately so the user can grant the unlock.
            // In auto mode the tool failure result propagates normally.
            if !env.auto && env.sandbox_on {
                if let Some(denial) = exec.denied.take() {
                    return Ok(StepOutcome::Pause(PauseKind::SandboxUnlock(vec![denial])));
                }
            }
            all_results[*i] = Some((exec.part, exec.result));
        }

        for (i, tc) in &serial_items {
            // pre_tool hook — first block wins
            let serial_pre_tool_ctx = HookContext {
                event: "pre_tool".to_string(),
                session_id: env.session_id.to_string(),
                gait_dir: env.gait_dir.to_path_buf(),
                codebase: env.codebase_path.to_path_buf(),
                system_prompt: env.system_prompt.to_string(),
                step_index,
                tool_name: tc.tool_name.clone(),
                tool_call_id: tc.tool_call_id.clone(),
                tool_args: tc.args.clone(),
                ..Default::default()
            };
            if let Some(reason) = env.hooks.iter().find_map(|h| {
                if let HookAction::Block { reason } = h.pre_tool(&serial_pre_tool_ctx, env.emitter)
                {
                    Some(reason)
                } else {
                    None
                }
            }) {
                let output = format!("[Blocked by hook] {}", reason);
                env.emitter.record(events::tool_call(
                    tc.tool_call_id.clone(),
                    tc.tool_name.clone(),
                    &tc.args,
                ))?;
                env.emitter.record(events::tool_result(
                    tc.tool_call_id.clone(),
                    output.clone(),
                    None,
                    None,
                ))?;
                all_results[*i] = Some((
                    AssistantPart::ToolCall(ToolCall {
                        tool_call_id: tc.tool_call_id.clone(),
                        tool_name: tc.tool_name.clone(),
                        args: tc.args.clone(),
                    }),
                    ToolResult {
                        kind: ToolResultKind::ToolResult,
                        tool_call_id: tc.tool_call_id.clone(),
                        result: serde_json::Value::String(output),
                    },
                ));
                continue;
            }

            let signal = state
                .tools
                .iter()
                .find(|t| t.name() == tc.tool_name)
                .map(|t| t.execution_signal())
                .unwrap_or(ExecutionSignal::Continue);

            match signal {
                ExecutionSignal::LoadContext => {
                    // load: server-granularity enable or disable.
                    // item_name is a server name for MCPs, a skill name for skills.
                    if let (Some(item_name), Some(on)) = (
                        tc.args.get("name").and_then(|v| v.as_str()),
                        tc.args.get("enabled").and_then(|v| v.as_bool()),
                    ) {
                        // Phase 3 (scoped registry) enforcement: only mutate
                        // state / persist a Toggle for a name within the
                        // already-visible-set-gated `state.entries`
                        // (finish_after_resume's catalog gate). A name
                        // outside every whitelist is a no-op here — `TurnEnv`
                        // doesn't carry `agent_whitelist` and shouldn't need
                        // to; `state.entries` already *is* the resolved
                        // visible set, so re-deriving it here would just
                        // duplicate that logic. Note this doesn't stop
                        // `LoadTool::execute()` (below) from returning content
                        // for a not-visible name if it happens to still be in
                        // the full mcp/skill/plugin registries passed to it —
                        // a known, narrower-than-ideal scope; see the Phase 3
                        // plan notes.
                        let visible = state.entries.iter().any(|e| e.name == item_name);
                        if visible {
                            if on {
                                // MCP: push all tools from this server (server-granularity)
                                for def in env
                                    .mcp_registry
                                    .iter()
                                    .filter(|d| d.server_name == item_name)
                                {
                                    if let Some(client) = env.mcp_clients.get(&def.server_name) {
                                        if !state.tools.iter().any(|t| t.name() == def.name) {
                                            // duplicate guard
                                            state.tools.push(super::prompt::make_mcp_tool_def(
                                                def,
                                                client.clone(),
                                            ));
                                        }
                                    }
                                }
                                // Skill: no tools push needed — content is returned by LoadTool.execute()
                            } else {
                                // Remove all tools from this server
                                state.tools.retain(|t| {
                                    env.mcp_registry
                                        .iter()
                                        .find(|d| d.name == t.name())
                                        .map(|d| d.server_name != item_name)
                                        .unwrap_or(true)
                                });
                            }
                            let toggle_kind =
                                if env.mcp_registry.iter().any(|d| d.server_name == item_name) {
                                    "mcp"
                                } else if env.plugin_registry.iter().any(|d| d.name == item_name) {
                                    "plugin"
                                } else {
                                    "skill"
                                };
                            env.emitter
                                .record(events::toggle(toggle_kind, item_name, on))?;
                        }
                    }
                    let prefix = tool_approval_prefixes.get(&tc.tool_call_id).cloned();
                    let mut exec = execute_tool_call(
                        tc,
                        &state.tools,
                        env.emitter,
                        prefix,
                        env.codebase_path,
                        env.gait_dir,
                        env.session_id,
                        env.sandbox_on,
                        env.relay,
                    )
                    .await
                    .unwrap_or_else(|e| error_tool_result(tc, e));
                    {
                        let raw_result = exec.result.result.as_str().unwrap_or("").to_string();
                        let post_tool_ctx = HookContext {
                            event: "post_tool".to_string(),
                            session_id: env.session_id.to_string(),
                            gait_dir: env.gait_dir.to_path_buf(),
                            codebase: env.codebase_path.to_path_buf(),
                            system_prompt: env.system_prompt.to_string(),
                            step_index,
                            tool_name: tc.tool_name.clone(),
                            tool_call_id: tc.tool_call_id.clone(),
                            tool_args: tc.args.clone(),
                            tool_result: raw_result,
                            ..Default::default()
                        };
                        apply_output_filter(
                            &mut exec,
                            tc,
                            env.hooks,
                            env.emitter,
                            &post_tool_ctx,
                            env.model,
                            env.provider.clone(),
                        )
                        .await;
                    }
                    // Task 6: seal denial in supervised mode → pause for unlock.
                    if !env.auto && env.sandbox_on {
                        if let Some(denial) = exec.denied.take() {
                            return Ok(StepOutcome::Pause(PauseKind::SandboxUnlock(vec![denial])));
                        }
                    }
                    all_results[*i] = Some((exec.part, exec.result));
                }
                ExecutionSignal::FinishTurn => {
                    let message = tc
                        .args
                        .get("message")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let result = tc.args.get("result").cloned();

                    // Validate result against schema if configured.
                    if let Some(schema) = env.result_schema {
                        let value_to_validate = result.clone().unwrap_or(serde_json::Value::Null);
                        if let Err(errors) = validate_against_schema(schema, &value_to_validate) {
                            if state.schema_retries_remaining > 0 {
                                state.schema_retries_remaining -= 1;
                                env.emitter.record(events::tool_call(
                                    tc.tool_call_id.clone(),
                                    tc.tool_name.clone(),
                                    &tc.args,
                                ))?;
                                let error_msg = format!(
                                    "result schema validation failed ({} retries remaining):\n{}",
                                    state.schema_retries_remaining, errors
                                );
                                env.emitter.record(events::tool_result(
                                    tc.tool_call_id.clone(),
                                    error_msg,
                                    None,
                                    None,
                                ))?;
                                return Ok(StepOutcome::Continue);
                            } else {
                                anyhow::bail!(
                                    "result schema validation failed after all retries:\n{}",
                                    errors
                                );
                            }
                        }
                    }

                    // Emit call+result pair for transcript auditability before ending the turn.
                    env.emitter.record(events::tool_call(
                        tc.tool_call_id.clone(),
                        tc.tool_name.clone(),
                        &tc.args,
                    ))?;
                    env.emitter.record(events::tool_result(
                        tc.tool_call_id.clone(),
                        "recorded".to_string(),
                        None,
                        None,
                    ))?;
                    for t in post_tool_text.drain(..) {
                        env.emitter.publish_delta(t);
                    }
                    return Ok(StepOutcome::Done {
                        message,
                        result,
                        implicit: false,
                    });
                }
                ExecutionSignal::Clarify => {
                    let items: Vec<pb::ClarifyItem> = tc
                        .args
                        .get("items")
                        .and_then(|v| serde_json::from_value::<Vec<ClarifyArg>>(v.clone()).ok())
                        .unwrap_or_default()
                        .into_iter()
                        .map(|a| pb::ClarifyItem {
                            id: a.id,
                            content: a.content,
                            why: a.why,
                            kind: match a.kind.as_str() {
                                "is" => pb::ClarifyKind::Is as i32,
                                _ => pb::ClarifyKind::Ought as i32,
                            },
                            options: a.options,
                        })
                        .collect();
                    env.emitter.record(events::clarify(&items))?;
                    state.clarify_signal = Some(items);
                }
                ExecutionSignal::Inform => {
                    let items: Vec<pb::InformItem> = tc
                        .args
                        .get("items")
                        .and_then(|v| serde_json::from_value::<Vec<InformArg>>(v.clone()).ok())
                        .unwrap_or_default()
                        .into_iter()
                        .map(|a| pb::InformItem {
                            id: a.id,
                            content: a.content,
                            context: a.context,
                        })
                        .collect();
                    env.emitter.record(events::inform(&items))?;
                    state.inform_signal = Some(items);
                }
                ExecutionSignal::Continue => {
                    // skill (default) and any other serial tool
                    let prefix = tool_approval_prefixes.get(&tc.tool_call_id).cloned();
                    let mut exec = execute_tool_call(
                        tc,
                        &state.tools,
                        env.emitter,
                        prefix,
                        env.codebase_path,
                        env.gait_dir,
                        env.session_id,
                        env.sandbox_on,
                        env.relay,
                    )
                    .await
                    .unwrap_or_else(|e| error_tool_result(tc, e));
                    {
                        let raw_result = exec.result.result.as_str().unwrap_or("").to_string();
                        let post_tool_ctx = HookContext {
                            event: "post_tool".to_string(),
                            session_id: env.session_id.to_string(),
                            gait_dir: env.gait_dir.to_path_buf(),
                            codebase: env.codebase_path.to_path_buf(),
                            system_prompt: env.system_prompt.to_string(),
                            step_index,
                            tool_name: tc.tool_name.clone(),
                            tool_call_id: tc.tool_call_id.clone(),
                            tool_args: tc.args.clone(),
                            tool_result: raw_result,
                            ..Default::default()
                        };
                        apply_output_filter(
                            &mut exec,
                            tc,
                            env.hooks,
                            env.emitter,
                            &post_tool_ctx,
                            env.model,
                            env.provider.clone(),
                        )
                        .await;
                    }
                    // Task 6: seal denial in supervised mode → pause for unlock.
                    if !env.auto && env.sandbox_on {
                        if let Some(denial) = exec.denied.take() {
                            return Ok(StepOutcome::Pause(PauseKind::SandboxUnlock(vec![denial])));
                        }
                    }
                    all_results[*i] = Some((exec.part, exec.result));
                }
            }
        }

        for (part, result) in all_results.into_iter().flatten() {
            assistant_parts.push(part);
            tool_results.push(result);
        }
    }

    // Emit any model text that arrived after tool calls in this step.
    for t in post_tool_text.drain(..) {
        env.emitter.publish_delta(t);
    }

    // Gated tools: emit Permissions and exit so the user can decide.
    // (In auto mode gated_ids is always empty — this block is never reached.)
    if !pending_permissions.is_empty() {
        attach_near_miss_hints(&mut pending_permissions, env.effective_allow);
        env.emitter
            .record(events::permissions(&pending_permissions))?;
        return Ok(StepOutcome::Pause(PauseKind::Permissions));
    }

    // Exit turn for clarify/inform — event already in transcript+stdout via emit_event
    if let Some(items) = state.clarify_signal.take() {
        return Ok(StepOutcome::Pause(PauseKind::Clarify(items)));
    }
    if let Some(items) = state.inform_signal.take() {
        return Ok(StepOutcome::Pause(PauseKind::Inform(items)));
    }

    // Append this step to messages for next iteration
    state.messages.push(Message::Assistant {
        content: AssistantContent::Parts(assistant_parts),
        id: String::new(),
    });
    state.messages.push(Message::ToolResult {
        content: tool_results,
        id: String::new(),
    });

    Ok(StepOutcome::Continue)
}

/// Appends a near-miss `--allow` suggestion to each item's `reason` when the
/// gated command almost matches an existing allow pattern (e.g. missing the
/// trailing space before `*`). Mutates in place so the hint rides the same
/// `PermissionItem` event already sent over `env.emitter` — no separate
/// stderr print (which used to race the REPL's status-line timer thread),
/// no new proto field.
fn attach_near_miss_hints(items: &mut [pb::PermissionItem], allow: &[String]) {
    for item in items.iter_mut() {
        if let Some(fix) = crate::permissions::matching::near_miss_suggestion(&item.cmd, allow) {
            item.reason = format!(
                "{} (hint: patterns match as \"<cmd> *\" — did you mean --allow {:?}?)",
                item.reason, fix
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(cmd: &str) -> pb::PermissionItem {
        pb::PermissionItem {
            id: "p1".into(),
            cmd: cmd.into(),
            reason: format!("{cmd} requires permission"),
            patterns: vec![cmd.into()],
            tool_name: "shell".into(),
            tool_args: None,
            preview: None,
        }
    }

    #[test]
    fn attach_near_miss_hints_appends_suggestion_when_near_miss_exists() {
        let mut items = vec![item("rm -rf /tmp/x")];
        attach_near_miss_hints(&mut items, &["shell:rm*".to_string()]);
        assert!(
            items[0].reason.contains("did you mean --allow"),
            "reason was: {}",
            items[0].reason
        );
        assert!(items[0].reason.contains("shell:rm *"));
    }

    #[test]
    fn attach_near_miss_hints_leaves_reason_untouched_when_no_near_miss() {
        let mut items = vec![item("curl https://evil")];
        let original = items[0].reason.clone();
        attach_near_miss_hints(&mut items, &["shell:ls *".to_string()]);
        assert_eq!(items[0].reason, original);
    }

    #[test]
    fn attach_near_miss_hints_handles_empty_item_list() {
        let mut items: Vec<pb::PermissionItem> = vec![];
        attach_near_miss_hints(&mut items, &["shell:rm *".to_string()]);
        assert!(items.is_empty());
    }
}

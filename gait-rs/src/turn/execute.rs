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
    Message, Tool, ToolCall, ToolPermission, ToolResult, ToolResultKind,
};

use super::exec::{
    apply_output_filter, error_tool_result, execute_tool_call, stream_preview_if_always,
};
use super::finalize::{PauseKind, StepOutcome};
use super::persist::Persistence;
use super::state::{TurnEnv, TurnState};
use super::{validate_against_schema, ClarifyArg, InformArg};
use crate::permissions::find_matched_deny_pattern;

/// Injectable boundary around the external classifier. Tests use a fake;
/// production resolves the named inference profile here, at the turn layer.
#[async_trait::async_trait]
pub(crate) trait Classifier: Send + Sync {
    async fn classify(
        &self,
        inference: &str,
        state: &str,
    ) -> Result<gait_provider::classify::RiskScores, gait_provider::classify::ClassifyError>;
}

pub(crate) struct ProductionClassifier {
    pub gait_dir: std::path::PathBuf,
}

pub(crate) struct DisabledClassifier;

fn classify_approves(scores: &gait_provider::classify::RiskScores, threshold: f64) -> bool {
    // noul: 0 is confident safe, 1 is confident risk.
    scores.confidentiality_risk < threshold
        && scores.integrity_risk < threshold
        && scores.availability_risk < threshold
}

#[async_trait::async_trait]
impl Classifier for DisabledClassifier {
    async fn classify(
        &self,
        _inference: &str,
        _state: &str,
    ) -> Result<gait_provider::classify::RiskScores, gait_provider::classify::ClassifyError> {
        Err(gait_provider::classify::ClassifyError::BadModelString(
            "disabled".into(),
        ))
    }
}

#[async_trait::async_trait]
impl Classifier for ProductionClassifier {
    async fn classify(
        &self,
        inference: &str,
        state: &str,
    ) -> Result<gait_provider::classify::RiskScores, gait_provider::classify::ClassifyError> {
        let profile = gait_state::inferences::load_inference(&self.gait_dir, inference)
            .map_err(|e| gait_provider::classify::ClassifyError::BadModelString(e.to_string()))?;
        gait_provider::classify::classify(&profile.model, state).await
    }
}

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
                // All-or-nothing per tool_call_id (every item here shares the
                // same id -- see resume.rs's "all segments must be approved"
                // convention). Classify every item first, without emitting
                // anything or touching the streak, so a partial-batch failure
                // (some items classified, one call errored) can't leave
                // already-decided items dropped -- neither executed nor
                // re-surfaced for a human. Only commit to an outcome once
                // every item in the batch got a real answer.
                let mut resolved = false;
                if let Some(policy) = env.classify_policy {
                    if state.classify_denial_streak < policy.max_consecutive_denials {
                        let mut scored: Vec<(
                            pb::PermissionItem,
                            gait_provider::classify::RiskScores,
                        )> = Vec::with_capacity(items.len());
                        let mut call_failed = false;
                        for item in &items {
                            let state_text = format!(
                                "{}: {}\n\npatterns: {}",
                                tc.tool_name,
                                tc.args,
                                item.patterns.join(", ")
                            );
                            match env
                                .classifier
                                .classify(&policy.inference, &state_text)
                                .await
                            {
                                Ok(scores) => scored.push((item.clone(), scores)),
                                Err(_) => {
                                    call_failed = true;
                                    break;
                                }
                            }
                        }
                        if !call_failed {
                            let tc_approved = scored
                                .iter()
                                .all(|(_, s)| classify_approves(s, policy.risk_threshold));
                            for (item, scores) in &scored {
                                env.emitter.record(events::classify_decision(
                                    pb::ClassifyDecision {
                                        permission_item_id: item.id.clone(),
                                        tool_name: item.tool_name.clone(),
                                        cmd: item.cmd.clone(),
                                        approved: tc_approved,
                                        confidentiality_risk: scores.confidentiality_risk,
                                        integrity_risk: scores.integrity_risk,
                                        availability_risk: scores.availability_risk,
                                        inference: policy.inference.clone(),
                                    },
                                ))?;
                            }
                            if tc_approved {
                                state.classify_denial_streak = 0;
                                gated_ids.remove(&tc.tool_call_id);
                                tool_approval_prefixes.insert(
                                    tc.tool_call_id.clone(),
                                    "[Classifier-approved]".into(),
                                );
                            } else {
                                state.classify_denial_streak =
                                    state.classify_denial_streak.saturating_add(1);
                                gated_ids.remove(&tc.tool_call_id);
                                denied_ids.insert(tc.tool_call_id.clone());
                            }
                            resolved = true;
                        }
                        // call_failed: no ClassifyDecision emitted (no decision was
                        // made), streak untouched, falls through below unresolved.
                    }
                }
                if !resolved {
                    pending_permissions.extend(items);
                }
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
            let mut exec = match raw {
                Ok(exec) => exec,
                Err(error) => error_tool_result(tc, error, env.emitter)?,
            };
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
                let output = format!("[Blocked by hook] {reason}");
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
                                // Skill: no tools push needed — content is returned by LoadTool.execute()
                            } else {
                                // MCP façades are not provider-native tools.
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
                            if toggle_kind == "mcp" {
                                state.capability_dispatcher.set_enabled(item_name, on);
                            }
                        }
                    }
                    let prefix = tool_approval_prefixes.get(&tc.tool_call_id).cloned();
                    let mut exec = match execute_tool_call(
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
                    {
                        Ok(exec) => exec,
                        Err(error) => error_tool_result(tc, error, env.emitter)?,
                    };
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

                    // Emit the structured_done tool call for transcript auditability.
                    env.emitter.record(events::tool_call(
                        tc.tool_call_id.clone(),
                        tc.tool_name.clone(),
                        &tc.args,
                    ))?;

                    // Publish any post-tool text before the completion gate
                    for t in post_tool_text.drain(..) {
                        env.emitter.publish_delta(t);
                    }

                    // Route through the shared completion gate
                    return verify_completion(
                        &env,
                        &mut *state,
                        &persist,
                        step_index,
                        message,
                        result,
                        false, // explicit structured_done, not implicit
                        tc.tool_call_id.clone(),
                    )
                    .await;
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
                    let mut exec = match execute_tool_call(
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
                    {
                        Ok(exec) => exec,
                        Err(error) => error_tool_result(tc, error, env.emitter)?,
                    };
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

/// Shared completion gate: check structured-result validity and verifier status,
/// building one combined feedback response before continuing or completing the turn.
///
/// Returns:
/// - `Ok(StepOutcome::Done)` when both checks pass (or when disabled/not configured)
/// - `Ok(StepOutcome::Continue)` when any check fails, with combined feedback in transcript
/// - `Ok(StepOutcome::Pause(...))` when permission/sandbox approval is needed
/// - `Err(...)` when retries exhausted
#[allow(clippy::too_many_arguments)]
pub(crate) async fn verify_completion(
    env: &TurnEnv<'_>,
    state: &mut TurnState,
    _persist: &Persistence<'_>,
    _step_index: usize,
    completion_message: String,
    completion_result: Option<serde_json::Value>,
    completion_implicit: bool,
    tool_call_id: String,
) -> anyhow::Result<StepOutcome> {
    let mut feedback_messages = Vec::new();
    let mut schema_failure = false;
    let mut verifier_failure = false;
    let mut execution_error = false;

    // Step 1-2: Check structured-result validity and implicit-without-schema requirement
    // When schema is required and we have implicit completion, that's a special case
    if env.result_schema.is_some() && completion_implicit {
        // Plain-text completion when schema is required - emit the nudge message
        schema_failure = true;
        feedback_messages.push(
            "You must finish by calling `structured_done` with a `result` matching \
             the declared schema. Plain-text completion is not accepted for this task."
                .to_string(),
        );
    } else if let Some(schema) = env.result_schema {
        // Schema is required and completion is explicit (structured_done) - validate the result
        let value_to_validate = completion_result.clone().unwrap_or(serde_json::Value::Null);
        if let Err(errors) = validate_against_schema(schema, &value_to_validate) {
            schema_failure = true;
            feedback_messages.push(format!("result schema validation failed:\n{}", errors));
        }
    }

    // Step 3: Run verifier if configured
    let mut verifier_output = String::new();
    if let Some(ref verified_goal) = state.verified_goal {
        // Increment attempt counter and generate attempt ID
        state.verifier_attempt_counter += 1;
        let attempt_id = format!("attempt-{}", state.verifier_attempt_counter);

        // Find the shell tool, or create one if not found
        let shell_tool = state
            .tools
            .iter()
            .find(|t| t.name() == "shell")
            .cloned()
            .or_else(|| {
                // If shell tool is not in state.tools (e.g., in tests with minimal setup),
                // create one on-the-fly using the turn's context
                use crate::tools::{ShellTool, ToolCtx};
                Some(
                    ShellTool::new(ToolCtx::new(
                        env.gait_dir.to_path_buf(),
                        env.codebase_path.to_path_buf(),
                        env.session_id.to_string(),
                    ))
                    .arced(),
                )
            });

        if let Some(shell_tool) = shell_tool {
            // Dispatch the verifier through the normal permission gate
            match super::exec::dispatch_verifier(
                verified_goal,
                shell_tool.as_ref(),
                env.emitter,
                env.relay,
                env.effective_allow,
                env.deny,
                env.auto,
                env.classifier,
                env.classify_policy,
            )
            .await
            {
                Ok(super::exec::VerifierDispatchOutcome::Executed(exec_result)) => {
                    verifier_output = exec_result.output.clone();

                    // Record the verification attempt
                    let completion = serde_json::json!({
                        "message": completion_message,
                        "result": completion_result,
                        "implicit": completion_implicit,
                    });

                    let status = if exec_result.exit_code == Some(0) {
                        "passed"
                    } else {
                        "failed"
                    };

                    env.emitter.record(events::verified_goal(
                        &attempt_id,
                        status,
                        verified_goal,
                        exec_result.exit_code,
                        &exec_result.output,
                        completion,
                    ))?;

                    // Check for verifier failure
                    if exec_result.exit_code != Some(0) {
                        verifier_failure = true;
                        feedback_messages.push(format!(
                            "verified_goal command failed:\nCommand: {}\nStatus: exit code {}\nOutput:\n{}",
                            verified_goal,
                            exec_result.exit_code.unwrap_or(-1),
                            verifier_output
                        ));
                    }
                }
                Ok(super::exec::VerifierDispatchOutcome::NeedsPermission(items)) => {
                    // Store pending completion for resumption after approval
                    state.pending_verifier_attempt = Some(super::exec::PendingVerifierAttempt {
                        attempt_id: attempt_id.clone(),
                        command: verified_goal.clone(),
                        completion_message: completion_message.clone(),
                        completion_result: completion_result.clone(),
                        completion_implicit,
                        schema_diagnostics: if schema_failure {
                            Some(feedback_messages.join("\n"))
                        } else {
                            None
                        },
                        pause_kind: "permission".to_string(),
                        schema_retries_remaining: state.schema_retries_remaining,
                        done_nudges_remaining: state.done_nudges_remaining,
                        verified_goal_retries_remaining: state.verified_goal_retries_remaining,
                    });

                    // Emit pending verified_goal event
                    let completion = serde_json::json!({
                        "message": completion_message,
                        "result": completion_result,
                        "implicit": completion_implicit,
                    });

                    env.emitter.record(events::verified_goal(
                        &attempt_id,
                        "pending",
                        verified_goal,
                        None,
                        "",
                        completion,
                    ))?;

                    // Record permissions and pause
                    let mut pending = items;
                    attach_near_miss_hints(&mut pending, env.effective_allow);
                    env.emitter.record(events::permissions(&pending))?;
                    return Ok(StepOutcome::Pause(PauseKind::Permissions));
                }
                Ok(super::exec::VerifierDispatchOutcome::NeedsSandboxUnlock(denial)) => {
                    // Store pending completion for resumption after sandbox unlock
                    state.pending_verifier_attempt = Some(super::exec::PendingVerifierAttempt {
                        attempt_id: attempt_id.clone(),
                        command: verified_goal.clone(),
                        completion_message: completion_message.clone(),
                        completion_result: completion_result.clone(),
                        completion_implicit,
                        schema_diagnostics: if schema_failure {
                            Some(feedback_messages.join("\n"))
                        } else {
                            None
                        },
                        pause_kind: "sandbox".to_string(),
                        schema_retries_remaining: state.schema_retries_remaining,
                        done_nudges_remaining: state.done_nudges_remaining,
                        verified_goal_retries_remaining: state.verified_goal_retries_remaining,
                    });

                    // Emit pending verified_goal event
                    let completion = serde_json::json!({
                        "message": completion_message,
                        "result": completion_result,
                        "implicit": completion_implicit,
                    });

                    env.emitter.record(events::verified_goal(
                        &attempt_id,
                        "pending",
                        verified_goal,
                        None,
                        "",
                        completion,
                    ))?;

                    return Ok(StepOutcome::Pause(PauseKind::SandboxUnlock(vec![denial])));
                }
                Err(e) => {
                    // Dispatch error becomes execution_error
                    execution_error = true;
                    verifier_failure = true;
                    feedback_messages.push(format!("verified_goal dispatch error: {}", e));

                    // Record the execution error
                    let completion = serde_json::json!({
                        "message": completion_message,
                        "result": completion_result,
                        "implicit": completion_implicit,
                    });

                    env.emitter.record(events::verified_goal(
                        &attempt_id,
                        "execution_error",
                        verified_goal,
                        None,
                        &format!("dispatch error: {}", e),
                        completion,
                    ))?;
                }
            }
        }
    }

    // Step 4: Check retries before decrementing
    let mut has_retries = true;
    if schema_failure && state.schema_retries_remaining == 0 {
        has_retries = false;
    }
    if verifier_failure && state.verified_goal_retries_remaining == 0 {
        has_retries = false;
    }

    // Step 5: If both checks pass, complete successfully
    if !schema_failure && !verifier_failure {
        // Emit the successful tool_result only for explicit (structured_done) completions
        // For implicit completions without schema, no tool result was ever called, so don't emit one
        if tool_call_id != "implicit_done" {
            let recorded = "recorded".to_string();
            env.emitter.record(events::tool_result(
                tool_call_id.clone(),
                recorded,
                None,
                None,
            ))?;
        }
        return Ok(StepOutcome::Done {
            message: completion_message,
            result: completion_result,
            implicit: completion_implicit,
        });
    }

    // Step 6: If any check failed and we're out of retries, fail the turn
    if !has_retries {
        let error_msg = feedback_messages.join("\n");
        anyhow::bail!(error_msg);
    }

    // Step 7: Decrement applicable retry counters
    if schema_failure {
        state.schema_retries_remaining = state.schema_retries_remaining.saturating_sub(1);
    }
    if verifier_failure && !execution_error {
        state.verified_goal_retries_remaining =
            state.verified_goal_retries_remaining.saturating_sub(1);
    } else if verifier_failure && execution_error {
        // Execution error also counts against verified_goal retries
        state.verified_goal_retries_remaining =
            state.verified_goal_retries_remaining.saturating_sub(1);
    }
    if env.result_schema.is_some() && completion_implicit && !schema_failure {
        // Only decrement done_nudges if we had the structured_done requirement failure
        // (not already counted in schema_failure)
    }

    // Step 8: Send combined feedback and continue turn
    let feedback = feedback_messages.join("\n\n");

    // For implicit completion, emit UserText feedback; for explicit completion, emit ToolResult
    if tool_call_id == "implicit_done" {
        env.emitter.record(events::user_text(feedback))?;
    } else {
        env.emitter.record(events::tool_result(
            tool_call_id.clone(),
            feedback,
            None,
            None,
        ))?;
    }

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

#[cfg(test)]
mod classify_gate_tests {
    use super::*;
    use crate::git::checkpoint::MockCheckpointRepo;
    use crate::git::Repos;
    use crate::session::Ingestor;
    use crate::session::Session;
    use gait_provider::classify::{ClassifyError, RiskScores};
    use gait_provider::provider::{ChunkStream, Provider, ProviderError, StreamChunk};
    use std::sync::{Arc, Mutex};

    struct NoopTrace;
    impl crate::git::trace::TraceRepo for NoopTrace {
        fn init_workspace(&self, _: &std::path::Path) -> anyhow::Result<()> {
            Ok(())
        }
        fn create_session_worktree(&self, _: &std::path::Path, _: &str) -> anyhow::Result<()> {
            Ok(())
        }
        fn commit_session(&self, _: &std::path::Path, _: &str, _: &str) -> anyhow::Result<()> {
            Ok(())
        }
        fn list_active_sessions(&self, _: &std::path::Path) -> anyhow::Result<Vec<String>> {
            Ok(vec![])
        }
        fn finish_session(&self, _: &std::path::Path, _: &str) -> anyhow::Result<()> {
            Ok(())
        }
        fn create_branch_worktree(
            &self,
            _: &std::path::Path,
            _: &str,
            _: &str,
            _: &std::path::Path,
        ) -> anyhow::Result<()> {
            Ok(())
        }
        fn remove_branch_worktree(
            &self,
            _: &std::path::Path,
            _: &std::path::Path,
        ) -> anyhow::Result<()> {
            Ok(())
        }
        fn merge_branch_ff(&self, _: &std::path::Path, _: &str, _: &str) -> anyhow::Result<()> {
            Ok(())
        }
        fn discard_session(&self, _: &std::path::Path, _: &str) -> anyhow::Result<()> {
            Ok(())
        }
    }

    struct NoopProvider;
    #[async_trait::async_trait]
    impl Provider for NoopProvider {
        fn model_id(&self) -> &str {
            "noop"
        }
        async fn stream(
            &self,
            _system: &str,
            _messages: &[crate::types::Message],
            _tools: &[Arc<dyn crate::types::Tool>],
            _params: &crate::config::LlmParamsConfig,
            _tail_context: Option<&str>,
        ) -> Result<ChunkStream, ProviderError> {
            let _ = StreamChunk::Done;
            Err(ProviderError::Other("unused in classify_gate tests".into()))
        }
    }

    /// A fixed-answer or always-erroring fake, counting how many calls it took.
    struct FakeClassifier {
        scores: Option<RiskScores>,
        calls: Mutex<u32>,
    }
    impl FakeClassifier {
        fn approving() -> Self {
            Self {
                scores: Some(RiskScores {
                    confidentiality_risk: 0.02,
                    integrity_risk: 0.01,
                    availability_risk: 0.01,
                }),
                calls: Mutex::new(0),
            }
        }
        fn denying() -> Self {
            Self {
                scores: Some(RiskScores {
                    confidentiality_risk: 0.02,
                    integrity_risk: 0.87,
                    availability_risk: 0.01,
                }),
                calls: Mutex::new(0),
            }
        }
        fn call_count(&self) -> u32 {
            *self.calls.lock().unwrap()
        }
    }
    #[async_trait::async_trait]
    impl Classifier for FakeClassifier {
        async fn classify(
            &self,
            _inference: &str,
            _state: &str,
        ) -> Result<RiskScores, ClassifyError> {
            *self.calls.lock().unwrap() += 1;
            match &self.scores {
                Some(s) => Ok(RiskScores {
                    confidentiality_risk: s.confidentiality_risk,
                    integrity_risk: s.integrity_risk,
                    availability_risk: s.availability_risk,
                }),
                None => Err(ClassifyError::BadModelString("fake-disabled".into())),
            }
        }
    }

    struct Fixture {
        _ws: tempfile::TempDir,
        session: Session,
        emitter: crate::Recorder,
        ingestor: Ingestor,
        provider: Arc<dyn Provider>,
        hooks: Vec<Box<dyn crate::types::Hook>>,
        sandbox_cfg: crate::config::SandboxConfig,
        deny: Vec<String>,
        daemons: Vec<(String, i32, bool, String)>,
        system_prompt: String,
        codebase_hash: String,
        llm_params: crate::config::LlmParamsConfig,
        effective_allow: Vec<String>,
        model: String,
        mcp_registry: Vec<crate::config::McpToolDef>,
        mcp_clients: crate::profile::McpClients,
        plugin_registry: Vec<crate::config::WasmPluginDef>,
        result_schema: Option<serde_json::Value>,
    }

    fn fixture() -> Fixture {
        let ws = tempfile::TempDir::new().unwrap();
        let session_id = "sclassify01".to_string();
        let repos = Repos {
            codebase: Arc::new(crate::git::codebase::GitCodebaseRepo),
            traces: Arc::new(NoopTrace),
            checkpoints: Arc::new(MockCheckpointRepo::new()),
        };
        let session = Session {
            id: session_id.clone(),
            gait_dir: ws.path().to_path_buf(),
            codebase_path: ws.path().to_path_buf(),
            assigned_by: None,
            repos,
        };
        Fixture {
            _ws: ws,
            session,
            emitter: crate::Recorder::buffering(),
            ingestor: Ingestor::from_shared(Arc::new(Mutex::new(Vec::new()))),
            provider: Arc::new(NoopProvider),
            hooks: vec![],
            sandbox_cfg: crate::config::SandboxConfig::default(),
            deny: vec![],
            daemons: vec![],
            system_prompt: "sys".to_string(),
            codebase_hash: "deadbeef".to_string(),
            llm_params: crate::config::LlmParamsConfig::default(),
            effective_allow: vec![],
            model: "test/model".to_string(),
            mcp_registry: vec![],
            mcp_clients: Default::default(),
            plugin_registry: vec![],
            result_schema: None,
        }
    }

    fn env_from<'a>(
        fx: &'a Fixture,
        classify_policy: Option<&'a pb::ClassifyPolicy>,
        classifier: &'a dyn Classifier,
    ) -> TurnEnv<'a> {
        TurnEnv {
            session: &fx.session,
            emitter: &fx.emitter,
            repos: &fx.session.repos,
            gait_dir: &fx.session.gait_dir,
            codebase_path: &fx.session.codebase_path,
            session_id: &fx.session.id,
            provider: &fx.provider,
            hooks: &fx.hooks,
            sandbox_cfg: &fx.sandbox_cfg,
            sandbox_on: false,
            cancel: None,
            deny: &fx.deny,
            preview_policy: crate::config::PreviewPolicy::default(),
            auto: false,
            daemons: &fx.daemons,
            ingestor: &fx.ingestor,
            system_prompt: &fx.system_prompt,
            codebase_hash: &fx.codebase_hash,
            codebase_dirty: false,
            llm_params: &fx.llm_params,
            context_policy: crate::profile::ContextPolicy::TEST,
            effective_allow: &fx.effective_allow,
            classify_policy,
            classifier,
            relay: None,
            model: &fx.model,
            mcp_registry: &fx.mcp_registry,
            mcp_clients: &fx.mcp_clients,
            plugin_registry: &fx.plugin_registry,
            result_schema: &fx.result_schema,
        }
    }

    fn unsealed<'a>(fx: &'a Fixture) -> Persistence<'a> {
        Persistence::Unsealed {
            repos: &fx.session.repos,
            gait_dir: &fx.session.gait_dir,
            session_id: &fx.session.id,
            codebase_path: &fx.session.codebase_path,
        }
    }

    fn bare_state() -> TurnState {
        TurnState {
            classify_denial_streak: 0,
            capability_dispatcher: Arc::new(super::super::capabilities::CapabilityDispatcher::new(
                std::iter::empty::<Arc<dyn crate::types::Tool>>(),
                Vec::new(),
                std::collections::HashMap::new(),
                std::iter::empty::<String>(),
                None,
                Vec::new(),
            )),
            total_usage: crate::types::Usage::default(),
            clarify_signal: None,
            inform_signal: None,
            all_turn_text: String::new(),
            final_step_index: 0,
            messages: Vec::new(),
            turn_ctx: None,
            schema_retries_remaining: 0,
            done_nudges_remaining: 0,
            verified_goal_retries_remaining: 0,
            tools: vec![Arc::new(crate::types::ToolDef {
                name: "test_tool".into(),
                description: "a no-op test tool".into(),
                parameters: serde_json::json!({"type": "object"}),
                execute: Box::new(|_args| {
                    Box::pin(async move { Ok(serde_json::Value::String("ok".into())) })
                }),
                permission: None, // defaults to RequiresApproval([name]) -- always hits Tier 3
            })],
            entries: Vec::new(),
            verified_goal: None,
            pending_verifier_attempt: None,
            verifier_attempt_counter: 0,
        }
    }

    fn gate_tool_call() -> ToolCall {
        ToolCall {
            tool_call_id: "tc1".into(),
            tool_name: "test_tool".into(),
            args: serde_json::json!({}),
        }
    }

    fn policy(risk_threshold: f64, max_consecutive_denials: u32) -> pb::ClassifyPolicy {
        pb::ClassifyPolicy {
            inference: "fake".into(),
            risk_threshold,
            max_consecutive_denials,
        }
    }

    #[tokio::test]
    async fn classify_approve_executes_without_pausing() {
        let fx = fixture();
        let fake = FakeClassifier::approving();
        let policy = policy(0.3, 3);
        let env = env_from(&fx, Some(&policy), &fake);
        let mut state = bare_state();
        let persist = unsealed(&fx);

        let outcome = run_tools(
            &env,
            &mut state,
            &persist,
            0,
            String::new(),
            vec![gate_tool_call()],
            vec![],
        )
        .await
        .unwrap();

        assert!(
            matches!(outcome, StepOutcome::Continue),
            "an approved tool call must not pause the turn"
        );
        assert_eq!(state.classify_denial_streak, 0);
        assert_eq!(fake.call_count(), 1);
        let events = fx.emitter.take_buffer();
        let decision = events.iter().find_map(|e| match &e.body {
            Some(crate::proto::gait::log::v1::event::Body::Payload(p)) => match &p.kind {
                Some(crate::proto::gait::log::v1::payload::Kind::ClassifyDecision(d)) => {
                    Some(d.clone())
                }
                _ => None,
            },
            _ => None,
        });
        let decision = decision.expect("a ClassifyDecision event must be recorded");
        assert!(decision.approved);
    }

    #[tokio::test]
    async fn classify_deny_blocks_without_pausing() {
        let fx = fixture();
        let fake = FakeClassifier::denying();
        let policy = policy(0.3, 3);
        let env = env_from(&fx, Some(&policy), &fake);
        let mut state = bare_state();
        let persist = unsealed(&fx);

        let outcome = run_tools(
            &env,
            &mut state,
            &persist,
            0,
            String::new(),
            vec![gate_tool_call()],
            vec![],
        )
        .await
        .unwrap();

        assert!(
            matches!(outcome, StepOutcome::Continue),
            "a classifier-denied tool call must not pause the turn either"
        );
        assert_eq!(
            state.classify_denial_streak, 1,
            "a denial must increment the streak"
        );
        let events = fx.emitter.take_buffer();
        let has_classify_decision = events.iter().any(|e| {
            matches!(
                &e.body,
                Some(crate::proto::gait::log::v1::event::Body::Payload(p))
                    if matches!(&p.kind, Some(crate::proto::gait::log::v1::payload::Kind::ClassifyDecision(d)) if !d.approved)
            )
        });
        assert!(
            has_classify_decision,
            "a denied ClassifyDecision event must be recorded"
        );
        let has_plain_denial_text = events.iter().any(|e| {
            matches!(
                &e.body,
                Some(crate::proto::gait::log::v1::event::Body::Payload(p))
                    if matches!(&p.kind, Some(crate::proto::gait::log::v1::payload::Kind::ToolResult(tr)) if tr.output == "permission denied")
            )
        });
        assert!(
            has_plain_denial_text,
            "the model-facing deny text must be the plain, unbracketed \"permission denied\" string"
        );
    }

    #[tokio::test]
    async fn circuit_breaker_stops_trusting_the_classifier_after_max_consecutive_denials() {
        let fx = fixture();
        let fake = FakeClassifier::denying();
        let policy = policy(0.3, 1); // trips after just 1 denial
        let env = env_from(&fx, Some(&policy), &fake);
        let mut state = bare_state();
        let persist = unsealed(&fx);

        // First call: classifier denies, streak becomes 1 (== max_consecutive_denials).
        run_tools(
            &env,
            &mut state,
            &persist,
            0,
            String::new(),
            vec![gate_tool_call()],
            vec![],
        )
        .await
        .unwrap();
        assert_eq!(state.classify_denial_streak, 1);
        assert_eq!(fake.call_count(), 1);

        // Second call: circuit breaker must skip the classifier entirely and
        // fall through to a normal pause instead.
        let outcome = run_tools(
            &env,
            &mut state,
            &persist,
            1,
            String::new(),
            vec![gate_tool_call()],
            vec![],
        )
        .await
        .unwrap();

        assert_eq!(
            fake.call_count(),
            1,
            "the circuit breaker must skip the classify call once tripped"
        );
        assert!(
            matches!(outcome, StepOutcome::Pause(_)),
            "once tripped, the item must fall through to a normal pause instead of \
             being silently continued, got a different StepOutcome variant"
        );
    }
}

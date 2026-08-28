#[allow(unused_imports)] // `Tool` is used by `#[cfg(test)]` in tests.rs
use crate::types::{
    AssistantContent, AssistantPart, ExecutionMode, ExecutionSignal, HookAction, HookContext,
    InjectRole, Message, MessageContent, MessageMutation, MutationPayload, Tool, ToolCall,
    ToolPermission, ToolResult, ToolResultKind, Usage,
};

use crate::proto::events;
use crate::proto::Recorder;
use crate::session::retrace::{
    active_mode, active_mutations, active_unlock_state, apply_cascade, coalesce, run_projection,
    ActiveRawMessages,
};

use crate::proto::gait::log::v1 as pb;
use crate::proto::gait::log::v1::{event, payload, Event};

// ── LLM arg parse structs (private) ──────────────────────────────────────
// These are the serde-shaped mirrors of the proto types, used to parse the
// raw JSON `tc.args` from the LLM tool call. The `pb::*` types are the canonical
// form on the wire; we translate via these thin structs at the parse boundary.
#[derive(serde::Deserialize)]
struct ClarifyArg {
    id: String,
    content: String,
    #[serde(default)]
    why: String,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    options: Vec<String>,
}

mod diag_sink;
pub(crate) use diag_sink::DiagnosticSink;
mod session_git;
pub(crate) use session_git::SessionGit;
mod session_log;
pub(crate) use session_log::SessionLog;
#[derive(serde::Deserialize)]
struct InformArg {
    id: String,
    content: String,
    #[serde(default)]
    context: Option<String>,
}

pub(crate) mod agent_event;
pub(crate) mod brain;
pub(crate) mod capabilities;
mod compaction;
mod completion_poll;
mod curate;
mod exec;
mod execute;
mod finalize;
pub(crate) mod hand;
pub mod nerve;
mod persist;
mod prompt;
mod resume;
pub(crate) mod run_lifecycle;
mod run_summary;
mod setup;
pub(crate) mod spine;
mod state;
mod stream;
pub(crate) mod synapse;

use persist::Persistence;

// Re-exports
#[allow(unused_imports)] // used by `#[cfg(test)]` in tests.rs
pub(crate) use exec::apply_parse_filter;
#[allow(unused_imports)]
// used by `#[cfg(test)]` in tests.rs; call site moved to resume.rs (Task 14)
pub(crate) use prompt::build_turn_context;
pub use prompt::{
    build_loadable_catalog, build_loadable_enabled_state, build_permission_status,
    build_session_state,
};

/// The substantive payload kind for a proto event, if it carries one.
pub(super) fn event_kind(e: &Event) -> Option<payload::Kind> {
    match &e.body {
        Some(event::Body::Payload(p)) => p.kind.clone(),
        _ => None,
    }
}

/// Proto tool-input `Struct` → `serde_json::Value` (object, or Null if unset).
fn input_value(s: &Option<pbjson_types::Struct>) -> serde_json::Value {
    s.as_ref()
        .map(|st| serde_json::to_value(st).unwrap_or(serde_json::Value::Null))
        .unwrap_or(serde_json::Value::Null)
}

#[derive(Debug, Clone)]
pub enum TurnResult {
    Done {
        message: String,
        result: Option<serde_json::Value>,
        changes: Option<crate::proto::gait::log::v1::ChangeSummary>,
        /// `true` = implicit completion (model emitted no tool calls);
        /// `false` = explicit `structured_done`/`output()`.
        implicit: bool,
    },
    Permissions,
    Clarify {
        items: Vec<pb::ClarifyItem>,
    },
    Inform {
        items: Vec<pb::InformItem>,
    },
    SandboxUnlock {
        items: Vec<pb::SandboxUnlockItem>,
    },
    Cancelled,
    /// A turn that ended in failure. `ended` is the typed reason
    /// ("max_steps" | "error"); `message` is the human-readable detail.
    /// The turn still counts as failed (exit 1) — this makes the *reason*
    /// legible. (Wired by Task 4; constructor lives in finalize.rs.)
    Failed {
        ended: String,
        message: String,
    },
}

/// Git state at the start of a turn — captured once by the spine (BrainInit)
/// and threaded in so the brain does not read git itself on the spine path.
#[derive(Clone, Default)]
pub struct GitSnapshot {
    pub head_hash: String,
    pub dirty_files: Vec<String>,
}

/// Per-call inputs for one turn (the things that vary every invocation).
#[derive(Clone)]
pub struct TurnInput {
    pub auto: bool,
    pub max_iter: Option<usize>,
    pub preview_policy: crate::config::PreviewPolicy,
    pub cancel: Option<tokio_util::sync::CancellationToken>,
    pub sandbox_on: bool,
    /// The sandbox config from gait.jsonc — used by the pre-check
    /// (`check_own_tool_scope`) to respect user-configured `readable`/`writable`
    /// entries instead of always using defaults.
    pub sandbox_cfg: crate::config::SandboxConfig,
    /// Phase 3 Task 4a: when Some, execute_tool_call uses RelaySynapse.
    /// None for direct runs (spine path sets this from LoopConfig).
    pub relay: Option<crate::turn::nerve::NerveWriter>,
    /// When `Some` (spine path), the turn-start git snapshot supplied by the spine
    /// via BrainInit — the brain does not read git itself. `None` reads disk.
    pub initial_git: Option<GitSnapshot>,
    /// Active daemons injected by the brain from BrainInit.daemons. Empty on direct runs.
    pub daemons: Vec<(String, i32, bool, String)>,
}

// `SessionLock` now lives in `crate::session::lock` (moved down into the state
// layer); re-exported here for engine-internal callers via `crate::session`.

fn validate_against_schema(
    schema: &serde_json::Value,
    value: &serde_json::Value,
) -> Result<(), String> {
    let compiled =
        jsonschema::JSONSchema::compile(schema).map_err(|e| format!("invalid schema: {e}"))?;
    let result = compiled.validate(value);
    if let Err(errors) = result {
        let msgs: Vec<String> = errors.map(|e| format!("  - {e}")).collect();
        return Err(msgs.join("\n"));
    }
    Ok(())
}

fn role_str(role: &InjectRole) -> &'static str {
    match role {
        InjectRole::User => "user",
        InjectRole::Assistant => "assistant",
    }
}

fn mutation_payload_to_pb(
    payload: &MutationPayload,
    _id: &str,
    _all_events: &[crate::proto::gait::log::v1::Event],
) -> crate::proto::gait::log::v1::Payload {
    use crate::proto::gait::log::v1 as pb;
    match payload {
        MutationPayload::Text(text) => pb::Payload {
            kind: Some(pb::payload::Kind::AssistantText(pb::AssistantText {
                content: text.clone(),
            })),
        },
        MutationPayload::ToolOutput {
            tool_call_id,
            output,
        } => pb::Payload {
            kind: Some(pb::payload::Kind::ToolResult(pb::ToolResult {
                call_id: tool_call_id.clone(),
                output: output.clone(),
                duration: None,
                process: None,
                checkpoint: None,
            })),
        },
    }
}

fn find_original_payload(
    id: &str,
    all_events: &[crate::proto::gait::log::v1::Event],
) -> Option<crate::proto::gait::log::v1::Payload> {
    use crate::proto::gait::log::v1::event;
    all_events
        .iter()
        .find(|e| e.id == id)
        .and_then(|e| match &e.body {
            Some(event::Body::Payload(p)) => Some(p.clone()),
            _ => None,
        })
}

pub async fn run_turn(
    session: &crate::session::Session,
    emitter: &Recorder,
    caps: crate::profile::ResolvedProfile,
    input: TurnInput,
) -> anyhow::Result<TurnResult> {
    let setup::SetupData {
        mut state,
        agent,
        provider,
        session_id,
        gait_dir,
        codebase_path,
        model,
        repos,
        system_prompt,
        mcp_registry,
        mcp_clients,
        skill_registry,
        plugin_registry,
        hooks,
        result_schema,
        llm_params,
        context_policy,
        agent_seed,
        auto,
        max_iter,
        preview_policy,
        cancel,
        sandbox_on,
        sandbox_cfg,
        relay,
        lock: _lock,
        ingestor,
        codebase_hash,
        dirty_files,
        codebase_dirty,
        effective_allow,
        deny,
        pre_events,
        current_run,
        current_turn,
        daemons,
        initial_git: _initial_git,
        agent_whitelist,
    } = setup::init(session, emitter, caps, input)?;
    let persist = Persistence::from_relay(&relay, &repos, &gait_dir, &session_id, &codebase_path);
    // Synthetic turn context (built once, prepended to every step's outgoing in
    // the step loop per plan §5d). None on the resume path — the model already
    // has context from the prior turn.
    resume::apply_if_pending(
        &pre_events,
        &ingestor,
        emitter,
        &mut state,
        &codebase_path,
        &gait_dir,
        &session_id,
        sandbox_on,
        relay.as_ref(),
        &codebase_hash,
        &dirty_files,
        &daemons,
    )
    .await?;

    let system_prompt = setup::finish_after_resume(
        &ingestor,
        emitter,
        &agent,
        &agent_seed,
        &mcp_registry,
        &mcp_clients,
        &skill_registry,
        &plugin_registry,
        &effective_allow,
        &hooks,
        &session_id,
        &gait_dir,
        relay.clone(),
        &codebase_path,
        system_prompt,
        &mut state,
        &agent_whitelist,
    )?;
    let env = state::TurnEnv {
        session,
        emitter,
        repos: &repos,
        gait_dir: &gait_dir,
        codebase_path: &codebase_path,
        session_id: &session_id,
        provider: &provider,
        hooks: &hooks,
        sandbox_cfg: &sandbox_cfg,
        sandbox_on,
        cancel: cancel.as_ref(),
        deny: &deny,
        preview_policy,
        auto,
        daemons: &daemons,
        ingestor: &ingestor,
        system_prompt: &system_prompt,
        codebase_hash: &codebase_hash,
        codebase_dirty,
        llm_params: &llm_params,
        context_policy,
        effective_allow: &effective_allow,
        relay: relay.as_ref(),
        model: &model,
        mcp_registry: &mcp_registry,
        mcp_clients: &mcp_clients,
        plugin_registry: &plugin_registry,
        result_schema: &result_schema,
    };

    let mut step_index = 0usize;
    let outcome = 'turn: loop {
        if step_index >= max_iter.unwrap_or(100) {
            break 'turn finalize::StepOutcome::MaxSteps {
                max_iter: max_iter.unwrap_or(100),
            };
        }
        // Check cancellation before starting this step
        state.final_step_index = step_index;
        // Stamp (turn, step) on the emitter so any event recorded this step
        // carries the correct position. Steps advance monotonically per turn.
        emitter.set_scope(crate::proto::envelope::EventScope::Step {
            run: current_run,
            turn: current_turn,
            step: step_index as u32,
        });
        if cancel.as_ref().map(|c| c.is_cancelled()).unwrap_or(false) {
            break 'turn finalize::StepOutcome::Cancelled;
        }

        // Per-step completion poll (Part 2): surface finished daemon and child-agent
        // completions mid-turn by recording *_update events before the next LLM
        // request. Holds the session lock → emitter.record is the safe writer.
        // retrace folds the new events into the outgoing rebuild on the very next line.
        if persist.poll_completions_allowed() {
            let events_snap = ingestor.events().unwrap_or_default();
            // Daemon completions
            let already_d =
                crate::turn::completion_poll::surfaced_daemon_idempotency_keys(&events_snap);
            for c in crate::turn::completion_poll::pending_daemon_completions(
                &gait_dir,
                &session_id,
                &already_d,
            ) {
                let _ = emitter.record(c.body());
            }
            // Child-agent completions (Stage 4, Task 4)
            let already_a =
                crate::turn::completion_poll::surfaced_agent_idempotency_keys(&events_snap);
            for c in crate::turn::completion_poll::pending_agent_completions(
                &gait_dir,
                &session_id,
                &already_a,
            ) {
                let _ = emitter.record(c.body());
            }
        }

        // 1. Build transcript from log (log-first: all prior mutations already logged)
        let all_events = ingestor.events()?;
        let mut step_muts = active_mutations(&all_events);
        apply_cascade(&mut step_muts, &all_events);
        let raw = run_projection(&ActiveRawMessages, &all_events, &step_muts);
        let mut outgoing_raw: Vec<Message> = raw.base.clone();

        let last_role = outgoing_raw
            .last()
            .map(|m| match m {
                Message::User { .. } => "user",
                Message::Assistant { .. } => "assistant",
                Message::ToolResult { .. } => "tool",
            })
            .unwrap_or("")
            .to_string();

        // 2. pre_step hook — log ALL mutations first, then re-fold (log-first enforcement)
        for h in &hooks {
            let ctx = HookContext {
                event: "pre_step".to_string(),
                session_id: session_id.clone(),
                gait_dir: gait_dir.clone(),
                codebase: codebase_path.clone(),
                system_prompt: system_prompt.clone(),
                step_index,
                message_count: outgoing_raw.len(),
                last_role: last_role.clone(),
                ..Default::default()
            };
            if let Some(mutations) = h.pre_step(&ctx, emitter, &outgoing_raw) {
                for m in &mutations {
                    match m {
                        MessageMutation::Redact { id } => {
                            let _ = emitter
                                .record(events::redact(id.clone(), Some("pre_step redact".into())));
                        }
                        MessageMutation::Replace { id, payload } => {
                            let replacement_pb = mutation_payload_to_pb(payload, id, &all_events);
                            let _ = emitter.record(events::replace(
                                id.clone(),
                                replacement_pb,
                                Some("pre_step replace".into()),
                            ));
                        }
                        MessageMutation::Restore { id } => {
                            if let Some(original_pb) = find_original_payload(id, &all_events) {
                                let _ = emitter.record(events::replace(
                                    id.clone(),
                                    original_pb,
                                    Some("pre_step restore".into()),
                                ));
                            }
                        }
                        MessageMutation::Inject { role, text } => {
                            let _ = emitter.record(events::inject(role_str(role), text.clone()));
                        }
                    }
                }
                // Re-fold from log after all mutations are written
                let all_events = ingestor.events()?;
                let mut step_muts = active_mutations(&all_events);
                apply_cascade(&mut step_muts, &all_events);
                let raw = run_projection(&ActiveRawMessages, &all_events, &step_muts);
                outgoing_raw = raw.base;
            }
        }

        // 3. Coalesce for LLM
        let mut outgoing = coalesce(outgoing_raw);

        // 4. Build the session-state tail message (cache-safe). The status
        // (`<status>`) was previously prepended as a synthetic message at index
        // 0 — that placed it ahead of history and bust the message-prefix
        // cache every turn. Now we assemble status + permissions + loadable
        // enabled-state into one string and pass it to the provider as
        // `tail_context`; the provider appends it as an uncached trailing
        // block (Anthropic) or final message (OpenAI), so churn in the tail
        // never invalidates the cached [tools + system + history] prefix.
        let permission_status = prompt::build_permission_status(&effective_allow, &deny);
        let enabled_state = prompt::build_loadable_enabled_state(&state.entries);
        let mode_steer = {
            let evs = ingestor.events()?;
            let mut muts = active_mutations(&evs);
            apply_cascade(&mut muts, &evs);
            active_mode(&evs, &muts)
                .map(|m| m.steer)
                .unwrap_or_default()
        };
        let session_state_body = prompt::build_session_state(
            state.turn_ctx.as_deref().unwrap_or(""),
            &permission_status,
            &enabled_state,
            &mode_steer,
        );
        // This is the final quiescent boundary before the normal provider
        // request.  A Summary is session-scoped and only changes the
        // model-facing fold; the immutable event history stays intact.
        let events_before_request = ingestor.events()?;
        let observed = compaction::observed_usage_after_latest_summary(&events_before_request);
        let estimate = compaction::estimate(
            &outgoing,
            &system_prompt,
            &state.tools,
            &session_state_body,
            observed,
        );
        if estimate.crosses(env.context_policy) {
            if compaction::has_unmatched_tool_call(&outgoing) {
                break 'turn finalize::StepOutcome::Fail(anyhow::anyhow!(
                    compaction::Error::UnsafePendingToolCall
                ));
            }
            let through_seq = events_before_request.last().map(|event| event.seq);
            let summary = match compaction::summarize_with_retry(&env, &outgoing).await {
                Ok(summary) => summary,
                Err(compaction::Error::Cancelled) => break 'turn finalize::StepOutcome::Cancelled,
                Err(error) => break 'turn finalize::StepOutcome::Fail(anyhow::anyhow!(error)),
            };
            emitter.set_scope(crate::proto::envelope::EventScope::Session);
            emitter.record(events::summary(
                summary.summary,
                pb::CompactionTokenUsage {
                    input: summary.usage.input_tokens as u64,
                    output: summary.usage.output_tokens as u64,
                    cache_read: summary.usage.cache_read_tokens as u64,
                    cache_write: summary.usage.cache_write_tokens as u64,
                },
                through_seq,
            ))?;
            emitter.set_scope(crate::proto::envelope::EventScope::Step {
                run: current_run,
                turn: current_turn,
                step: step_index as u32,
            });
            let all_events = ingestor.events()?;
            let mut step_muts = active_mutations(&all_events);
            apply_cascade(&mut step_muts, &all_events);
            outgoing = coalesce(run_projection(&ActiveRawMessages, &all_events, &step_muts).base);
            let rebuilt = compaction::estimate(
                &outgoing,
                &system_prompt,
                &state.tools,
                &session_state_body,
                0,
            );
            if rebuilt.crosses(env.context_policy) {
                break 'turn finalize::StepOutcome::Fail(anyhow::anyhow!(
                    compaction::Error::StillTooLarge {
                        predicted: rebuilt.predicted_tokens(),
                        threshold: env.context_policy.compaction_threshold_tokens,
                    }
                ));
            }
        }
        // `outgoing` is the per-step message list sent to the LLM (rebuilt from
        // the log every step under the log-first design).
        //
        // Durable schedule/complete pair around the LLM call (the gait analogue
        // of Temporal's ScheduleActivityTask/ActivityTaskCompleted): recorded
        // here, not in stream.rs, which is permission-free generation only.
        emitter.record(events::llm_call_started(env.model.to_string()))?;
        let usage_before = state.total_usage.clone();
        let stream::StepYield {
            step_text,
            step_tool_calls,
            post_tool_text,
            mut step_thinking_text,
            mut step_thinking_signature,
        } = match stream::run_step(&env, &mut state, &outgoing, session_state_body.as_str()).await {
            Ok(y) => y,
            Err(e) => break 'turn finalize::StepOutcome::Fail(e),
        };
        let usage = pb::StepTokenUsage {
            input: state
                .total_usage
                .input_tokens
                .saturating_sub(usage_before.input_tokens) as u64,
            output: state
                .total_usage
                .output_tokens
                .saturating_sub(usage_before.output_tokens) as u64,
            cache_read: state
                .total_usage
                .cache_read_tokens
                .saturating_sub(usage_before.cache_read_tokens) as u64,
            cache_write: state
                .total_usage
                .cache_write_tokens
                .saturating_sub(usage_before.cache_write_tokens) as u64,
        };
        emitter.record(events::llm_call_completed(
            usage,
            step_tool_calls.len() as u32,
            !step_text.is_empty() || !step_thinking_text.is_empty(),
        ))?;
        // ALWAYS emit ThinkingBlock to the log — show_thinking in GaitConfig
        // controls terminal rendering only; the event is always written.
        if !step_thinking_text.is_empty() {
            emitter.record(events::thinking_block(
                std::mem::take(&mut step_thinking_text),
                std::mem::take(&mut step_thinking_signature),
            ))?;
        }
        state.all_turn_text.push_str(&step_text);
        // Was Esc pressed during this step's stream?
        if cancel.as_ref().map(|c| c.is_cancelled()).unwrap_or(false) {
            break 'turn finalize::StepOutcome::Cancelled;
        }
        {
            let ctx = HookContext {
                event: "post_step".to_string(),
                session_id: session_id.clone(),
                gait_dir: gait_dir.clone(),
                codebase: codebase_path.clone(),
                system_prompt: system_prompt.clone(),
                step_index,
                message_count: state.messages.len(),
                last_role: last_role.clone(),
                response_text: step_text.clone(),
                tool_count: step_tool_calls.len(),
                tool_names: step_tool_calls
                    .iter()
                    .map(|tc| tc.tool_name.clone())
                    .collect(),
                ..Default::default()
            };
            for h in &hooks {
                h.post_step(&ctx, emitter);
            }
        }

        // Flush assistant text — only as a standalone event when there are NO tool calls.
        // When tool calls are present, the text is included as AssistantPart::Text in the
        // Parts message below. Writing a separate AssistantText event here would produce
        // two consecutive assistant messages in context reconstruction, which Anthropic rejects.
        if !step_text.is_empty() && step_tool_calls.is_empty() {
            emitter.record(events::assistant_text(step_text.clone()))?;
        }

        // No tool calls → auto-wrap the text as an implicit output() call.
        // output() is always the turn-completion path; plain text is just a short-hand.
        // Exception: when a result_schema is configured the agent MUST call structured_done.
        // Nudge it back into generation until done_nudges_remaining is exhausted.
        if step_tool_calls.is_empty() && result_schema.is_some() {
            if state.done_nudges_remaining > 0 {
                state.done_nudges_remaining -= 1;
                emitter.record(events::user_text(
                    "You must finish by calling `structured_done` with a `result` matching \
                     the declared schema. Plain-text completion is not accepted for this task."
                        .into(),
                ))?;
                step_index += 1;
                continue 'turn;
            } else {
                break 'turn finalize::StepOutcome::Fail(anyhow::anyhow!(
                    "subagent ended without calling structured_done after all nudges"
                ));
            }
        }
        if step_tool_calls.is_empty() {
            let message = step_text.clone();
            break 'turn finalize::StepOutcome::Done {
                message,
                result: None,
                implicit: true,
            };
        }

        // Scope pre-check (supervised mode + sandbox on only): for each world-touching
        // tool call that exposes resolvable path targets, verify every target falls
        // within the sandbox ceiling (codebase root + default config). If any target
        // is outside the ceiling, pause immediately with TurnResult::SandboxUnlock — the tool
        // is NOT run. In auto mode the kernel seal (EACCES) handles out-of-scope writes
        // via Task 6; the pre-check is a supervised-only UX feature.
        if !auto && sandbox_on {
            // Build the active unlock set from the session log so that
            // once/session grants (not persisted to config) pass through.
            let active_unlock = {
                let evs = ingestor.events().unwrap_or_default();
                active_unlock_state(&evs)
            };
            let mut pending_unlocks: Vec<pb::SandboxUnlockItem> = vec![];
            for tc in &step_tool_calls {
                if let Some(tool) = state.tools.iter().find(|t| t.name() == tc.tool_name) {
                    let items =
                        match check_own_tool_scope(tc, tool.as_ref(), &codebase_path, &sandbox_cfg)
                        {
                            Ok(items) => items,
                            Err(error) => break 'turn finalize::StepOutcome::Fail(error),
                        };
                    // Filter out items whose resource path is already granted
                    // by a prior unlock reply (once or session scope).
                    for item in items {
                        let path = std::path::PathBuf::from(&item.resource);
                        let already_granted = active_unlock.once.contains(&path)
                            || active_unlock.session.contains(&path);
                        if !already_granted {
                            pending_unlocks.push(item);
                        }
                    }
                }
            }
            if !pending_unlocks.is_empty() {
                break 'turn finalize::StepOutcome::Pause(finalize::PauseKind::SandboxUnlock(
                    pending_unlocks,
                ));
            }
        }

        match execute::run_tools(
            &env,
            &mut state,
            &persist,
            step_index,
            step_text,
            step_tool_calls,
            post_tool_text,
        )
        .await
        {
            Ok(finalize::StepOutcome::Continue) => {}
            Ok(other) => break 'turn other,
            Err(e) => break 'turn finalize::StepOutcome::Fail(e),
        }
        step_index += 1;
    };

    finalize::finalize(&env, &mut state, &persist, outcome).await
}

/// Check whether a single tool call targets any path outside its sandbox ceiling.
///
/// Returns a list of `SandboxUnlockItem`s — one per out-of-scope target. Returns an
/// empty vec when all targets are within scope (or the tool exposes no resolvable
/// targets via `scope_targets`).
fn check_own_tool_scope(
    tc: &ToolCall,
    tool: &dyn Tool,
    codebase: &std::path::Path,
    sandbox_cfg: &crate::config::SandboxConfig,
) -> anyhow::Result<Vec<pb::SandboxUnlockItem>> {
    use crate::sandbox::ceiling::compute_scope_nofs;
    use crate::sandbox::normalize_lexical;
    use crate::types::Dimension;

    let targets = tool.scope_targets(&tc.args);
    if targets.is_empty() {
        return Ok(vec![]);
    }

    let temp_jail = std::path::Path::new("/tmp/gait-jail");
    // Disk-free pre-check: normalize codebase lexically (no canonicalize).
    // The hand's kernel seal remains the authoritative enforcement; this advisory
    // check only needs to be a sound approximation (no symlink resolution needed).
    let codebase_norm = normalize_lexical(codebase);
    let ceiling = compute_scope_nofs(tool.access(), sandbox_cfg, &codebase_norm, temp_jail)?;

    let mut items = vec![];
    for (path, dim) in targets {
        // Normalize target lexically — no filesystem access, works for non-existent paths.
        let target = normalize_lexical(&path);
        let in_scope = match dim {
            Dimension::Write => ceiling
                .writable
                .iter()
                .any(|allowed| target.starts_with(allowed)),
            Dimension::Read => {
                // Phase 2d: read_all defaults to false → reads are scope-checked.
                // When read_all=true (config `readable: ["*"]`) all reads pass through.
                if ceiling.read_all {
                    true
                } else {
                    ceiling
                        .readable
                        .iter()
                        .any(|allowed| target.starts_with(allowed))
                }
            }
        };
        if !in_scope {
            items.push(pb::SandboxUnlockItem {
                id: uuid::Uuid::new_v4().to_string(),
                tool_name: tc.tool_name.clone(),
                resource: path.to_string_lossy().into_owned(),
                dimension: dim.as_str().to_string(),
                preview: None,
            });
        }
    }
    Ok(items)
}

#[cfg(test)]
mod golden_tests;
#[cfg(test)]
mod tests;

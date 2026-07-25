use crate::proto::gait::log::v1 as pb;
use crate::proto::wire;
use crate::types::HookContext;

/// `SessionStart` — identity only (model / codebase history dropped per design).
pub fn session_start(
    session_id: String,
    assigned_by: Option<String>,
    codebase_path: Option<String>,
    notify_done: bool,
) -> pb::event::Body {
    wire::body(pb::payload::Kind::SessionStart(pb::SessionStart {
        session_id,
        assigned_by,
        codebase_path,
        notify_done,
    }))
}

/// `UserText` — user-authored message text. Per-turn environment state
/// (codebase stamp, injected files) is now a separate `ContextStatus` event;
/// `user_text` carries only content — sender attribution lives on Event.provenance.
pub fn user_text(content: String) -> pb::event::Body {
    wire::body(pb::payload::Kind::UserText(pb::UserText { content }))
}

/// `ContextStatus` — per-turn environment state (codebase stamp + injected
/// files). Emitted alongside a `UserText` (or after a snapshot change) so the
/// transcript can reconstruct working-tree context without it leaking into
/// the user message itself.
pub fn context_status(
    codebase: Option<pb::CodebaseState>,
    injected_files: Vec<String>,
) -> pb::event::Body {
    wire::body(pb::payload::Kind::ContextStatus(pb::ContextStatus {
        codebase,
        injected_files,
    }))
}

/// `Inject` — append a new message to the transcript (Design A: permanent).
/// `role` is "user" or "assistant"; anything else folds to user-side for
/// safety. Used by `pre_step` hooks to push context into the next fold.
pub fn inject(role: &str, content: String) -> pb::event::Body {
    wire::body(pb::payload::Kind::Inject(pb::Inject {
        role: role.to_string(),
        content,
    }))
}

/// `AssistantText`.
pub fn assistant_text(content: String) -> pb::event::Body {
    wire::body(pb::payload::Kind::AssistantText(pb::AssistantText {
        content,
    }))
}

/// A working-directory change, recorded when a `shell` call's `cwd` param
/// resolves successfully. Plain historical log record — nothing folds these
/// into an authoritative "current directory" fed back into later calls; each
/// `shell` call is independent (see `tools::shell`).
pub fn cwd(path: &str) -> pb::event::Body {
    wire::body(pb::payload::Kind::Cwd(pb::Cwd {
        path: path.to_string(),
    }))
}

/// Turn cancellation signal — emitted when the user presses Esc mid-turn.
/// Retrace folds this into `[turn interrupted by user]` in the next transcript.
pub fn cancellation_signal() -> pb::event::Body {
    wire::body(pb::payload::Kind::CancellationSignal(
        pb::CancellationSignal {},
    ))
}

/// `Toggle` — capability enable/disable delta (skill | mcp | tool | plugin), folded by ActiveAgent.
pub fn toggle(kind: &str, name: &str, enabled: bool) -> pb::event::Body {
    let k = match kind {
        "skill" => pb::ToggleKind::Skill,
        "mcp" => pb::ToggleKind::Mcp,
        "tool" => pb::ToggleKind::Tool,
        "plugin" => pb::ToggleKind::Plugin,
        _ => pb::ToggleKind::Unspecified,
    };
    wire::body(pb::payload::Kind::Toggle(pb::Toggle {
        kind: k as i32,
        name: name.to_string(),
        enabled,
    }))
}

/// `Registry` — registration/whitelist delta (skill | mcp | plugin), folded by
/// `ActiveAgent` into `session_skills`/`session_mcp`/`session_plugins`. Distinct
/// from `toggle`'s live-load axis above; `kind: &str` uses the same convention.
pub fn registry(kind: &str, name: &str, listed: bool) -> pb::event::Body {
    let k = match kind {
        "skill" => pb::ToggleKind::Skill,
        "mcp" => pb::ToggleKind::Mcp,
        "plugin" => pb::ToggleKind::Plugin,
        _ => pb::ToggleKind::Unspecified,
    };
    wire::body(pb::payload::Kind::Registry(pb::Registry {
        kind: k as i32,
        name: name.to_string(),
        listed,
    }))
}

/// Complete, immutable inference snapshot. `name == ""` clears the selection.
pub fn inference(inference: pb::Inference) -> pb::event::Body {
    wire::body(pb::payload::Kind::Inference(inference))
}

/// Active-mode overlay event (replay-complete, LWW-folded by `ActiveMode`).
/// `name == ""` is the clear sentinel (back to default/no overlay).
pub fn mode(name: &str, deny: Vec<String>, steer: &str, description: &str) -> pb::event::Body {
    wire::body(pb::payload::Kind::Mode(pb::Mode {
        name: name.to_string(),
        deny,
        steer: steer.to_string(),
        description: description.to_string(),
    }))
}

/// `ToolCall` — id → call_id, `Value` input → `Struct`.
pub fn tool_call(call_id: String, name: String, input: &serde_json::Value) -> pb::event::Body {
    wire::body(pb::payload::Kind::ToolCall(pb::ToolCall {
        call_id,
        name,
        input: to_struct(input),
    }))
}

/// `ToolResult` — generic `duration` (every tool) + `ProcessResult` (shell/process tools).
/// Both are optional; the synthetic sites (denied/error/approved-fallback/recorded) pass `None, None`.
pub fn tool_result(
    call_id: String,
    output: String,
    duration: Option<pbjson_types::Duration>,
    process: Option<pb::ProcessResult>,
) -> pb::event::Body {
    wire::body(pb::payload::Kind::ToolResult(pb::ToolResult {
        call_id,
        output,
        duration,
        process,
    }))
}

/// `Error` — phase string → proto enum.
pub fn error(message: String, phase: &str) -> pb::event::Body {
    wire::body(pb::payload::Kind::Error(pb::Error {
        message,
        phase: error_phase(phase),
    }))
}

/// `Clarify`.
pub fn clarify(items: &[pb::ClarifyItem]) -> pb::event::Body {
    wire::body(pb::payload::Kind::Clarify(pb::Clarify {
        items: items.to_vec(),
    }))
}

/// `ClarifyReply`.
pub fn clarify_reply(items: &[pb::ClarifyReplyItem]) -> pb::event::Body {
    wire::body(pb::payload::Kind::ClarifyReply(pb::ClarifyReply {
        items: items.to_vec(),
    }))
}

/// `ClarifyConvergence`.
pub fn clarify_convergence(items: &[pb::ConvergenceItem]) -> pb::event::Body {
    wire::body(pb::payload::Kind::ClarifyConvergence(
        pb::ClarifyConvergence {
            items: items.to_vec(),
        },
    ))
}

/// `Inform`.
pub fn inform(items: &[pb::InformItem]) -> pb::event::Body {
    wire::body(pb::payload::Kind::Inform(pb::Inform {
        items: items.to_vec(),
    }))
}

/// `InformReply`.
pub fn inform_reply(items: &[pb::InformReplyItem]) -> pb::event::Body {
    wire::body(pb::payload::Kind::InformReply(pb::InformReply {
        items: items.to_vec(),
    }))
}

/// `InformConvergence`.
pub fn inform_convergence(items: &[pb::ConvergenceItem]) -> pb::event::Body {
    wire::body(pb::payload::Kind::InformConvergence(
        pb::InformConvergence {
            items: items.to_vec(),
        },
    ))
}

/// `AgentUpdate` — status string → proto enum, items → `Value`.
pub fn agent_update(
    session_id: String,
    status: &str,
    summary: String,
    items: Option<serde_json::Value>,
    idempotency_key: String,
) -> pb::event::Body {
    wire::body(pb::payload::Kind::AgentUpdate(pb::AgentUpdate {
        session_id,
        status: agent_status(status),
        summary,
        items: to_value_opt(&items),
        idempotency_key,
    }))
}

/// `DaemonUpdate` — completion event for a detached daemon. Status string →
/// proto enum; the rendered form lives in `retrace::push_daemon_update` and
/// its inline mirror at the raw-projection site.
pub fn daemon_update(
    name: String,
    exit_code: i32,
    status: &str,
    log_path: String,
    idempotency_key: String,
) -> pb::event::Body {
    wire::body(pb::payload::Kind::DaemonUpdate(pb::DaemonUpdate {
        name,
        exit_code,
        status: daemon_status(status),
        log_path,
        idempotency_key,
    }))
}

/// `Permissions`.
pub fn permissions(items: &[pb::PermissionItem]) -> pb::event::Body {
    wire::body(pb::payload::Kind::Permissions(pb::Permissions {
        items: items.to_vec(),
    }))
}

/// `PermissionsReply` — caller is responsible for constructing `pb::PermissionsReplyItem` with the correct enum values for `decision` and `scope`.
/// Replier id is no longer carried on the proto message — it lives on
/// `Event.provenance` for events that need it.
pub fn permissions_reply(items: &[pb::PermissionsReplyItem]) -> pb::event::Body {
    wire::body(pb::payload::Kind::PermissionsReply(pb::PermissionsReply {
        items: items.to_vec(),
    }))
}

/// `SandboxUnlock` — raised by the hand when a tool access exceeds the
/// declared sandbox scope. The brain pauses and surfaces these items for
/// human review.
pub fn sandbox_unlock(items: &[pb::SandboxUnlockItem]) -> pb::event::Body {
    wire::body(pb::payload::Kind::SandboxUnlock(pb::SandboxUnlock {
        items: items.to_vec(),
    }))
}

/// `SandboxUnlockReply` — the human's decision for each `SandboxUnlockItem`.
/// `from` is the replier id; each item carries a `decision` and `scope`.
/// Replier id is no longer carried on the proto message — it lives on
/// `Event.provenance` for events that need it.
pub fn sandbox_unlock_reply(items: &[pb::SandboxUnlockReplyItem]) -> pb::event::Body {
    wire::body(pb::payload::Kind::SandboxUnlockReply(
        pb::SandboxUnlockReply {
            items: items.to_vec(),
        },
    ))
}

pub fn sandbox_whitelist_match(items: &[pb::SandboxWhitelistMatchItem]) -> pb::event::Body {
    wire::body(pb::payload::Kind::SandboxWhitelistMatch(
        pb::SandboxWhitelistMatch {
            items: items.to_vec(),
        },
    ))
}

/// `SandboxDenied` — a tool call's failure was attributed to a sandbox
/// restriction. Fires in both supervised and auto mode, unlike
/// `SandboxUnlock` (which only exists when there's a human to pause for).
pub fn sandbox_denied(
    tool_name: &str,
    resource: &str,
    dimension: &str,
    matched_signature: &str,
    hint: Option<String>,
) -> pb::event::Body {
    wire::body(pb::payload::Kind::SandboxDenied(pb::SandboxDenied {
        tool_name: tool_name.to_string(),
        resource: resource.to_string(),
        dimension: dimension.to_string(),
        matched_signature: matched_signature.to_string(),
        hint,
        // Populated by the hand's deny-policy classification (Task 8 of the
        // 2026-07-25 sandbox-file-deny-policy plan); not yet wired at this
        // construction site — placeholder until that task lands.
        matched_deny_pattern: None,
    }))
}

/// `PermissionDenied` — a tool call matched a deny[] pattern. Emitted
/// alongside the ordinary ToolCall/ToolResult, not instead of them.
pub fn permission_denied(tool_name: &str, resource: &str, pattern: &str) -> pb::event::Body {
    wire::body(pb::payload::Kind::PermissionDenied(pb::PermissionDenied {
        tool_name: tool_name.to_string(),
        resource: resource.to_string(),
        pattern: pattern.to_string(),
    }))
}

/// Parameters for [`turn_complete`].
#[derive(Debug, Default)]
pub struct TurnCompleteParams {
    pub usage: pb::TurnTokenUsage,
    pub codebase_hash: String,
    pub codebase_dirty: bool,
    pub checkpoint: Option<String>,
    pub blocks: Vec<String>,
    pub message: String,
    pub result: Option<serde_json::Value>,
    pub changes: Option<pb::Changes>,
}

/// `TurnComplete`.
pub fn turn_complete(p: TurnCompleteParams) -> pb::event::Body {
    wire::body(pb::payload::Kind::TurnComplete(pb::TurnComplete {
        usage: Some(p.usage),
        codebase: Some(pb::CodebaseState {
            hash: p.codebase_hash,
            dirty: p.codebase_dirty,
            dirty_files: Vec::new(),
        }),
        checkpoint: p.checkpoint,
        blocks: p.blocks,
        output: Some(pb::StructuredDoneOutput {
            message: p.message,
            result: to_value_opt(&p.result),
            changes: p.changes,
        }),
    }))
}

// NOTE: the `ResolvedProfile`-typed `agent_msg` / `agent` builders formerly
// here have moved to the engine side (`crate::turn::agent_event`) — proto (now
// heading to `gait-state`) no longer names the engine `ResolvedProfile` type.

/// `SandboxApplied` — kernel enforcement status at process start.
/// `enforcement` is one of "active" | "unavailable" | "noop".
pub fn sandbox_applied(
    enforcement: &str,
    scope_paths: Vec<String>,
    reason: Option<String>,
) -> pb::event::Body {
    wire::body(pb::payload::Kind::SandboxApplied(pb::SandboxApplied {
        enforcement: sandbox_enforcement(enforcement),
        scope_paths,
        reason,
        // Populated from SandboxScope.read_deny/write_deny by Task 10 of the
        // 2026-07-25 sandbox-file-deny-policy plan; not yet wired at this
        // construction site — placeholder until that task lands.
        read_deny_paths: Vec::new(),
        write_deny_paths: Vec::new(),
        deny_enforced: false,
    }))
}

/// Build a `HookEnv` proto from a `HookContext` — the typed snapshot of what a hook process saw.
fn hook_env_from_context(ctx: &HookContext) -> pb::HookEnv {
    let tool_args = ctx
        .tool_args
        .as_object()
        .map(|obj| {
            let mut m = std::collections::HashMap::new();
            for (k, v) in obj {
                let val = match v {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                m.insert(k.clone(), val);
            }
            m
        })
        .unwrap_or_default();
    pb::HookEnv {
        event: ctx.event.clone(),
        session_id: ctx.session_id.clone(),
        gait_dir: ctx.gait_dir.display().to_string(),
        codebase: ctx.codebase.display().to_string(),
        system_prompt: ctx.system_prompt.clone(),
        step_index: ctx.step_index as u32,
        step_count: ctx.step_count as u32,
        message_count: ctx.message_count as u32,
        last_role: ctx.last_role.clone(),
        response_text: ctx.response_text.clone(),
        tool_count: ctx.tool_count as u32,
        tool_names: ctx.tool_names.clone(),
        tool_name: ctx.tool_name.clone(),
        tool_call_id: ctx.tool_call_id.clone(),
        tool_result: ctx.tool_result.clone(),
        turn_outcome: ctx.turn_outcome.clone(),
        final_response: ctx.final_response.clone(),
        tool_args,
    }
}

/// `HookRun` — hook execution record with full env snapshot.
/// `phase`: "post_tool" | "pre_tool" | "post_turn" | "pre_turn" | "pre_step" | "post_step"
pub fn hook_run(
    phase: &str,
    command: &str,
    exit_code: i32,
    output: &str,
    ctx: &HookContext,
) -> pb::event::Body {
    let call_id = if ctx.tool_call_id.is_empty() {
        None
    } else {
        Some(ctx.tool_call_id.clone())
    };
    wire::body(pb::payload::Kind::HookRun(pb::HookRun {
        phase: hook_phase(phase),
        command: command.to_string(),
        exit_code,
        output: output.to_string(),
        call_id,
        env: Some(hook_env_from_context(ctx)),
    }))
}

/// `Replace` mutation — rewrites a prior event's payload in the fold.
/// The replacement must be a fully-typed `Payload`; the target is the event's `id` string.
pub fn replace(
    target: String,
    replacement: pb::Payload,
    reason: Option<String>,
) -> pb::event::Body {
    pb::event::Body::Replace(pb::Replace {
        target,
        replacement: Some(replacement),
        reason,
    })
}

/// `Redact` mutation — removes a prior event from the fold entirely.
pub fn redact(target: String, reason: Option<String>) -> pb::event::Body {
    pb::event::Body::Redact(pb::Redact { target, reason })
}

fn hook_phase(s: &str) -> i32 {
    let p = match s {
        "post_tool" => pb::HookPhase::PostTool,
        "pre_tool" => pb::HookPhase::PreTool,
        "post_turn" => pb::HookPhase::PostTurn,
        "pre_turn" => pb::HookPhase::PreTurn,
        "pre_step" => pb::HookPhase::PreStep,
        "post_step" => pb::HookPhase::PostStep,
        _ => pb::HookPhase::Unspecified,
    };
    p as i32
}

/// `Summary` — through_seq unset (no seq in legacy data).
pub fn summary(
    summary: String,
    turns_covered: u32,
    usage: pb::CompactionTokenUsage,
) -> pb::event::Body {
    wire::body(pb::payload::Kind::Summary(pb::Summary {
        summary,
        turns_covered,
        usage: Some(usage),
        through_seq: None,
    }))
}

/// `ThinkingBlock` — a captured thinking/reasoning block, always emitted to the log.
/// The `signature` is required for Anthropic multi-turn replay; pass empty string
/// for non-Anthropic providers that don't supply one.
/// NOTE: show_thinking (render toggle) lives on GaitConfig, not here.
pub fn thinking_block(content: String, signature: String) -> pb::event::Body {
    wire::body(pb::payload::Kind::ThinkingBlock(pb::ThinkingBlock {
        content,
        signature,
    }))
}

/// `LlmCallStarted` — durably marks that an LLM request is about to be sent,
/// recorded immediately before `provider.stream()` is called. Paired with
/// `llm_call_completed`; `turn/resume.rs` treats an unmatched
/// `LlmCallStarted` as a call that crashed mid-stream — since it has no
/// external side effect, recovery just retries rather than needing a
/// synthetic failure result (contrast with `tool_result`'s crash path).
pub fn llm_call_started(model: String) -> pb::event::Body {
    wire::body(pb::payload::Kind::LlmCallStarted(pb::LlmCallStarted {
        model,
    }))
}

/// `LlmCallCompleted` — durably marks that the LLM stream finished. `usage`
/// is the cumulative turn usage AFTER this step (same convention as
/// `TurnComplete.usage`, not a per-step delta); `tool_call_count`/`had_text`
/// describe this step's own yield.
pub fn llm_call_completed(
    usage: pb::TurnTokenUsage,
    tool_call_count: u32,
    had_text: bool,
) -> pb::event::Body {
    wire::body(pb::payload::Kind::LlmCallCompleted(pb::LlmCallCompleted {
        usage: Some(usage),
        tool_call_count,
        had_text,
    }))
}

// ── helpers ──────────────────────────────────────────────────────────────────

/// Convert `LlmParamsConfig` (serde/config layer) to proto `LlmParams` (wire layer).
/// Called by `agent_msg` to embed the config in the Agent event.
pub fn llm_params_to_proto(p: &crate::config::LlmParamsConfig) -> pb::LlmParams {
    use pb::llm_params::ProviderParams;

    let provider_params = if let Some(a) = &p.anthropic {
        Some(ProviderParams::Anthropic(anthropic_llm_params_to_proto(a)))
    } else if let Some(o) = &p.openai {
        Some(ProviderParams::Openai(openai_llm_params_to_proto(o)))
    } else {
        p.gemini
            .as_ref()
            .map(|g| ProviderParams::Gemini(gemini_llm_params_to_proto(g)))
    };

    pb::LlmParams {
        max_tokens: p.max_tokens,
        temperature: p.temperature,
        top_p: p.top_p,
        // show_thinking is NOT stored in LlmParams proto — it is a GaitConfig render toggle.
        provider_params,
    }
}

/// Convert the replay value back into the config-layer provider parameters.
/// The proto union is validated instead of silently choosing a provider.
pub fn llm_params_from_proto(p: &pb::LlmParams) -> anyhow::Result<crate::config::LlmParamsConfig> {
    use crate::config as cfg;
    use pb::llm_params::ProviderParams;
    let mut out = cfg::LlmParamsConfig {
        max_tokens: p.max_tokens,
        temperature: p.temperature,
        top_p: p.top_p,
        ..Default::default()
    };
    match &p.provider_params {
        None => {}
        Some(ProviderParams::Anthropic(a)) => {
            out.anthropic = Some(cfg::AnthropicLlmParamsConfig {
                top_k: a.top_k,
                effort: a.effort.clone(),
                thinking: a.thinking.as_ref().map(|t| cfg::AnthropicThinkingConfig {
                    kind: match pb::AnthropicThinkingType::try_from(t.r#type)
                        .unwrap_or(pb::AnthropicThinkingType::Unspecified)
                    {
                        pb::AnthropicThinkingType::Adaptive => Some("adaptive".into()),
                        pb::AnthropicThinkingType::Disabled => Some("disabled".into()),
                        pb::AnthropicThinkingType::Enabled => Some("enabled".into()),
                        _ => None,
                    },
                    budget_tokens: t.budget_tokens,
                    display: match pb::AnthropicThinkingDisplay::try_from(t.display)
                        .unwrap_or(pb::AnthropicThinkingDisplay::Unspecified)
                    {
                        pb::AnthropicThinkingDisplay::Summarized => Some("summarized".into()),
                        pb::AnthropicThinkingDisplay::Omitted => Some("omitted".into()),
                        _ => None,
                    },
                }),
                strict_tools: None,
            });
        }
        Some(ProviderParams::Openai(o)) => {
            out.openai = Some(cfg::OpenAILlmParamsConfig {
                reasoning_effort: o.reasoning_effort.clone(),
                frequency_penalty: o.frequency_penalty,
                presence_penalty: o.presence_penalty,
                seed: o.seed,
                stop: o.stop.clone(),
                strict_tools: None,
            });
        }
        Some(ProviderParams::Gemini(g)) => {
            out.gemini = Some(cfg::GeminiLlmParamsConfig {
                top_k: g.top_k,
                candidate_count: g.candidate_count,
                stop_sequences: g.stop_sequences.clone(),
                seed: g.seed,
                frequency_penalty: g.frequency_penalty,
                presence_penalty: g.presence_penalty,
                thinking: g.thinking.as_ref().map(|t| cfg::GeminiThinkingConfig {
                    include_thoughts: t.include_thoughts,
                    budget: match &t.depth {
                        Some(pb::gemini_thinking::Depth::Budget(n)) => Some(*n),
                        _ => None,
                    },
                    level: match &t.depth {
                        Some(pb::gemini_thinking::Depth::Level(n)) => {
                            match pb::GeminiThinkingLevel::try_from(*n)
                                .unwrap_or(pb::GeminiThinkingLevel::Unspecified)
                            {
                                pb::GeminiThinkingLevel::Minimal => Some("minimal".into()),
                                pb::GeminiThinkingLevel::Low => Some("low".into()),
                                pb::GeminiThinkingLevel::Medium => Some("medium".into()),
                                pb::GeminiThinkingLevel::High => Some("high".into()),
                                _ => None,
                            }
                        }
                        _ => None,
                    },
                }),
            });
        }
    }
    Ok(out)
}

fn anthropic_llm_params_to_proto(
    a: &crate::config::AnthropicLlmParamsConfig,
) -> pb::AnthropicLlmParams {
    pb::AnthropicLlmParams {
        top_k: a.top_k,
        effort: a.effort.clone(),
        thinking: a.thinking.as_ref().map(|t| pb::AnthropicThinking {
            r#type: thinking_type_to_proto(t.kind.as_deref()) as i32,
            budget_tokens: t.budget_tokens,
            display: thinking_display_to_proto(t.display.as_deref()) as i32,
        }),
    }
}

fn thinking_type_to_proto(s: Option<&str>) -> pb::AnthropicThinkingType {
    match s {
        Some("adaptive") => pb::AnthropicThinkingType::Adaptive,
        Some("disabled") => pb::AnthropicThinkingType::Disabled,
        Some("enabled") => pb::AnthropicThinkingType::Enabled,
        _ => pb::AnthropicThinkingType::Unspecified,
    }
}

fn thinking_display_to_proto(s: Option<&str>) -> pb::AnthropicThinkingDisplay {
    match s {
        Some("summarized") => pb::AnthropicThinkingDisplay::Summarized,
        Some("omitted") => pb::AnthropicThinkingDisplay::Omitted,
        _ => pb::AnthropicThinkingDisplay::Unspecified,
    }
}

fn openai_llm_params_to_proto(o: &crate::config::OpenAILlmParamsConfig) -> pb::OpenAiLlmParams {
    pb::OpenAiLlmParams {
        reasoning_effort: o.reasoning_effort.clone(),
        frequency_penalty: o.frequency_penalty,
        presence_penalty: o.presence_penalty,
        seed: o.seed,
        stop: o.stop.clone(),
    }
}

fn gemini_llm_params_to_proto(g: &crate::config::GeminiLlmParamsConfig) -> pb::GeminiLlmParams {
    use pb::gemini_thinking::Depth;
    use pb::GeminiThinkingLevel;
    let thinking = g.thinking.as_ref().map(|t| pb::GeminiThinking {
        include_thoughts: t.include_thoughts,
        depth: if let Some(b) = t.budget {
            Some(Depth::Budget(b))
        } else {
            t.level.as_deref().map(|l| {
                let level = match l {
                    "minimal" => GeminiThinkingLevel::Minimal,
                    "low" => GeminiThinkingLevel::Low,
                    "medium" => GeminiThinkingLevel::Medium,
                    "high" => GeminiThinkingLevel::High,
                    _ => GeminiThinkingLevel::Unspecified,
                };
                Depth::Level(level as i32)
            })
        },
    });
    pb::GeminiLlmParams {
        top_k: g.top_k,
        candidate_count: g.candidate_count,
        stop_sequences: g.stop_sequences.clone(),
        seed: g.seed,
        frequency_penalty: g.frequency_penalty,
        presence_penalty: g.presence_penalty,
        thinking,
    }
}

/// A JSON object → proto `Struct`. Non-objects (e.g. `Value::Null`) → `None`.
pub fn to_struct(v: &serde_json::Value) -> Option<pbjson_types::Struct> {
    serde_json::from_value(v.clone()).ok()
}

/// An optional JSON value → proto `Value` (any JSON shape).
pub fn to_value_opt(v: &Option<serde_json::Value>) -> Option<pbjson_types::Value> {
    v.as_ref()
        .and_then(|x| serde_json::from_value(x.clone()).ok())
}

fn error_phase(s: &str) -> i32 {
    let e = match s {
        "llm" => pb::ErrorPhase::Llm,
        "tool" => pb::ErrorPhase::Tool,
        "git" => pb::ErrorPhase::Git,
        "init" => pb::ErrorPhase::Init,
        _ => pb::ErrorPhase::Unspecified,
    };
    e as i32
}

fn agent_status(s: &str) -> i32 {
    let a = match s {
        "done" => pb::AgentStatus::Done,
        "clarify" => pb::AgentStatus::Clarify,
        "inform" => pb::AgentStatus::Inform,
        _ => pb::AgentStatus::Unspecified,
    };
    a as i32
}

fn daemon_status(s: &str) -> i32 {
    let a = match s {
        "done" => pb::DaemonStatus::Done,
        "killed" => pb::DaemonStatus::Killed,
        _ => pb::DaemonStatus::Unspecified,
    };
    a as i32
}

fn sandbox_enforcement(s: &str) -> i32 {
    let e = match s {
        "active" => pb::SandboxEnforcement::Active,
        "unavailable" => pb::SandboxEnforcement::Unavailable,
        "noop" => pb::SandboxEnforcement::Noop,
        _ => pb::SandboxEnforcement::Unspecified,
    };
    e as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kind_of(body: &pb::event::Body) -> &pb::payload::Kind {
        match body {
            pb::event::Body::Payload(p) => p.kind.as_ref().expect("kind set"),
            other => panic!("expected Payload body, got {other:?}"),
        }
    }

    #[test]
    fn tool_call_sets_call_id_and_struct_input() {
        let body = tool_call(
            "call-1".into(),
            "read".into(),
            &serde_json::json!({"file_path": "a.rs"}),
        );
        match kind_of(&body) {
            pb::payload::Kind::ToolCall(tc) => {
                assert_eq!(tc.call_id, "call-1");
                assert_eq!(tc.name, "read");
                let s = tc.input.as_ref().expect("input struct");
                assert!(s.fields.contains_key("file_path"));
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
    }

    #[test]
    fn error_git_maps_to_enum() {
        let body = error("boom".into(), "git");
        match kind_of(&body) {
            pb::payload::Kind::Error(e) => assert_eq!(e.phase, pb::ErrorPhase::Git as i32),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn user_text_carries_content() {
        // `from` was removed — sender attribution now lives on Event.provenance
        // (see append_with_provenance / Provenance), not on UserText itself.
        let body = user_text("hi".into());
        match kind_of(&body) {
            pb::payload::Kind::UserText(u) => {
                assert_eq!(u.content, "hi");
            }
            other => panic!("expected UserText, got {other:?}"),
        }
    }

    #[test]
    fn context_status_carries_codebase_and_injected_files() {
        let codebase = pb::CodebaseState {
            hash: "abc".into(),
            dirty: true,
            dirty_files: vec!["x.rs".into()],
        };
        let body = context_status(Some(codebase.clone()), vec!["README.md".into()]);
        match kind_of(&body) {
            pb::payload::Kind::ContextStatus(cs) => {
                let cb = cs.codebase.as_ref().expect("codebase");
                assert_eq!(cb.hash, "abc");
                assert!(cb.dirty);
                assert_eq!(cs.injected_files, vec!["README.md".to_string()]);
            }
            other => panic!("expected ContextStatus, got {other:?}"),
        }
    }

    #[test]
    fn inject_round_trips_role_and_content() {
        use crate::proto::gait::log::v1::payload;
        let body = super::inject("user", "hello from hook".into());
        match kind_of(&body) {
            payload::Kind::Inject(ti) => {
                assert_eq!(ti.role, "user");
                assert_eq!(ti.content, "hello from hook");
            }
            other => panic!("expected Inject, got {other:?}"),
        }
    }

    #[test]
    fn llm_call_started_carries_model() {
        let body = llm_call_started("anthropic/claude-sonnet-5".to_string());
        match kind_of(&body) {
            pb::payload::Kind::LlmCallStarted(s) => {
                assert_eq!(s.model, "anthropic/claude-sonnet-5");
            }
            other => panic!("expected LlmCallStarted, got {other:?}"),
        }
    }

    #[test]
    fn llm_call_completed_carries_usage_and_flags() {
        let usage = pb::TurnTokenUsage {
            input: 100,
            output: 20,
            cache_read: 5,
            cache_write: 0,
        };
        let body = llm_call_completed(usage.clone(), 2, true);
        match kind_of(&body) {
            pb::payload::Kind::LlmCallCompleted(c) => {
                assert_eq!(c.usage, Some(usage));
                assert_eq!(c.tool_call_count, 2);
                assert!(c.had_text);
            }
            other => panic!("expected LlmCallCompleted, got {other:?}"),
        }
    }

    #[test]
    fn toggle_skill_enabled_round_trips() {
        use crate::proto::gait::log::v1::{payload, ToggleKind};
        use crate::proto::wire;
        use tempfile::TempDir;

        let dir = TempDir::new().unwrap();
        let path = dir.path().join("log.jsonl");

        wire::append(&path, super::toggle("skill", "tdd", true)).expect("append toggle");

        let evs = wire::read_events(&path).expect("read events");
        assert_eq!(evs.len(), 1);
        match evs[0].body.as_ref().and_then(|b| match b {
            crate::proto::gait::log::v1::event::Body::Payload(p) => p.kind.as_ref(),
            _ => None,
        }) {
            Some(payload::Kind::Toggle(t)) => {
                assert_eq!(t.kind, ToggleKind::Skill as i32);
                assert_eq!(t.name, "tdd");
                assert!(t.enabled);
            }
            other => panic!("expected Toggle payload, got {other:?}"),
        }
    }

    #[test]
    fn toggle_skill_disabled_round_trips() {
        use crate::proto::gait::log::v1::{payload, ToggleKind};
        use crate::proto::wire;
        use tempfile::TempDir;

        let dir = TempDir::new().unwrap();
        let path = dir.path().join("log.jsonl");

        wire::append(&path, super::toggle("skill", "tdd", false)).expect("append toggle");

        let evs = wire::read_events(&path).expect("read events");
        match evs[0].body.as_ref().and_then(|b| match b {
            crate::proto::gait::log::v1::event::Body::Payload(p) => p.kind.as_ref(),
            _ => None,
        }) {
            Some(payload::Kind::Toggle(t)) => {
                assert_eq!(
                    t.kind,
                    ToggleKind::Skill as i32,
                    "kind round-trips even when disabled"
                );
                assert!(!t.enabled, "enabled must be false");
                assert_eq!(t.name, "tdd");
            }
            other => panic!("expected Toggle payload, got {other:?}"),
        }
    }

    #[test]
    fn toggle_chains_seq_after_other_event() {
        use crate::proto::wire;
        use tempfile::TempDir;

        let dir = TempDir::new().unwrap();
        let path = dir.path().join("log.jsonl");

        wire::append(&path, super::assistant_text("hello".into())).unwrap();
        wire::append(&path, super::toggle("skill", "tdd", true)).unwrap();

        let evs = wire::read_events(&path).unwrap();
        assert_eq!(evs.len(), 2);
        assert_eq!(evs[0].seq, 1);
        assert_eq!(evs[1].seq, 2);
        assert_eq!(
            evs[1].parents,
            vec![evs[0].id.clone()],
            "Toggle must chain off the previous event"
        );
    }

    #[test]
    fn toggle_plugin_kind_round_trips() {
        use crate::proto::gait::log::v1::{payload, ToggleKind};
        let body = super::toggle("plugin", "myfmt", true);
        match kind_of(&body) {
            payload::Kind::Toggle(t) => {
                assert_eq!(t.kind, ToggleKind::Plugin as i32);
                assert_eq!(t.name, "myfmt");
                assert!(t.enabled);
            }
            other => panic!("expected Toggle, got {other:?}"),
        }
    }

    #[test]
    fn turn_complete_carries_changes() {
        use crate::proto::gait::log::v1::{payload, Changes, TurnTokenUsage};
        let usage = TurnTokenUsage {
            input: 0,
            output: 0,
            cache_read: 0,
            cache_write: 0,
        };
        let changes = Some(Changes {
            added: vec!["a.rs".into()],
            modified: vec![],
            deleted: vec![],
        });
        let body = turn_complete(TurnCompleteParams {
            usage,
            message: "done".into(),
            changes,
            ..Default::default()
        });
        let pb::event::Body::Payload(p) = body else {
            panic!()
        };
        let Some(payload::Kind::TurnComplete(tc)) = p.kind else {
            panic!()
        };
        assert_eq!(
            tc.output.unwrap().changes.unwrap().added,
            vec!["a.rs".to_string()]
        );
    }

    #[test]
    fn replace_body_targets_correct_event() {
        use crate::proto::gait::log::v1::{event, payload};
        let payload = pb::Payload {
            kind: Some(payload::Kind::AssistantText(pb::AssistantText {
                content: "new".into(),
            })),
        };
        let body = super::replace("target-id-123".into(), payload.clone(), Some("test".into()));
        match body {
            event::Body::Replace(r) => {
                assert_eq!(r.target, "target-id-123");
                assert_eq!(r.reason, Some("test".into()));
                assert!(r.replacement.is_some());
            }
            other => panic!("expected Replace, got {other:?}"),
        }
    }

    #[test]
    fn redact_body_targets_correct_event() {
        use crate::proto::gait::log::v1::event;
        let body = super::redact("target-id-456".into(), None);
        match body {
            event::Body::Redact(r) => {
                assert_eq!(r.target, "target-id-456");
                assert_eq!(r.reason, None);
            }
            other => panic!("expected Redact, got {other:?}"),
        }
    }

    #[test]
    fn unlock_round_trips_items() {
        use crate::proto::wire;
        use tempfile::TempDir;

        let dir = TempDir::new().unwrap();
        let path = dir.path().join("log.jsonl");

        let items = vec![pb::SandboxUnlockItem {
            id: "req-1".into(),
            tool_name: "write".into(),
            resource: "/etc/secrets".into(),
            dimension: "write".into(),
            preview: Some("--- old\n+++ new".into()),
        }];
        wire::append(&path, super::sandbox_unlock(&items)).expect("append unlock");

        let evs = wire::read_events(&path).expect("read events");
        assert_eq!(evs.len(), 1);
        match evs[0].body.as_ref().and_then(|b| match b {
            crate::proto::gait::log::v1::event::Body::Payload(p) => p.kind.as_ref(),
            _ => None,
        }) {
            Some(pb::payload::Kind::SandboxUnlock(u)) => {
                assert_eq!(u.items.len(), 1);
                let item = &u.items[0];
                assert_eq!(item.id, "req-1");
                assert_eq!(item.tool_name, "write");
                assert_eq!(item.resource, "/etc/secrets");
                assert_eq!(item.dimension, "write");
                assert_eq!(item.preview, Some("--- old\n+++ new".into()));
            }
            other => panic!("expected SandboxUnlock payload, got {other:?}"),
        }
    }

    #[test]
    fn unlock_reply_round_trips_items() {
        use crate::proto::wire;
        use tempfile::TempDir;

        let dir = TempDir::new().unwrap();
        let path = dir.path().join("log.jsonl");

        let items = vec![pb::SandboxUnlockReplyItem {
            id: "req-1".into(),
            decision: pb::Decision::Allow as i32,
            scope: pb::Scope::Once as i32,
        }];
        wire::append(&path, super::sandbox_unlock_reply(&items)).expect("append unlock_reply");

        let evs = wire::read_events(&path).expect("read events");
        assert_eq!(evs.len(), 1);
        match evs[0].body.as_ref().and_then(|b| match b {
            crate::proto::gait::log::v1::event::Body::Payload(p) => p.kind.as_ref(),
            _ => None,
        }) {
            Some(pb::payload::Kind::SandboxUnlockReply(ur)) => {
                assert_eq!(ur.items.len(), 1);
                let item = &ur.items[0];
                assert_eq!(item.id, "req-1");
                assert_eq!(item.decision, pb::Decision::Allow as i32);
                assert_eq!(item.scope, pb::Scope::Once as i32);
            }
            other => panic!("expected SandboxUnlockReply payload, got {other:?}"),
        }
    }

    #[test]
    fn sandbox_whitelist_match_round_trips_items() {
        let body = sandbox_whitelist_match(&[pb::SandboxWhitelistMatchItem {
            tool_name: "shell".into(),
            resource: "flutter test --coverage".into(),
            dimension: "network".into(),
            pattern: "flutter test *".into(),
        }]);
        match body {
            pb::event::Body::Payload(p) => match p.kind {
                Some(pb::payload::Kind::SandboxWhitelistMatch(m)) => {
                    assert_eq!(m.items[0].pattern, "flutter test *");
                    assert_eq!(m.items[0].dimension, "network");
                }
                other => panic!("expected whitelist match, got {other:?}"),
            },
            other => panic!("expected payload, got {other:?}"),
        }
    }

    #[test]
    fn sandbox_denied_event_shape() {
        let body = sandbox_denied(
            "shell",
            "flutter test ...",
            "network",
            "CERTIFICATE_VERIFY_FAILED",
            Some("try running verbatim".to_string()),
        );
        match body {
            pb::event::Body::Payload(p) => match p.kind {
                Some(pb::payload::Kind::SandboxDenied(sd)) => {
                    assert_eq!(sd.tool_name, "shell");
                    assert_eq!(sd.dimension, "network");
                    assert_eq!(sd.hint.as_deref(), Some("try running verbatim"));
                }
                other => panic!("expected SandboxDenied, got {other:?}"),
            },
            other => panic!("expected Payload body, got {other:?}"),
        }
    }

    #[test]
    fn permission_denied_event_shape() {
        let body = permission_denied("shell", "rm -rf /", "shell:rm -rf *");
        match body {
            pb::event::Body::Payload(p) => match p.kind {
                Some(pb::payload::Kind::PermissionDenied(pd)) => {
                    assert_eq!(pd.tool_name, "shell");
                    assert_eq!(pd.pattern, "shell:rm -rf *");
                }
                other => panic!("expected PermissionDenied, got {other:?}"),
            },
            other => panic!("expected Payload body, got {other:?}"),
        }
    }

    #[test]
    fn mode_event_roundtrips_and_empty_name_is_clear_sentinel() {
        let body = super::mode("plan", vec!["write".into()], "no edits", "d");
        match body {
            crate::proto::gait::log::v1::event::Body::Payload(p) => match p.kind {
                Some(pb::payload::Kind::Mode(m)) => {
                    assert_eq!(m.name, "plan");
                    assert_eq!(m.deny, vec!["write".to_string()]);
                    assert_eq!(m.steer, "no edits");
                    assert_eq!(m.description, "d");
                }
                other => panic!("expected Mode, got {other:?}"),
            },
            other => panic!("expected Payload body, got {other:?}"),
        }

        let clear = super::mode("", vec![], "", "");
        match clear {
            crate::proto::gait::log::v1::event::Body::Payload(p) => match p.kind {
                Some(pb::payload::Kind::Mode(m)) => assert_eq!(m.name, ""),
                other => panic!("expected Mode, got {other:?}"),
            },
            other => panic!("expected Payload body, got {other:?}"),
        }
    }

    #[test]
    fn session_start_carries_codebase_path() {
        let body = super::session_start(
            "s1234567".to_string(),
            Some("sparent0".to_string()),
            Some("/code".to_string()),
            false,
        );
        match body {
            crate::proto::gait::log::v1::event::Body::Payload(p) => match p.kind {
                Some(crate::proto::gait::log::v1::payload::Kind::SessionStart(s)) => {
                    assert_eq!(s.session_id, "s1234567");
                    assert_eq!(s.assigned_by.as_deref(), Some("sparent0"));
                    assert_eq!(s.codebase_path.as_deref(), Some("/code"));
                }
                other => panic!("expected SessionStart, got {other:?}"),
            },
            other => panic!("expected Payload body, got {other:?}"),
        }
    }
}

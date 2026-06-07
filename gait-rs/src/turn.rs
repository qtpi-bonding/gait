use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::collections::HashMap;
use futures_util::StreamExt;
use crate::provider::{Provider, StreamChunk};
use crate::types::{Message, MessageContent, AssistantContent, AssistantPart, ToolCall, ToolResult, ToolResultKind, Usage, ExecutionMode};
use crate::types::Tool;
use crate::session::transcript::{
    AnyEvent, append_event, emit_event, read_events, init_session, count_turn_complete, now_ms,
    InitSessionOpts, PermissionItem, PermissionsReplyItem, TurnUsage, TurnOutput,
    ClarifyItem, InformItem,
};
use crate::session::context::reconstruct_messages_from_events;
use crate::config::{McpToolDef, SkillDef};
use crate::git;
use crate::permission::{is_tool_allowed, unapproved_segments, command_patterns};

pub struct TurnOpts {
    pub session_id: String,
    pub task: Option<String>,
    pub auto: bool,
    pub workspace: PathBuf,
    pub codebase_path: PathBuf,
    pub allow: Vec<String>,
    pub system_prompt: String,
    pub model: String,  // stored in session_start event for wire-compat with TS gait
    pub mcp_registry: Vec<McpToolDef>,
    pub skill_registry: Vec<SkillDef>,
}

#[derive(Debug, Clone)]
pub enum TurnResult {
    Done { summary: String, result: Option<serde_json::Value> },
    Permissions,
    Clarify { items: Vec<ClarifyItem> },
    Inform  { items: Vec<InformItem> },
}

// ── Session lock (RAII PID file) ─────────────────────────────────────────────

pub struct SessionLock {
    path: PathBuf,
}

impl SessionLock {
    pub fn acquire(session_dir: &Path) -> anyhow::Result<Self> {
        let path = session_dir.join("lock");
        if path.exists() {
            let raw = std::fs::read_to_string(&path).unwrap_or_default();
            if let Ok(pid) = raw.trim().parse::<u32>() {
                // Check if that PID is still alive
                if std::path::Path::new(&format!("/proc/{}", pid)).exists()
                    || unsafe { libc::kill(pid as i32, 0) == 0 }
                {
                    anyhow::bail!("session {} is already running (PID {})", session_dir.display(), pid);
                }
            }
        }
        std::fs::write(&path, std::process::id().to_string())?;
        Ok(Self { path })
    }
}

impl Drop for SessionLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn build_turn_context(codebase_hash: &str, dirty_files: &[String]) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    let mut ctx = format!("Current time (unix): {}\nCodebase commit: {}\n", secs, codebase_hash);
    if !dirty_files.is_empty() {
        ctx.push_str("Dirty files:\n");
        for f in dirty_files { ctx.push_str(&format!("  {}\n", f)); }
    }
    ctx
}

/// Scan transcript events to derive which MCP tools are currently loaded.
/// Last call per tool name wins. Returns names of tools where context=true.
/// IMPORTANT: Pass only the post-compaction slice (from last_compaction_index forward).
/// Loads before the compaction breakpoint are intentionally forgotten — the model
/// reloads only what it still needs after compaction.
pub fn derive_loaded_mcps(events: &[AnyEvent]) -> Vec<String> {
    let mut state: HashMap<String, bool> = HashMap::new();
    for event in events {
        if let AnyEvent::ToolCall { name, input, .. } = event {
            if name == "mcp" {
                if let (Some(tool_name), Some(context)) = (
                    input.get("name").and_then(|v| v.as_str()),
                    input.get("context").and_then(|v| v.as_bool()),
                ) {
                    state.insert(tool_name.to_string(), context);
                }
            }
        }
    }
    state.into_iter().filter(|(_, loaded)| *loaded).map(|(name, _)| name).collect()
}

/// Same as derive_loaded_mcps but for skills.
pub fn derive_loaded_skills(events: &[AnyEvent]) -> Vec<String> {
    let mut state: HashMap<String, bool> = HashMap::new();
    for event in events {
        if let AnyEvent::ToolCall { name, input, .. } = event {
            if name == "skill" {
                if let (Some(skill_name), Some(context)) = (
                    input.get("name").and_then(|v| v.as_str()),
                    input.get("context").and_then(|v| v.as_bool()),
                ) {
                    state.insert(skill_name.to_string(), context);
                }
            }
        }
    }
    state.into_iter().filter(|(_, loaded)| *loaded).map(|(name, _)| name).collect()
}

/// Build the <mcp-index> system prompt section. Returns empty string if registry is empty.
pub fn build_mcp_index(registry: &[McpToolDef]) -> String {
    if registry.is_empty() { return String::new(); }
    let lines: Vec<String> = registry.iter()
        .map(|d| format!("{} — {}", d.name, d.description))
        .collect();
    format!("\n<mcp-index>\n{}\n</mcp-index>", lines.join("\n"))
}

/// Build the <skill-index> system prompt section. Returns empty string if registry is empty.
pub fn build_skill_index(registry: &[SkillDef]) -> String {
    if registry.is_empty() { return String::new(); }
    let lines: Vec<String> = registry.iter()
        .map(|d| format!("{} — {}", d.name, d.description))
        .collect();
    format!("\n<skill-index>\n{}\n</skill-index>", lines.join("\n"))
}

fn make_mcp_tool_def(
    def: &McpToolDef,
    workspace: &std::path::Path,
    codebase_path: &std::path::Path,
) -> Box<dyn crate::types::Tool> {
    let cmd = def.cmd.clone();
    let cwd = if def.cwd == "workspace" {
        workspace.to_path_buf()
    } else {
        codebase_path.to_path_buf()
    };
    let name = def.name.clone();
    let description = def.description.clone();
    let schema = def.schema.clone();
    Box::new(crate::types::ToolDef {
        name,
        description,
        parameters: schema,
        execute: Box::new(move |_args| {
            let cmd = cmd.clone();
            let cwd = cwd.clone();
            Box::pin(async move {
                let output = tokio::process::Command::new("sh")
                    .arg("-c").arg(&cmd)
                    .current_dir(&cwd)
                    .output().await
                    .map_err(|e| anyhow::anyhow!("{}", e))?;
                let stdout = String::from_utf8_lossy(&output.stdout).to_string();
                let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                let combined = if stderr.is_empty() { stdout }
                    else { format!("{}\nstderr:\n{}", stdout, stderr) };
                Ok(serde_json::Value::String(combined))
            })
        }),
    })
}

/// Execute one tool call: emit ToolCall event, run the tool, emit ToolResult event.
/// Used identically by the normal execution loop and the permission-resume path.
/// Does NOT handle output/mcp/skill special tools — those are never gated.
async fn execute_tool_call(
    tc: &ToolCall,
    tools: &[Box<dyn Tool>],
    tx_path: &std::path::Path,
) -> anyhow::Result<(AssistantPart, ToolResult)> {
    let id = tc.tool_call_id.clone();
    emit_event(tx_path, &AnyEvent::ToolCall {
        id: id.clone(),
        name: tc.tool_name.clone(),
        input: tc.args.clone(),
        ts: now_ms(),
    })?;
    let assistant_part = AssistantPart::ToolCall(ToolCall {
        tool_call_id: id.clone(),
        tool_name: tc.tool_name.clone(),
        args: tc.args.clone(),
    });
    let output = match tools.iter().find(|t| t.name() == tc.tool_name) {
        Some(t) => match t.execute(tc.args.clone()).await {
            Ok(v) => if v.is_string() { v.as_str().unwrap().to_owned() } else { v.to_string() },
            Err(e) => format!("error: {e}"),
        },
        None => format!("unknown tool: {}", tc.tool_name),
    };
    emit_event(tx_path, &AnyEvent::ToolResult {
        id: id.clone(), output: output.clone(), ts: now_ms(),
    })?;
    let tool_result = ToolResult {
        kind: ToolResultKind::ToolResult,
        tool_call_id: id,
        result: serde_json::Value::String(output),
    };
    Ok((assistant_part, tool_result))
}

/// Build a (AssistantPart, ToolResult) pair representing a tool-call error
/// WITHOUT emitting events. Use when `execute_tool_call` fails — the model
/// still gets a result back so the turn can continue.
fn error_tool_result(tc: &ToolCall, e: anyhow::Error) -> (AssistantPart, ToolResult) {
    let output = format!("error: {e}");
    (
        AssistantPart::ToolCall(tc.clone()),
        ToolResult {
            kind: ToolResultKind::ToolResult,
            tool_call_id: tc.tool_call_id.clone(),
            result: serde_json::Value::String(output),
        },
    )
}

pub async fn run_turn(
    opts: TurnOpts,
    provider: Arc<dyn Provider>,
    tools: Vec<Box<dyn Tool>>,
) -> anyhow::Result<TurnResult> {
    git::create_session_worktree(&opts.workspace, &opts.session_id)?;

    let session_dir = opts.workspace.join("sessions").join(&opts.session_id);
    let tx_path = session_dir.join("transcript.jsonl");

    let _lock = SessionLock::acquire(&session_dir)?;

    let codebase_hash = git::get_codebase_hash(&opts.codebase_path);
    let dirty_files = git::get_dirty_files(&opts.codebase_path);
    let codebase_dirty = !dirty_files.is_empty();

    // Init transcript on first turn
    let existing = std::fs::read_to_string(&tx_path).unwrap_or_default();
    if existing.trim().is_empty() {
        init_session(InitSessionOpts {
            transcript_path: tx_path.clone(),
            id: opts.session_id.clone(),
            model: opts.model.clone(),
            codebase_hash: codebase_hash.clone(),
            codebase_dirty,
        })?;
    }

    // Read and delete once-approval temp file (written by gait chat after user approval)
    let once_path = opts.workspace.join("tmp").join(format!("gait-approve-{}", opts.session_id));
    let once_allow: Vec<String> = std::fs::read_to_string(&once_path)
        .unwrap_or_default()
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    let _ = std::fs::remove_file(&once_path);
    let mut effective_allow = opts.allow.clone();
    effective_allow.extend(once_allow);

    // Detect resume after permission approval: last two transcript events are
    // Permissions → PermissionsReply. On resume we directly execute the
    // approved tools (or emit denied ToolCall/ToolResult for rejected ones),
    // re-read the transcript, and reconstruct messages — all without an
    // extra LLM round-trip.
    let pre_events = read_events(&tx_path)?;
    let mut messages: Vec<Message> = Vec::new();
    let mut tools = tools; // take ownership so we can mutate (mcp load/unload)
    let did_resume = if matches!(pre_events.last(), Some(AnyEvent::PermissionsReply { .. })) {
        // Find the Permissions event immediately preceding the reply
        let perm_event = pre_events.iter().rev()
            .skip(1) // skip PermissionsReply
            .find(|e| matches!(e, AnyEvent::Permissions { .. }));
        let reply_event = pre_events.last();

        if let (
            Some(AnyEvent::Permissions { items: perm_items, .. }),
            Some(AnyEvent::PermissionsReply { items: reply_items, .. }),
        ) = (perm_event, reply_event) {
            // Build a map: id → decision
            let decisions: std::collections::HashMap<&str, &str> = reply_items.iter()
                .map(|r| (r.id.as_str(), r.decision.as_str()))
                .collect();

            // Deduplicate by id: collect unique tool_call_ids and their items
            let mut by_id: std::collections::BTreeMap<String, Vec<&PermissionItem>> =
                std::collections::BTreeMap::new();
            for item in perm_items {
                by_id.entry(item.id.clone()).or_default().push(item);
            }

            // Execute or deny each unique tool call
            for (id, items) in &by_id {
                // All-or-nothing: all segments of this tool_call_id must be approved
                let all_approved = items.iter().all(|item| {
                    decisions.get(item.id.as_str()).copied() == Some("approve")
                });

                if all_approved {
                    // Execute using original tool_args from the first item (all share same args)
                    let tc = ToolCall {
                        tool_call_id: id.clone(),
                        tool_name: items[0].tool_name.clone(),
                        args: items[0].tool_args.clone(),
                    };
                    if let Err(e) = execute_tool_call(&tc, &tools, &tx_path).await {
                        // Emit error as tool result so model sees it
                        emit_event(&tx_path, &AnyEvent::ToolResult {
                            id: id.clone(),
                            output: format!("error executing approved tool: {e}"),
                            ts: now_ms(),
                        })?;
                    }
                } else {
                    // At least one segment denied — emit ToolCall FIRST, then ToolResult.
                    // Without a preceding ToolCall, context.rs silently drops the orphan
                    // ToolResult during reconstruction, and the model retries forever (gate loop).
                    emit_event(&tx_path, &AnyEvent::ToolCall {
                        id: id.clone(),
                        name: items[0].tool_name.clone(),
                        input: items[0].tool_args.clone(),
                        ts: now_ms(),
                    })?;
                    emit_event(&tx_path, &AnyEvent::ToolResult {
                        id: id.clone(),
                        output: "permission denied by user".into(),
                        ts: now_ms(),
                    })?;
                }
            }

            // Re-read events after execution and reconstruct messages.
            // This is the ONLY source of truth — do not manually push to messages.
            let events_after = read_events(&tx_path)?;
            messages = reconstruct_messages_from_events(&events_after)?;
            // Do NOT inject UserTurn or turn_ctx — model already has context from the prior turn.
            true
        } else {
            false
        }
    } else {
        false
    };

    if !did_resume {
        if let Some(task) = &opts.task {
            emit_event(&tx_path, &AnyEvent::UserTurn {
                content: task.clone(),
                from: None,
                context: Some(crate::session::transcript::UserTurnContext {
                    codebase_hash: Some(codebase_hash.clone()),
                    codebase_dirty: Some(codebase_dirty),
                    dirty_files: if dirty_files.is_empty() { None } else { Some(dirty_files.clone()) },
                    injected_files: None,
                }),
                ts: now_ms(),
            })?;
        }
        let events = read_events(&tx_path)?;
        messages = reconstruct_messages_from_events(&events)?;
        let turn_ctx = build_turn_context(&codebase_hash, &dirty_files);
        messages.insert(0, Message::User { content: MessageContent::Text(turn_ctx) });
    }

    // Derive loaded MCP tools from the POST-COMPACTION slice only.
    // Pre-compaction loads are intentionally forgotten (spec: model reloads after compaction).
    let events = read_events(&tx_path)?;
    let compaction_start = crate::session::transcript::last_compaction_index(&events).unwrap_or(0);
    let post_compaction = &events[compaction_start..];
    let loaded_mcp_names = derive_loaded_mcps(post_compaction);
    for name in &loaded_mcp_names {
        if let Some(def) = opts.mcp_registry.iter().find(|d| d.name == *name) {
            tools.push(make_mcp_tool_def(def, &opts.workspace, &opts.codebase_path));
        }
    }

    // Augment system prompt with MCP/skill indexes
    let mcp_idx = build_mcp_index(&opts.mcp_registry);
    let skill_idx = build_skill_index(&opts.skill_registry);
    let system_prompt = if mcp_idx.is_empty() && skill_idx.is_empty() {
        opts.system_prompt.clone()
    } else {
        format!("{}{}{}", opts.system_prompt, mcp_idx, skill_idx)
    };

    let mut total_usage = Usage::default();
    let mut output_signal: Option<TurnOutput> = None;
    let mut clarify_signal: Option<Vec<ClarifyItem>> = None;
    let mut inform_signal: Option<Vec<InformItem>> = None;

    for _step in 0..100 {
        let mut chunk_stream = provider.stream(&system_prompt, &messages, &tools).await
            .map_err(|e| anyhow::anyhow!("provider error: {}", e))?;

        let mut step_text = String::new();
        let mut step_tool_calls: Vec<ToolCall> = vec![];

        while let Some(chunk) = chunk_stream.next().await {
            match chunk.map_err(|e| anyhow::anyhow!("stream error: {}", e))? {
                StreamChunk::Text(t) => {
                    println!("{}", serde_json::json!({"type": "text_delta", "content": t, "ts": now_ms()}));
                    step_text.push_str(&t);
                }
                StreamChunk::ToolCallComplete { tool_call_id, tool_name, args } => {
                    step_tool_calls.push(ToolCall { tool_call_id, tool_name, args });
                }
                StreamChunk::StepUsage(u) => {
                    total_usage = total_usage.add(&u);
                }
                StreamChunk::Done => break,
                _ => {}
            }
        }

        // Flush assistant text — only as a standalone event when there are NO tool calls.
        // When tool calls are present, the text is included as AssistantPart::Text in the
        // Parts message below. Writing a separate AssistantText event here would produce
        // two consecutive assistant messages in context reconstruction, which Anthropic rejects.
        if !step_text.is_empty() && step_tool_calls.is_empty() {
            emit_event(&tx_path, &AnyEvent::AssistantText {
                content: step_text.clone(),
                ts: now_ms(),
            })?;
        }

        // No tool calls → turn complete
        if step_tool_calls.is_empty() {
            let events = read_events(&tx_path)?;
            let turn_n = count_turn_complete(&events) + 1;
            let summary = output_signal.as_ref()
                .map(|o| o.summary.clone())
                .unwrap_or_else(|| step_text.lines().next().unwrap_or("").to_string());
            let output_for_event = output_signal.take().unwrap_or_else(|| TurnOutput {
                summary: summary.clone(),
                result: None,
            });
            let done_result = output_for_event.result.clone();
            emit_event(&tx_path, &AnyEvent::TurnComplete {
                usage: TurnUsage {
                    input: total_usage.input_tokens as u64,   // Usage uses u32; TurnUsage uses u64
                    output: total_usage.output_tokens as u64,
                    cache_read: 0,
                    cache_write: 0,
                },
                codebase_hash: codebase_hash.clone(),
                codebase_dirty,
                output: output_for_event,
                ts: now_ms(),
            })?;
            git::commit_session(&opts.workspace, &opts.session_id,
                &format!("session({}): turn {}", opts.session_id, turn_n))?;
            return Ok(TurnResult::Done {
                summary,
                result: done_result,
            });
        }

        // Single permission gate: classify all tool calls into allowed vs gated.
        // Allowed tools execute below (parallel batch + serial). Gated tools either
        // trigger a Permissions exit (non-auto) or never exist (auto mode, where
        // is_tool_allowed is never consulted and all tools are allowed).
        // B2: allowed tools execute and land in the transcript before we potentially
        // exit for permissions — their results survive any subsequent permission exit.
        let mut pending_permissions: Vec<PermissionItem> = vec![];
        let mut gated_ids: std::collections::HashSet<String> = Default::default();

        for tc in &step_tool_calls {
            if !(opts.auto || is_tool_allowed(&tc.tool_name, &tc.args, &effective_allow)) {
                gated_ids.insert(tc.tool_call_id.clone());
                if tc.tool_name == "shell" {
                    let cmd = tc.args.get("cmd").and_then(|v| v.as_str()).unwrap_or("");
                    for seg in unapproved_segments(cmd, &effective_allow) {
                        pending_permissions.push(PermissionItem {
                            id: tc.tool_call_id.clone(),
                            cmd: seg.clone(),
                            reason: "shell command requires permission".into(),
                            patterns: command_patterns(&seg),
                            tool_name: tc.tool_name.clone(),
                            tool_args: tc.args.clone(),
                        });
                    }
                } else {
                    pending_permissions.push(PermissionItem {
                        id: tc.tool_call_id.clone(),
                        cmd: tc.tool_name.clone(),
                        reason: format!("{} requires permission", tc.tool_name),
                        patterns: vec![tc.tool_name.clone()],
                        tool_name: tc.tool_name.clone(),
                        tool_args: tc.args.clone(),
                    });
                }
            }
        }

        // Execute all allowed tool calls. Results are collected by original index
        // so the assistant_parts / tool_results vectors stay in call order.
        let mut tool_results: Vec<ToolResult> = vec![];
        let mut assistant_parts: Vec<AssistantPart> = vec![];

        if !step_text.is_empty() {
            assistant_parts.push(AssistantPart::Text { text: step_text.clone() });
        }

        {
            let allowed: Vec<(usize, &ToolCall)> = step_tool_calls.iter().enumerate()
                .filter(|(_, tc)| !gated_ids.contains(&tc.tool_call_id))
                .collect();

            // Special-signal tools must be serial (they mutate turn state or the tools vec).
            // Everything else with ExecutionMode::Parallel runs as a concurrent batch.
            let is_parallel_eligible = |tc: &ToolCall| -> bool {
                if matches!(tc.tool_name.as_str(), "clarify" | "inform" | "output" | "mcp" | "skill") {
                    return false;
                }
                tools.iter()
                    .find(|t| t.name() == tc.tool_name)
                    .map(|t| t.execution_mode() == ExecutionMode::Parallel)
                    .unwrap_or(false)
            };

            let (parallel_items, serial_items): (Vec<_>, Vec<_>) = allowed.iter()
                .partition(|(_, tc)| is_parallel_eligible(tc));

            // Parallel batch — all futures share &tools (immutable), safe to join
            let par_raw = futures_util::future::join_all(
                parallel_items.iter().map(|(_, tc)| execute_tool_call(tc, &tools, &tx_path))
            ).await;

            // Serial items — run in original order, handle special cases inline
            let mut all_results: Vec<Option<(AssistantPart, ToolResult)>> =
                vec![None; step_tool_calls.len()];

            for ((i, tc), raw) in parallel_items.iter().zip(par_raw) {
                all_results[*i] = Some(raw.unwrap_or_else(|e| error_tool_result(tc, e)));
            }

            for (i, tc) in &serial_items {
                match tc.tool_name.as_str() {
                    "mcp" => {
                        if let (Some(name), Some(ctx)) = (
                            tc.args.get("name").and_then(|v| v.as_str()),
                            tc.args.get("context").and_then(|v| v.as_bool()),
                        ) {
                            if ctx {
                                if let Some(def) = opts.mcp_registry.iter().find(|d| d.name == name) {
                                    if !tools.iter().any(|t| t.name() == name) {
                                        tools.push(make_mcp_tool_def(def, &opts.workspace, &opts.codebase_path));
                                    }
                                }
                            } else {
                                tools.retain(|t| t.name() != name);
                            }
                        }
                        let r = execute_tool_call(tc, &tools, &tx_path).await
                            .unwrap_or_else(|e| error_tool_result(tc, e));
                        all_results[*i] = Some(r);
                    }
                    "output" => {
                        if let Some(summary) = tc.args.get("summary").and_then(|v| v.as_str()) {
                            output_signal = Some(TurnOutput {
                                summary: summary.to_string(),
                                result: tc.args.get("result").cloned(),
                            });
                        }
                        let r = execute_tool_call(tc, &tools, &tx_path).await
                            .unwrap_or_else(|e| error_tool_result(tc, e));
                        all_results[*i] = Some(r);
                    }
                    "clarify" => {
                        let items: Vec<ClarifyItem> = tc.args.get("items")
                            .and_then(|v| serde_json::from_value(v.clone()).ok())
                            .unwrap_or_default();
                        emit_event(&tx_path, &AnyEvent::Clarify { items: items.clone(), ts: now_ms() })?;
                        clarify_signal = Some(items);
                        // no tool result — clarify exits the turn
                    }
                    "inform" => {
                        let items: Vec<InformItem> = tc.args.get("items")
                            .and_then(|v| serde_json::from_value(v.clone()).ok())
                            .unwrap_or_default();
                        emit_event(&tx_path, &AnyEvent::Inform { items: items.clone(), ts: now_ms() })?;
                        inform_signal = Some(items);
                        // no tool result — inform exits the turn
                    }
                    _ => {
                        // skill and all other serial tools
                        let r = execute_tool_call(tc, &tools, &tx_path).await
                            .unwrap_or_else(|e| error_tool_result(tc, e));
                        all_results[*i] = Some(r);
                    }
                }
            }

            for r in all_results {
                if let Some((part, result)) = r {
                    assistant_parts.push(part);
                    tool_results.push(result);
                }
            }
        }

        // Gated tools: emit Permissions and exit so the user can decide.
        // (In auto mode gated_ids is always empty — this block is never reached.)
        if !pending_permissions.is_empty() {
            emit_event(&tx_path, &AnyEvent::Permissions {
                items: pending_permissions,
                ts: now_ms(),
            })?;
            git::commit_session(&opts.workspace, &opts.session_id,
                &format!("session({}): permissions", opts.session_id))?;
            return Ok(TurnResult::Permissions);
        }

        // Exit turn for clarify/inform — event already in transcript+stdout via emit_event
        if let Some(items) = clarify_signal.take() {
            git::commit_session(&opts.workspace, &opts.session_id,
                &format!("session({}): clarify", opts.session_id))?;
            return Ok(TurnResult::Clarify { items });
        }
        if let Some(items) = inform_signal.take() {
            git::commit_session(&opts.workspace, &opts.session_id,
                &format!("session({}): inform", opts.session_id))?;
            return Ok(TurnResult::Inform { items });
        }

        // Append this step to messages for next iteration
        messages.push(Message::Assistant { content: AssistantContent::Parts(assistant_parts) });
        messages.push(Message::Tool { content: tool_results });
    }

    anyhow::bail!("reached max steps (100) without turn_complete")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use crate::provider::ProviderError;
    use crate::types::ToolDef;
    use async_stream::stream;

    fn init_git_repo(dir: &Path) {
        let repo = git2::Repository::init(dir).unwrap();
        let mut cfg = repo.config().unwrap();
        cfg.set_str("user.name", "test").unwrap();
        cfg.set_str("user.email", "test@test.com").unwrap();
        let sig = git2::Signature::now("test", "test@test.com").unwrap();
        let tree_id = repo.index().unwrap().write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[]).unwrap();
        let _ = repo.find_branch("master", git2::BranchType::Local)
            .or_else(|_| repo.find_branch("main", git2::BranchType::Local))
            .and_then(|mut b| { b.rename("main", false)?; Ok(()) });
    }

    struct MockProvider {
        responses: Arc<Vec<Vec<StreamChunk>>>,
        call_idx: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl Provider for MockProvider {
        fn model_id(&self) -> &str { "mock" }
        async fn stream(
            &self, _system: &str, _messages: &[Message], _tools: &[Box<dyn Tool>],
        ) -> Result<std::pin::Pin<Box<dyn futures_util::Stream<Item = Result<StreamChunk, ProviderError>> + Send>>, ProviderError> {
            let idx = self.call_idx.fetch_add(1, Ordering::SeqCst);
            let chunks = self.responses.get(idx).map(|v| v.as_slice()).unwrap_or(&[]);
            let chunks_vec: Vec<StreamChunk> = chunks.to_vec();
            Ok(Box::pin(stream! {
                for c in chunks_vec { yield Ok(c); }
            }))
        }
    }

    #[tokio::test]
    async fn run_turn_writes_transcript_and_commits() {
        let ws = TempDir::new().unwrap();
        init_git_repo(ws.path());
        // Codebase dir (can be same as workspace for test)
        let codebase = ws.path().to_path_buf();

        let provider = Arc::new(MockProvider {
            responses: Arc::new(vec![
                // Step 1: call the read tool
                vec![
                    StreamChunk::ToolCallComplete {
                        tool_call_id: "tc1".into(),
                        tool_name: "read".into(),
                        args: serde_json::json!({ "file_path": "README.md" }),
                    },
                    StreamChunk::Done,
                ],
                // Step 2: final text response
                vec![
                    StreamChunk::Text("Task complete.".into()),
                    StreamChunk::Done,
                ],
            ]),
            call_idx: Arc::new(AtomicUsize::new(0)),
        });

        let read_tool = ToolDef {
            name: "read".into(),
            description: "read a file".into(),
            parameters: serde_json::json!({ "type": "object" }),
            execute: Box::new(|_args| Box::pin(async move {
                Ok(serde_json::Value::String("file contents".into()))
            })),
        };

        run_turn(
            TurnOpts {
                session_id: "stest001".into(),
                task: Some("do the thing".into()),
                auto: true,
                workspace: ws.path().to_path_buf(),
                codebase_path: codebase,
                allow: vec!["read".into()],
                system_prompt: "you are a helpful assistant".into(),
                model: "mock/test".into(),
                mcp_registry: vec![],
                skill_registry: vec![],
            },
            provider,
            vec![Box::new(read_tool)],
        ).await.unwrap();

        // Verify transcript has expected events
        let tx_path = ws.path().join("sessions/stest001/transcript.jsonl");
        let events = read_events(&tx_path).unwrap();
        let types: Vec<&str> = events.iter().map(|e| match e {
            AnyEvent::SessionStart { .. } => "session_start",
            AnyEvent::UserTurn { .. } => "user_turn",
            AnyEvent::ToolCall { .. } => "tool_call",
            AnyEvent::ToolResult { .. } => "tool_result",
            AnyEvent::AssistantText { .. } => "assistant_text",
            AnyEvent::TurnComplete { .. } => "turn_complete",
            _ => "other",
        }).collect();
        assert!(types.contains(&"session_start"), "got: {:?}", types);
        assert!(types.contains(&"user_turn"), "got: {:?}", types);
        assert!(types.contains(&"tool_call"), "got: {:?}", types);
        assert!(types.contains(&"tool_result"), "got: {:?}", types);
        assert!(types.contains(&"turn_complete"), "got: {:?}", types);

        // Verify git commit was created
        let repo = git2::Repository::open(ws.path().join("sessions/stest001")).unwrap();
        let msg = repo.head().unwrap().peel_to_commit().unwrap().message().unwrap().to_string();
        assert!(msg.contains("turn 1"), "got: {msg}");
    }

    #[tokio::test]
    async fn output_tool_result_used_in_turn_complete() {
        let ws = TempDir::new().unwrap();
        init_git_repo(ws.path());

        let provider = Arc::new(MockProvider {
            responses: Arc::new(vec![
                vec![
                    StreamChunk::ToolCallComplete {
                        tool_call_id: "tc1".into(),
                        tool_name: "output".into(),
                        args: serde_json::json!({ "summary": "fixed the bug", "result": { "files": 2 } }),
                    },
                    StreamChunk::Done,
                ],
                vec![
                    StreamChunk::Text("".into()),
                    StreamChunk::Done,
                ],
            ]),
            call_idx: Arc::new(AtomicUsize::new(0)),
        });

        let output_tool = ToolDef {
            name: "output".into(),
            description: "signal done".into(),
            parameters: serde_json::json!({ "type": "object" }),
            execute: Box::new(|_args| Box::pin(async move {
                Ok(serde_json::Value::String("recorded".into()))
            })),
        };

        run_turn(
            TurnOpts {
                session_id: "stest_out".into(),
                task: Some("fix a bug".into()),
                auto: true,
                workspace: ws.path().to_path_buf(),
                codebase_path: ws.path().to_path_buf(),
                allow: vec![],
                system_prompt: "".into(),
                model: "mock/test".into(),
                mcp_registry: vec![],
                skill_registry: vec![],
            },
            provider,
            vec![Box::new(output_tool)],
        ).await.unwrap();

        let tx_path = ws.path().join("sessions/stest_out/transcript.jsonl");
        let events = read_events(&tx_path).unwrap();
        let tc_event = events.iter().find(|e| matches!(e, AnyEvent::TurnComplete { .. })).unwrap();
        if let AnyEvent::TurnComplete { output, .. } = tc_event {
            assert_eq!(output.summary, "fixed the bug");
            assert!(output.result.is_some());
        } else {
            panic!("expected TurnComplete");
        }
    }

    #[tokio::test]
    async fn output_tool_exempt_from_permission_gating() {
        let ws = TempDir::new().unwrap();
        init_git_repo(ws.path());

        let provider = Arc::new(MockProvider {
            responses: Arc::new(vec![
                vec![
                    StreamChunk::ToolCallComplete {
                        tool_call_id: "tc1".into(),
                        tool_name: "output".into(),
                        args: serde_json::json!({ "summary": "done" }),
                    },
                    StreamChunk::Done,
                ],
                vec![
                    StreamChunk::Text("".into()),
                    StreamChunk::Done,
                ],
            ]),
            call_idx: Arc::new(AtomicUsize::new(0)),
        });

        let output_tool = ToolDef {
            name: "output".into(),
            description: "signal done".into(),
            parameters: serde_json::json!({ "type": "object" }),
            execute: Box::new(|_args| Box::pin(async move {
                Ok(serde_json::Value::String("recorded".into()))
            })),
        };

        run_turn(
            TurnOpts {
                session_id: "stest_out2".into(),
                task: Some("do it".into()),
                auto: false,
                workspace: ws.path().to_path_buf(),
                codebase_path: ws.path().to_path_buf(),
                allow: vec![],
                system_prompt: "".into(),
                model: "mock/test".into(),
                mcp_registry: vec![],
                skill_registry: vec![],
            },
            provider,
            vec![Box::new(output_tool)],
        ).await.unwrap();

        let tx_path = ws.path().join("sessions/stest_out2/transcript.jsonl");
        let events = read_events(&tx_path).unwrap();
        let has_tc = events.iter().any(|e| matches!(e, AnyEvent::TurnComplete { .. }));
        let has_permissions = events.iter().any(|e| matches!(e, AnyEvent::Permissions { .. }));
        assert!(has_tc, "output tool should complete the turn");
        assert!(!has_permissions, "output tool should not trigger permission gating");
    }

    #[tokio::test]
    async fn permission_gating_pauses_turn_in_non_auto_mode() {
        let ws = TempDir::new().unwrap();
        init_git_repo(ws.path());

        let provider = Arc::new(MockProvider {
            responses: Arc::new(vec![
                vec![
                    StreamChunk::ToolCallComplete {
                        tool_call_id: "tc1".into(),
                        tool_name: "shell".into(), // not in allow list
                        args: serde_json::json!({ "cmd": "rm -rf /", "reason": "cleanup" }),
                    },
                    StreamChunk::Done,
                ],
            ]),
            call_idx: Arc::new(AtomicUsize::new(0)),
        });

        run_turn(
            TurnOpts {
                session_id: "stest002".into(),
                task: Some("do something dangerous".into()),
                auto: false, // non-auto
                workspace: ws.path().to_path_buf(),
                codebase_path: ws.path().to_path_buf(),
                allow: vec!["read".into()], // shell is NOT allowed
                system_prompt: "".into(),
                model: "mock/test".into(),
                mcp_registry: vec![],
                skill_registry: vec![],
            },
            provider,
            vec![],
        ).await.unwrap();

        let tx_path = ws.path().join("sessions/stest002/transcript.jsonl");
        let events = read_events(&tx_path).unwrap();
        let has_permissions = events.iter().any(|e| matches!(e, AnyEvent::Permissions { .. }));
        let has_turn_complete = events.iter().any(|e| matches!(e, AnyEvent::TurnComplete { .. }));
        assert!(has_permissions, "should have permissions event");
        assert!(!has_turn_complete, "should NOT have turn_complete — turn was paused");
    }

    #[test]
    fn derive_loaded_mcps_empty_events() {
        let events: Vec<AnyEvent> = vec![];
        let loaded = derive_loaded_mcps(&events);
        assert!(loaded.is_empty());
    }

    #[test]
    fn derive_loaded_mcps_single_load() {
        let events = vec![
            AnyEvent::ToolCall {
                id: "tc1".into(),
                name: "mcp".into(),
                input: serde_json::json!({ "name": "slack_send", "context": true }),
                ts: 1,
            },
        ];
        let loaded = derive_loaded_mcps(&events);
        assert_eq!(loaded.len(), 1);
        assert!(loaded.contains(&"slack_send".to_string()));
    }

    #[test]
    fn derive_loaded_mcps_load_then_unload() {
        let events = vec![
            AnyEvent::ToolCall {
                id: "tc1".into(), name: "mcp".into(),
                input: serde_json::json!({ "name": "slack_send", "context": true }), ts: 1,
            },
            AnyEvent::ToolCall {
                id: "tc2".into(), name: "mcp".into(),
                input: serde_json::json!({ "name": "slack_send", "context": false }), ts: 2,
            },
        ];
        let loaded = derive_loaded_mcps(&events);
        assert!(loaded.is_empty(), "should be empty after unload, got: {:?}", loaded);
    }

    #[test]
    fn derive_loaded_mcps_multiple_tools() {
        let events = vec![
            AnyEvent::ToolCall {
                id: "tc1".into(), name: "mcp".into(),
                input: serde_json::json!({ "name": "slack_send", "context": true }), ts: 1,
            },
            AnyEvent::ToolCall {
                id: "tc2".into(), name: "mcp".into(),
                input: serde_json::json!({ "name": "notion_create", "context": true }), ts: 2,
            },
            AnyEvent::ToolCall {
                id: "tc3".into(), name: "mcp".into(),
                input: serde_json::json!({ "name": "slack_send", "context": false }), ts: 3,
            },
        ];
        let loaded = derive_loaded_mcps(&events);
        assert_eq!(loaded.len(), 1);
        assert!(loaded.contains(&"notion_create".to_string()));
    }

    #[test]
    fn build_mcp_index_empty() {
        use crate::config::McpToolDef;
        let index = build_mcp_index(&[]);
        assert!(index.is_empty());
    }

    #[test]
    fn build_mcp_index_formats_correctly() {
        use crate::config::McpToolDef;
        let registry = vec![
            McpToolDef { name: "slack_send".into(), description: "post to Slack".into(),
                schema: serde_json::json!({}), cmd: "node s.js".into(), cwd: "workspace".into() },
        ];
        let index = build_mcp_index(&registry);
        assert!(index.contains("<mcp-index>"), "got: {index}");
        assert!(index.contains("slack_send"), "got: {index}");
        assert!(index.contains("post to Slack"), "got: {index}");
        assert!(index.contains("</mcp-index>"), "got: {index}");
    }

    #[test]
    fn build_skill_index_formats_correctly() {
        use crate::config::SkillDef;
        let registry = vec![
            SkillDef { name: "tdd".into(), description: "TDD workflow".into(), content: "...".into() },
        ];
        let index = build_skill_index(&registry);
        assert!(index.contains("<skill-index>"), "got: {index}");
        assert!(index.contains("tdd"), "got: {index}");
        assert!(index.contains("TDD workflow"), "got: {index}");
        assert!(index.contains("</skill-index>"), "got: {index}");
    }

    #[tokio::test]
    async fn run_turn_returns_done_result_on_completion() {
        let ws = TempDir::new().unwrap();
        init_git_repo(ws.path());

        let provider = Arc::new(MockProvider {
            responses: Arc::new(vec![
                vec![StreamChunk::Text("I finished the task.".into()), StreamChunk::Done],
            ]),
            call_idx: Arc::new(AtomicUsize::new(0)),
        });

        let result = run_turn(
            TurnOpts {
                session_id: "sdone001".into(),
                task: Some("do something".into()),
                auto: true,
                workspace: ws.path().to_path_buf(),
                codebase_path: ws.path().to_path_buf(),
                allow: vec![],
                system_prompt: "".into(),
                model: "mock/test".into(),
                mcp_registry: vec![],
                skill_registry: vec![],
            },
            provider,
            vec![],
        ).await.unwrap();

        assert!(matches!(result, TurnResult::Done { .. }), "expected Done, got: {:?}", result);
        if let TurnResult::Done { summary, .. } = result {
            assert!(!summary.is_empty(), "summary should not be empty");
        }
    }

    #[tokio::test]
    async fn run_turn_returns_permissions_when_paused() {
        let ws = TempDir::new().unwrap();
        init_git_repo(ws.path());

        let provider = Arc::new(MockProvider {
            responses: Arc::new(vec![
                vec![
                    StreamChunk::ToolCallComplete {
                        tool_call_id: "tc1".into(),
                        tool_name: "shell".into(),
                        args: serde_json::json!({ "cmd": "ls", "reason": "listing" }),
                    },
                    StreamChunk::Done,
                ],
            ]),
            call_idx: Arc::new(AtomicUsize::new(0)),
        });

        let result = run_turn(
            TurnOpts {
                session_id: "sperm001".into(),
                task: Some("list files".into()),
                auto: false,
                workspace: ws.path().to_path_buf(),
                codebase_path: ws.path().to_path_buf(),
                allow: vec![],
                system_prompt: "".into(),
                model: "mock/test".into(),
                mcp_registry: vec![],
                skill_registry: vec![],
            },
            provider,
            vec![],
        ).await.unwrap();

        assert!(matches!(result, TurnResult::Permissions), "expected Permissions, got: {:?}", result);
    }

    #[tokio::test]
    async fn streaming_text_does_not_produce_extra_transcript_events() {
        let ws = TempDir::new().unwrap();
        init_git_repo(ws.path());

        let provider = Arc::new(MockProvider {
            responses: Arc::new(vec![
                vec![
                    StreamChunk::Text("hello ".into()),
                    StreamChunk::Text("world".into()),
                    StreamChunk::Done,
                ],
            ]),
            call_idx: Arc::new(AtomicUsize::new(0)),
        });

        run_turn(
            TurnOpts {
                session_id: "sdelta01".into(),
                task: Some("say hello".into()),
                auto: true,
                workspace: ws.path().to_path_buf(),
                codebase_path: ws.path().to_path_buf(),
                allow: vec![],
                system_prompt: "".into(),
                model: "mock/test".into(),
                mcp_registry: vec![],
                skill_registry: vec![],
            },
            provider,
            vec![],
        ).await.unwrap();

        let tx_path = ws.path().join("sessions/sdelta01/transcript.jsonl");
        let events = read_events(&tx_path).unwrap();
        let text_events: Vec<_> = events.iter().filter(|e| matches!(e, AnyEvent::AssistantText { .. })).collect();
        assert_eq!(text_events.len(), 1);
        if let AnyEvent::AssistantText { content, .. } = text_events[0] {
            assert_eq!(content, "hello world");
        }
    }

    // ── Test fixtures for Task 3 (resume path) ───────────────────────────────

    /// One step in a scripted provider response. A `text` step produces a
    /// text-only response (turn complete after this); a `tool_call` step
    /// produces a tool call followed by a final text step.
    #[derive(Debug, Clone)]
    enum FakeStep {
        Text(String),
        ToolCall { id: String, name: String, args: serde_json::Value },
    }

    impl FakeStep {
        fn text(s: &str) -> Self { FakeStep::Text(s.into()) }
    }

    /// Scripted provider that returns a deterministic sequence of steps.
    /// Each `text` step is one model call (text → Done).
    /// Each `tool_call` step is one model call (ToolCallComplete → Done).
    /// The test must provide enough steps to satisfy the model's stream loop.
    struct FakeProvider {
        steps: Arc<Vec<FakeStep>>,
        call_idx: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl Provider for FakeProvider {
        fn model_id(&self) -> &str { "fake" }
        async fn stream(
            &self, _system: &str, _messages: &[Message], _tools: &[Box<dyn Tool>],
        ) -> Result<std::pin::Pin<Box<dyn futures_util::Stream<Item = Result<StreamChunk, ProviderError>> + Send>>, ProviderError> {
            let idx = self.call_idx.fetch_add(1, Ordering::SeqCst);
            let step = self.steps.get(idx).cloned().unwrap_or(FakeStep::Text("".into()));
            let chunks: Vec<StreamChunk> = match step {
                FakeStep::Text(s) => vec![StreamChunk::Text(s), StreamChunk::Done],
                FakeStep::ToolCall { id, name, args } => vec![
                    StreamChunk::ToolCallComplete { tool_call_id: id, tool_name: name, args },
                    StreamChunk::Done,
                ],
            };
            Ok(Box::pin(stream! {
                for c in chunks { yield Ok(c); }
            }))
        }
    }

    #[tokio::test]
    async fn resume_executes_approved_tool_directly() {
        let ws = TempDir::new().unwrap();
        init_git_repo(ws.path());
        let codebase = TempDir::new().unwrap();
        let session_id = "resume-exec-test";
        // Initialize the session worktree (creates the session dir + branch)
        crate::git::create_session_worktree(ws.path(), session_id).unwrap();
        let session_dir = ws.path().join("sessions").join(session_id);
        let tx_path = session_dir.join("transcript.jsonl");

        // Pre-populate transcript: SessionStart + UserTurn + Permissions + PermissionsReply
        let perm_item = PermissionItem {
            id: "tc-resume-1".into(),
            cmd: "echo hello".into(),
            reason: "shell requires permission".into(),
            patterns: vec!["echo *".into()],
            tool_name: "shell".into(),
            tool_args: serde_json::json!({"cmd": "echo hello", "reason": "test"}),
        };
        append_event(&tx_path, &AnyEvent::SessionStart {
            schema_version: 1, id: session_id.into(),
            model: "test".into(), codebase_hash: "".into(), codebase_dirty: false, ts: 1,
        }).unwrap();
        append_event(&tx_path, &AnyEvent::UserTurn {
            content: "run echo".into(), from: None, context: None, ts: 2,
        }).unwrap();
        append_event(&tx_path, &AnyEvent::Permissions { items: vec![perm_item], ts: 3 }).unwrap();
        append_event(&tx_path, &AnyEvent::PermissionsReply {
            from: "user".into(),
            items: vec![PermissionsReplyItem {
                id: "tc-resume-1".into(), decision: "approve".into(),
                text: Some("shell:echo *".into()),
            }],
            ts: 4,
        }).unwrap();

        let shell_tool = ToolDef {
            name: "shell".into(),
            description: "run a shell command".into(),
            parameters: serde_json::json!({ "type": "object" }),
            execute: Box::new(|_args| Box::pin(async move {
                Ok(serde_json::Value::String("hello\n".into()))
            })),
        };
        let provider = Arc::new(FakeProvider {
            steps: Arc::new(vec![FakeStep::text("done")]),
            call_idx: Arc::new(AtomicUsize::new(0)),
        });
        let result = run_turn(
            TurnOpts {
                session_id: session_id.into(),
                task: None, // resume path: no new task
                auto: false,
                workspace: ws.path().to_path_buf(),
                codebase_path: codebase.path().to_path_buf(),
                allow: vec!["shell:echo *".into()],
                system_prompt: "".into(),
                model: "test".into(),
                mcp_registry: vec![],
                skill_registry: vec![],
            },
            provider,
            vec![Box::new(shell_tool)],
        ).await.unwrap();
        assert!(matches!(result, TurnResult::Done { .. }), "expected Done, got: {:?}", result);

        let events = read_events(&tx_path).unwrap();
        // ToolCall + ToolResult must appear (from direct execution, not re-request)
        assert!(events.iter().any(|e| matches!(e, AnyEvent::ToolCall { name, .. } if name == "shell")),
            "expected ToolCall from resume");
        assert!(events.iter().any(|e| matches!(e, AnyEvent::ToolResult { .. })),
            "expected ToolResult from resume");
        // No extra Permissions event (tool was executed, not gated again)
        let perm_count = events.iter().filter(|e| matches!(e, AnyEvent::Permissions { .. })).count();
        assert_eq!(perm_count, 1, "should have only the pre-existing Permissions event");
    }

    #[tokio::test]
    async fn resume_emits_denied_tool_result_for_rejected_call() {
        let ws = TempDir::new().unwrap();
        init_git_repo(ws.path());
        let codebase = TempDir::new().unwrap();
        let session_id = "resume-deny-test";
        // Initialize the session worktree (creates the session dir + branch)
        crate::git::create_session_worktree(ws.path(), session_id).unwrap();
        let session_dir = ws.path().join("sessions").join(session_id);
        let tx_path = session_dir.join("transcript.jsonl");

        let perm_item = PermissionItem {
            id: "tc-deny-1".into(), cmd: "rm -rf /".into(),
            reason: "shell requires permission".into(), patterns: vec!["rm *".into()],
            tool_name: "shell".into(),
            tool_args: serde_json::json!({"cmd": "rm -rf /", "reason": "cleanup"}),
        };
        append_event(&tx_path, &AnyEvent::SessionStart {
            schema_version: 1, id: session_id.into(),
            model: "test".into(), codebase_hash: "".into(), codebase_dirty: false, ts: 1,
        }).unwrap();
        append_event(&tx_path, &AnyEvent::UserTurn {
            content: "clean up".into(), from: None, context: None, ts: 2,
        }).unwrap();
        append_event(&tx_path, &AnyEvent::Permissions { items: vec![perm_item], ts: 3 }).unwrap();
        append_event(&tx_path, &AnyEvent::PermissionsReply {
            from: "user".into(),
            items: vec![PermissionsReplyItem {
                id: "tc-deny-1".into(), decision: "deny".into(), text: None,
            }],
            ts: 4,
        }).unwrap();

        let shell_tool = ToolDef {
            name: "shell".into(),
            description: "run a shell command".into(),
            parameters: serde_json::json!({ "type": "object" }),
            execute: Box::new(|_args| Box::pin(async move {
                Ok(serde_json::Value::String("should not run".into()))
            })),
        };
        let provider = Arc::new(FakeProvider {
            steps: Arc::new(vec![FakeStep::text("ok I won't do that")]),
            call_idx: Arc::new(AtomicUsize::new(0)),
        });
        let result = run_turn(
            TurnOpts {
                session_id: session_id.into(),
                task: None, // resume path: no new task
                auto: false,
                workspace: ws.path().to_path_buf(),
                codebase_path: codebase.path().to_path_buf(),
                allow: vec![],
                system_prompt: "".into(),
                model: "test".into(),
                mcp_registry: vec![],
                skill_registry: vec![],
            },
            provider,
            vec![Box::new(shell_tool)],
        ).await.unwrap();
        assert!(matches!(result, TurnResult::Done { .. }), "expected Done, got: {:?}", result);

        // Must have a ToolResult for the denied call (API invariant)
        let events_after = read_events(&tx_path).unwrap();
        let denied_result = events_after.iter().any(|e| matches!(e, AnyEvent::ToolResult { id, .. } if id == "tc-deny-1"));
        assert!(denied_result, "expected ToolResult for denied call");
        if let Some(AnyEvent::ToolResult { output, .. }) = events_after.iter().find(|e| matches!(e, AnyEvent::ToolResult { id, .. } if id == "tc-deny-1")) {
            assert!(output.contains("denied"), "got: {output}");
        }

        // Reconstruction must yield paired messages — not drop the orphan ToolResult.
        // Without a preceding ToolCall the ToolResult is silently skipped by context.rs,
        // so the model never sees the denial and retries the tool call in a loop.
        let messages = reconstruct_messages_from_events(&events_after).unwrap();
        // Expect: User (UserTurn), Assistant (ToolCall), Tool (ToolResult with denial)
        let has_tool_msg = messages.iter().any(|m| matches!(m, Message::Tool { .. }));
        assert!(has_tool_msg, "reconstruction must include Tool message with denied result; got {} messages", messages.len());
    }
}

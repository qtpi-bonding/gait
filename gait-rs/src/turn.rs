use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::collections::HashMap;
use futures_util::StreamExt;
use crate::provider::{Provider, StreamChunk};
use crate::types::{Message, MessageContent, AssistantContent, AssistantPart, ToolCall, ToolResult, ToolResultKind, Usage, ExecutionMode, ExecutionSignal, ToolPermission, Hook, HookAction};
use crate::types::Tool;
use crate::session::transcript::{
    AnyEvent, append_event, emit_event, read_events, init_session, count_turn_complete, now_ms,
    InitSessionOpts, PermissionItem, PermissionsReplyItem, TurnUsage, TurnOutput,
    ClarifyItem, InformItem,
};
use crate::session::context::reconstruct_messages_from_events;
use crate::config::{McpToolDef, SkillDef};
use crate::git;
use crate::mcp_client::McpClient;
use crate::permission::{is_request_approved, find_matched_allow_pattern, is_request_denied, find_matched_deny_pattern};
use grep_regex::RegexMatcher;
use grep_searcher::{Searcher, Sink, SinkMatch};

pub struct TurnOpts {
    pub session_id: String,
    pub task: Option<String>,
    pub auto: bool,
    pub workspace: PathBuf,
    pub codebase_path: PathBuf,
    pub allow: Vec<String>,
    pub deny: Vec<String>,
    pub system_prompt: String,
    pub model: String,  // stored in session_start event for wire-compat with TS gait
    pub mcp_registry: Vec<McpToolDef>,
    pub mcp_clients: HashMap<String, Arc<McpClient>>,
    pub skill_registry: Vec<SkillDef>,
    /// Names of loadables (MCP server names + skill names) seeded as enabled
    /// from `gait.jsonc` (those with `enabled=true`).
    pub config_enabled: std::collections::HashSet<String>,
    /// Maximum agentic iterations. None → 100 (default).
    pub max_iter: Option<usize>,
    /// Lifecycle hooks fired at key points in the turn loop.
    pub hooks: Vec<Box<dyn Hook>>,
}

#[derive(Debug, Clone)]
pub enum TurnResult {
    Done { message: String, result: Option<serde_json::Value> },
    Permissions,
    Clarify { items: Vec<ClarifyItem> },
    Inform  { items: Vec<InformItem> },
}

// ── Session lock (RAII PID file) ─────────────────────────────────────────────

pub struct SessionLock {
    path: PathBuf,
}

/// Shared path for the once-approval temp file, used by both turn.rs and chat.rs.
pub fn once_approval_path(workspace: &std::path::Path, session_id: &str) -> std::path::PathBuf {
    workspace.join("tmp").join(format!("gait-approve-{}", session_id))
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
/// Derive which loadable names are currently enabled.
/// Seeds from config defaults (config_enabled), then replays transcript
/// load/unload events from the `load` tool.
pub fn derive_enabled_loadables(
    events: &[AnyEvent],
    config_enabled: &std::collections::HashSet<String>,
) -> std::collections::HashSet<String> {
    let mut enabled = config_enabled.clone();
    for event in events {
        if let AnyEvent::ToolCall { name, input, .. } = event {
            if name == "load" {
                let item_name = input.get("name")
                    .and_then(|v| v.as_str()).unwrap_or("").to_string();
                let on = input.get("enabled")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                if on { enabled.insert(item_name); } else { enabled.remove(&item_name); }
            }
        }
    }
    enabled
}



/// Build the unified <loadable-index> system prompt section. Replaces the
/// separate <mcp-index> and <skill-index>. Always emits the block so the
/// system prompt instruction to "check <loadable-index>" is never a lie.
/// Each entry renders as: `  <kind> <name> enabled=<bool> — <description>`.
pub fn build_loadable_index(entries: &[crate::types::LoadableEntry]) -> String {
    if entries.is_empty() {
        return "\n<loadable-index>\n  (none enabled)\n</loadable-index>".to_string();
    }
    let lines: Vec<String> = entries.iter().map(|e| {
        format!("  {} {} enabled={} — {}",
            e.kind.as_str(), e.name, e.enabled, e.description)
    }).collect();
    format!("\n<loadable-index>\nCall load(name, enabled=true/false) to enable or disable.\n{}\n</loadable-index>",
        lines.join("\n"))
}

pub fn build_permission_status(allow: &[String], deny: &[String]) -> String {
    if allow.is_empty() && deny.is_empty() {
        return String::new();
    }
    let mut lines = vec![];
    if !allow.is_empty() {
        lines.push(format!("Auto-approved: {}", allow.join(", ")));
    }
    if !deny.is_empty() {
        lines.push(format!("Blocked: {}", deny.join(", ")));
    }
    lines.push("Anything else requires user approval.".to_string());
    format!("\n<permission-status>\n{}\n</permission-status>", lines.join("\n"))
}

fn make_mcp_tool_def(def: &McpToolDef, client: Arc<McpClient>) -> Box<dyn crate::types::Tool> {
    let name = def.name.clone();
    let description = def.description.clone();
    let schema = def.schema.clone();
    Box::new(crate::types::ToolDef {
        name: name.clone(),
        description,
        parameters: schema,
        execute: Box::new(move |args| {
            let client = client.clone();
            let name = name.clone();
            Box::pin(async move { client.call_tool(&name, args).await })
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
    approval_prefix: Option<String>,
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
    let raw = match tools.iter().find(|t| t.name() == tc.tool_name) {
        Some(t) => match t.execute(tc.args.clone()).await {
            Ok(v) => if v.is_string() { v.as_str().unwrap().to_owned() } else { v.to_string() },
            Err(e) => format!("error: {e}"),
        },
        None => format!("unknown tool: {}", tc.tool_name),
    };
    let output = match approval_prefix {
        Some(prefix) => format!("{}\n{}", prefix, raw),
        None => raw,
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

/// Run a pipeline of rg (grep) and sd-style (find/replace) operations on `raw` in-process.
/// `ops` is a JSON array of {"rg": "<pattern>"} and/or {"sd": ["<from>", "<to>"]} items.
/// No subprocess, no shell injection surface — pure in-process regex, linear time, no backtracking.
fn apply_parse_filter(raw: &str, ops: &serde_json::Value) -> String {
    let ops = match ops.as_array() {
        Some(a) => a,
        None => return raw.to_string(),
    };
    if ops.is_empty() {
        return raw.to_string();
    }

    let mut current = raw.to_string();

    for op in ops {
        if let Some(pattern) = op.get("rg").and_then(|v| v.as_str()) {
            struct LineCollector(Vec<String>);
            impl Sink for LineCollector {
                type Error = std::io::Error;
                fn matched(
                    &mut self,
                    _searcher: &Searcher,
                    mat: &SinkMatch<'_>,
                ) -> Result<bool, Self::Error> {
                    let line = String::from_utf8_lossy(mat.bytes()).into_owned();
                    self.0.push(line.trim_end_matches('\n').to_string());
                    Ok(true)
                }
            }
            current = match RegexMatcher::new(pattern) {
                Ok(matcher) => {
                    let mut collector = LineCollector(vec![]);
                    let _ = Searcher::new().search_slice(
                        &matcher,
                        current.as_bytes(),
                        &mut collector,
                    );
                    if collector.0.is_empty() {
                        "(no matches)".to_string()
                    } else {
                        collector.0.join("\n")
                    }
                }
                Err(e) => format!("[rg error: invalid pattern {:?}: {}]", pattern, e),
            };
        } else if let Some(sd_args) = op.get("sd").and_then(|v| v.as_array()) {
            // Use regex::bytes directly — the `sd` CLI crate is binary-only and has no lib API.
            let from = sd_args.first().and_then(|v| v.as_str()).unwrap_or("");
            let to   = sd_args.get(1).and_then(|v| v.as_str()).unwrap_or("");
            current = match regex::bytes::RegexBuilder::new(from)
                .multi_line(true)
                .build()
            {
                Ok(re) => {
                    let replaced = re.replace_all(current.as_bytes(), to.as_bytes());
                    String::from_utf8_lossy(&replaced).into_owned()
                }
                Err(e) => format!("[sd error: invalid pattern {:?}: {}]", from, e),
            };
        }
    }

    current
}

const CURATE_SYSTEM_PROMPT: &str = "You are a curator. You have been given content and a curation task. Call annotate() exactly once with the relevant and irrelevant items. Do not call any other tool. Do not respond with text.";

async fn apply_curate_filter(
    raw: &str,
    curate_prompt: &str,
    workspace: &std::path::Path,
    codebase_path: &std::path::Path,
    model: &str,
    provider: std::sync::Arc<dyn crate::provider::Provider>,
) -> serde_json::Value {
    use crate::tools::annotate::AnnotateTool;
    use crate::session::transcript::read_events;

    let session_id = crate::chat::new_session_id();
    let task = format!(
        "Content to curate:\n{}\n\n---\nCuration task: {}",
        raw, curate_prompt
    );

    let _ = run_turn(
        TurnOpts {
            session_id: session_id.clone(),
            task: Some(task),
            auto: true,
            workspace: workspace.to_path_buf(),
            codebase_path: codebase_path.to_path_buf(),
            allow: vec![],
            deny: vec![],
            system_prompt: CURATE_SYSTEM_PROMPT.to_string(),
            model: model.to_string(),
            mcp_registry: vec![],
            mcp_clients: std::collections::HashMap::new(),
            skill_registry: vec![],
            config_enabled: std::collections::HashSet::new(),
            max_iter: Some(1),
            hooks: vec![],
        },
        provider,
        vec![AnnotateTool::new().boxed()],
    ).await;

    let tx_path = workspace.join("sessions").join(&session_id).join("transcript.jsonl");
    read_events(&tx_path)
        .ok()
        .and_then(|events| {
            events.into_iter().rev().find_map(|e| {
                if let crate::session::transcript::AnyEvent::ToolCall { name, input, .. } = e {
                    if name == "annotate" { Some(input) } else { None }
                } else {
                    None
                }
            })
        })
        .unwrap_or_else(|| serde_json::json!({"relevant": [], "irrelevant": []}))
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
    let once_path = once_approval_path(&opts.workspace, &opts.session_id);
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
                    // Build scope-aware prefix: tell the model whether the approval
                    // was a one-shot or saved to the agent/global whitelist.
                    let prefix = {
                        let scope = reply_items.iter()
                            .find(|r| r.id == *id)
                            .and_then(|r| r.scope.as_deref());
                        match scope {
                            Some("once") => Some("[User approved once.]".to_string()),
                            Some("agent") | Some("global") => {
                                let pattern = items.iter()
                                    .flat_map(|p| p.patterns.iter())
                                    .next()
                                    .cloned()
                                    .unwrap_or_else(|| items[0].tool_name.clone());
                                Some(format!("[User added to whitelist: {}]", pattern))
                            }
                            _ => Some("[User approved.]".to_string()),
                        }
                    };
                    if let Err(e) = execute_tool_call(&tc, &tools, &tx_path, prefix).await {
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

    // Step 6: post-compaction reload loop. Server-granularity push:
    // enabled_names is a set of server names (and skill names); for each
    // enabled MCP server, push all of its tools into the active tools vec.
    let enabled_names = derive_enabled_loadables(&post_compaction, &opts.config_enabled);
    for server_name in &enabled_names {
        for def in opts.mcp_registry.iter().filter(|d| &d.server_name == server_name) {
            if let Some(client) = opts.mcp_clients.get(&def.server_name) {
                if !tools.iter().any(|t| t.name() == def.name) {  // duplicate guard
                    tools.push(make_mcp_tool_def(def, client.clone()));
                }
            }
        }
    }
    // Skills are not pushed here — they surface via LoadTool.execute() return value.

    // Warn about allow-list entries that don't match any builtin tool name.
    // Catches stale names (e.g. "mcp", "skill") after tool renames.
    {
        let builtin_names: std::collections::HashSet<&str> =
            crate::tools::builtin_tool_names().iter().copied().collect();
        for entry in &effective_allow {
            if !entry.contains(':') && !builtin_names.contains(entry.as_str()) {
                eprintln!("[gait] warning: allow entry {:?} doesn't match any builtin tool — will never auto-approve", entry);
            }
        }
    }

    // Step 5: Augment system prompt with unified <loadable-index>.
    // Build entries: one per whitelisted MCP server, one per whitelisted skill.
    use crate::types::{LoadableEntry, LoadableKind};
    use std::collections::HashMap;
    // Group MCP tool defs by server name so we can show one entry per server.
    let mut mcp_tools_by_server: HashMap<&str, Vec<&McpToolDef>> = HashMap::new();
    for def in &opts.mcp_registry {
        mcp_tools_by_server.entry(def.server_name.as_str()).or_default().push(def);
    }
    let mut entries: Vec<LoadableEntry> = Vec::new();
    // MCP server entries — one per server. Description comes from
    // McpServerDef.description (config layer); fall back to tool count.
    // Note: we don't have direct access to McpServerDef in run_turn, so
    // we build the description from the discovered tool count when
    // there's no explicit description available. Run-level only.
    for (server_name, defs) in &mcp_tools_by_server {
        let description = format!("{} tool{}", defs.len(), if defs.len() == 1 { "" } else { "s" });
        entries.push(LoadableEntry {
            name: server_name.to_string(),
            kind: LoadableKind::Mcp,
            description,
            enabled: enabled_names.contains(*server_name),
        });
    }
    // Skill entries — one per skill.
    for skill in &opts.skill_registry {
        entries.push(LoadableEntry {
            name: skill.name.clone(),
            kind: LoadableKind::Skill,
            description: skill.description.clone(),
            enabled: enabled_names.contains(&skill.name),
        });
    }
    let loadable_idx = build_loadable_index(&entries);
    let permission_status = build_permission_status(&effective_allow, &opts.deny);
    let system_prompt = opts.hooks.iter().fold(
        format!("{}{}{}", opts.system_prompt, loadable_idx, permission_status),
        |sp, h| h.pre_turn(&sp).unwrap_or(sp),
    );

    let mut total_usage = Usage::default();
    let mut clarify_signal: Option<Vec<ClarifyItem>> = None;
    let mut inform_signal: Option<Vec<InformItem>> = None;

    for _step in 0..opts.max_iter.unwrap_or(100) {
        let outgoing = opts.hooks.iter().fold(messages.clone(), |m, h| h.pre_step(&m).unwrap_or(m));
        let mut chunk_stream = provider.stream(&system_prompt, &outgoing, &tools).await
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
        for h in &opts.hooks { h.post_step(&step_text, &step_tool_calls); }

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

        // No tool calls → auto-wrap the text as an implicit output() call.
        // output() is always the turn-completion path; plain text is just a short-hand.
        if step_tool_calls.is_empty() {
            let message = step_text.clone();
            let events_now = read_events(&tx_path)?;
            let turn_n = count_turn_complete(&events_now) + 1;
            emit_event(&tx_path, &AnyEvent::TurnComplete {
                usage: TurnUsage {
                    input: total_usage.input_tokens as u64,
                    output: total_usage.output_tokens as u64,
                    cache_read: total_usage.cache_read_tokens as u64,
                    cache_write: total_usage.cache_write_tokens as u64,
                },
                codebase_hash: codebase_hash.clone(),
                codebase_dirty,
                output: TurnOutput { message: message.clone(), result: None },
                ts: now_ms(),
            })?;
            git::commit_session(&opts.workspace, &opts.session_id,
                &format!("session({}): turn {}", opts.session_id, turn_n))?;
            for h in &opts.hooks { h.post_turn(); }
            return Ok(TurnResult::Done { message, result: None });
        }

        // Single permission gate: classify all tool calls into allowed vs gated.
        // Allowed tools execute below (parallel batch + serial). Gated tools either
        // trigger a Permissions exit (non-auto) or never exist (auto mode, where
        // the gate is short-circuited and all tools are allowed).
        // B2: allowed tools execute and land in the transcript before we potentially
        // exit for permissions — their results survive any subsequent permission exit.
        let mut pending_permissions: Vec<PermissionItem> = vec![];
        let mut gated_ids: std::collections::HashSet<String> = Default::default();
        let mut denied_ids: std::collections::HashSet<String> = Default::default();
        let mut tool_approval_prefixes: HashMap<String, String> = HashMap::new();

        for tc in &step_tool_calls {
            let perm = tools.iter()
                .find(|t| t.name() == tc.tool_name)
                .map(|t| t.permission(&tc.args))
                .unwrap_or_else(|| ToolPermission::RequiresApproval(vec![
                    crate::types::PermissionRequest {
                        display: tc.tool_name.clone(),
                        patterns: vec![tc.tool_name.clone()],
                    }
                ]));

            // Tier 1: deny list — hard floor, checked before allow, applies even in auto mode
            let is_denied = match &perm {
                ToolPermission::Passthrough => false,
                ToolPermission::RequiresApproval(requests) => requests.iter()
                    .any(|req| is_request_denied(&tc.tool_name, req, &opts.deny)),
            };

            if is_denied {
                denied_ids.insert(tc.tool_call_id.clone());
                continue;
            }

            // Tier 2: check allow list
            let is_allowed = opts.auto || match &perm {
                ToolPermission::Passthrough => true,
                ToolPermission::RequiresApproval(requests) => requests.iter()
                    .all(|req| is_request_approved(&tc.tool_name, req, &effective_allow)),
            };

            if is_allowed {
                let prefix: Option<String> = if opts.auto {
                    Some("[Auto mode.]".to_string())
                } else {
                    match &perm {
                        ToolPermission::Passthrough => None,
                        ToolPermission::RequiresApproval(requests) => requests.iter()
                            .find_map(|req| find_matched_allow_pattern(&tc.tool_name, req, &effective_allow))
                            .map(|p| format!("[Auto-approved: {}]", p)),
                    }
                };
                if let Some(p) = prefix {
                    tool_approval_prefixes.insert(tc.tool_call_id.clone(), p);
                }
                continue;
            }

            // Tier 3: ask
            gated_ids.insert(tc.tool_call_id.clone());
            if let ToolPermission::RequiresApproval(requests) = &perm {
                for req in requests.iter()
                    .filter(|r| !is_request_approved(&tc.tool_name, r, &effective_allow))
                {
                    pending_permissions.push(PermissionItem {
                        id: tc.tool_call_id.clone(),
                        cmd: req.display.clone(),
                        reason: format!("{} requires permission", tc.tool_name),
                        patterns: req.patterns.clone(),
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

        // Emit blocked results for denied tools immediately (no prompt, model sees why)
        for tc in step_tool_calls.iter().filter(|tc| denied_ids.contains(&tc.tool_call_id)) {
            let perm = tools.iter()
                .find(|t| t.name() == tc.tool_name)
                .map(|t| t.permission(&tc.args))
                .unwrap_or_else(|| ToolPermission::RequiresApproval(vec![
                    crate::types::PermissionRequest {
                        display: tc.tool_name.clone(),
                        patterns: vec![tc.tool_name.clone()],
                    }
                ]));
            let pattern = if let ToolPermission::RequiresApproval(requests) = &perm {
                requests.iter()
                    .find_map(|req| find_matched_deny_pattern(&tc.tool_name, req, &opts.deny))
                    .unwrap_or_else(|| tc.tool_name.clone())
            } else {
                tc.tool_name.clone()
            };
            let output = format!("[Blocked by config: {}] permission denied", pattern);
            emit_event(&tx_path, &AnyEvent::ToolCall {
                id: tc.tool_call_id.clone(),
                name: tc.tool_name.clone(),
                input: tc.args.clone(),
                ts: now_ms(),
            })?;
            emit_event(&tx_path, &AnyEvent::ToolResult {
                id: tc.tool_call_id.clone(),
                output: output.clone(),
                ts: now_ms(),
            })?;
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
            let allowed: Vec<(usize, &ToolCall)> = step_tool_calls.iter().enumerate()
                .filter(|(_, tc)| !gated_ids.contains(&tc.tool_call_id) && !denied_ids.contains(&tc.tool_call_id))
                .collect();

            // Special-signal tools must be serial (they mutate turn state or the tools vec).
            // Everything else with ExecutionMode::Parallel runs as a concurrent batch.
            let is_parallel_eligible = |tc: &ToolCall| -> bool {
                tools.iter()
                    .find(|t| t.name() == tc.tool_name)
                    .map(|t| {
                        t.execution_signal() == ExecutionSignal::Continue
                            && t.execution_mode() == ExecutionMode::Parallel
                    })
                    .unwrap_or(false)
            };

            let (parallel_items, serial_items): (Vec<_>, Vec<_>) = allowed.iter()
                .partition(|(_, tc)| is_parallel_eligible(tc));

            // Parallel batch — pre_tool hook may block individual items before join_all
            let par_raw = futures_util::future::join_all(
                parallel_items.iter().map(|(_, tc)| {
                    let block_reason = opts.hooks.iter()
                        .find_map(|h| if let HookAction::Block { reason } = h.pre_tool(tc) { Some(reason) } else { None });
                    let prefix = tool_approval_prefixes.get(&tc.tool_call_id).cloned();
                    let tc = *tc;
                    let tools = &tools;
                    let tx_path = &tx_path;
                    async move {
                        if let Some(reason) = block_reason {
                            Err(anyhow::anyhow!("[Blocked by hook] {}", reason))
                        } else {
                            execute_tool_call(tc, tools, tx_path, prefix).await
                        }
                    }
                })
            ).await;

            // Serial items — run in original order, handle special cases inline
            let mut all_results: Vec<Option<(AssistantPart, ToolResult)>> =
                vec![None; step_tool_calls.len()];

            for ((i, tc), raw) in parallel_items.iter().zip(par_raw) {
                let (part, mut result) = raw.unwrap_or_else(|e| error_tool_result(tc, e));
                let result_str = result.result.as_str().unwrap_or("").to_string();
                for h in &opts.hooks {
                    if let Some(new) = h.post_tool(&tc.tool_name, &result_str) {
                        result.result = serde_json::Value::String(new);
                    }
                }
                // Apply output filter — in-memory only; transcript keeps raw result.
                let result_str = result.result.as_str().map(|s| s.to_string())
                    .unwrap_or_else(|| result.result.to_string());
                if let Some(parse_ops) = tc.args.get("parse") {
                    result.result = serde_json::Value::String(apply_parse_filter(&result_str, parse_ops));
                } else if let Some(curate_prompt) = tc.args.get("curate_prompt").and_then(|v| v.as_str()) {
                    let workspace = opts.workspace.clone();
                    let codebase_path = opts.codebase_path.clone();
                    let model = opts.model.clone();
                    let provider_clone = provider.clone();
                    result.result = Box::pin(async move {
                        apply_curate_filter(
                            &result_str, curate_prompt,
                            &workspace, &codebase_path, &model, provider_clone,
                        ).await
                    }).await;
                }
                all_results[*i] = Some((part, result));
            }

            for (i, tc) in &serial_items {
                // pre_tool hook — first block wins
                if let Some(reason) = opts.hooks.iter()
                    .find_map(|h| if let HookAction::Block { reason } = h.pre_tool(tc) { Some(reason) } else { None })
                {
                    let output = format!("[Blocked by hook] {}", reason);
                    emit_event(&tx_path, &AnyEvent::ToolCall {
                        id: tc.tool_call_id.clone(), name: tc.tool_name.clone(),
                        input: tc.args.clone(), ts: now_ms(),
                    })?;
                    emit_event(&tx_path, &AnyEvent::ToolResult {
                        id: tc.tool_call_id.clone(), output: output.clone(), ts: now_ms(),
                    })?;
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

                let signal = tools.iter()
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
                            if on {
                                // MCP: push all tools from this server (server-granularity)
                                for def in opts.mcp_registry.iter().filter(|d| d.server_name == item_name) {
                                    if let Some(client) = opts.mcp_clients.get(&def.server_name) {
                                        if !tools.iter().any(|t| t.name() == def.name) {  // duplicate guard
                                            tools.push(make_mcp_tool_def(def, client.clone()));
                                        }
                                    }
                                }
                                // Skill: no tools push needed — content is returned by LoadTool.execute()
                            } else {
                                // Remove all tools from this server
                                tools.retain(|t| {
                                    opts.mcp_registry.iter()
                                        .find(|d| d.name == t.name())
                                        .map(|d| d.server_name != item_name)
                                        .unwrap_or(true)
                                });
                            }
                        }
                        let prefix = tool_approval_prefixes.get(&tc.tool_call_id).cloned();
                        let (part, mut result) = execute_tool_call(tc, &tools, &tx_path, prefix).await
                            .unwrap_or_else(|e| error_tool_result(tc, e));
                        let result_str = result.result.as_str().unwrap_or("").to_string();
                        for h in &opts.hooks {
                            if let Some(new) = h.post_tool(&tc.tool_name, &result_str) {
                                result.result = serde_json::Value::String(new);
                            }
                        }
                        // Apply output filter — in-memory only; transcript keeps raw result.
                        let result_str = result.result.as_str().map(|s| s.to_string())
                            .unwrap_or_else(|| result.result.to_string());
                        if let Some(parse_ops) = tc.args.get("parse") {
                            result.result = serde_json::Value::String(apply_parse_filter(&result_str, parse_ops));
                        } else if let Some(curate_prompt) = tc.args.get("curate_prompt").and_then(|v| v.as_str()) {
                            let workspace = opts.workspace.clone();
                            let codebase_path = opts.codebase_path.clone();
                            let model = opts.model.clone();
                            let provider_clone = provider.clone();
                            result.result = Box::pin(async move {
                                apply_curate_filter(
                                    &result_str, curate_prompt,
                                    &workspace, &codebase_path, &model, provider_clone,
                                ).await
                            }).await;
                        }
                        all_results[*i] = Some((part, result));
                    }
                    ExecutionSignal::TurnComplete => {
                        // Emit call+result pair for transcript auditability before ending the turn.
                        emit_event(&tx_path, &AnyEvent::ToolCall {
                            id: tc.tool_call_id.clone(),
                            name: tc.tool_name.clone(),
                            input: tc.args.clone(),
                            ts: now_ms(),
                        })?;
                        emit_event(&tx_path, &AnyEvent::ToolResult {
                            id: tc.tool_call_id.clone(),
                            output: "recorded".to_string(),
                            ts: now_ms(),
                        })?;
                        let message = tc.args.get("message").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        let result = tc.args.get("result").cloned();
                        let events_now = read_events(&tx_path)?;
                        let turn_n = count_turn_complete(&events_now) + 1;
                        emit_event(&tx_path, &AnyEvent::TurnComplete {
                            usage: TurnUsage {
                                input: total_usage.input_tokens as u64,
                                output: total_usage.output_tokens as u64,
                                cache_read: total_usage.cache_read_tokens as u64,
                                cache_write: total_usage.cache_write_tokens as u64,
                            },
                            codebase_hash: codebase_hash.clone(),
                            codebase_dirty,
                            output: TurnOutput { message: message.clone(), result: result.clone() },
                            ts: now_ms(),
                        })?;
                        git::commit_session(&opts.workspace, &opts.session_id,
                            &format!("session({}): turn {}", opts.session_id, turn_n))?;
                        for h in &opts.hooks { h.post_turn(); }
                        return Ok(TurnResult::Done { message, result });
                    }
                    ExecutionSignal::Clarify => {
                        let items: Vec<ClarifyItem> = tc.args.get("items")
                            .and_then(|v| serde_json::from_value(v.clone()).ok())
                            .unwrap_or_default();
                        emit_event(&tx_path, &AnyEvent::Clarify { items: items.clone(), ts: now_ms() })?;
                        clarify_signal = Some(items);
                    }
                    ExecutionSignal::Inform => {
                        let items: Vec<InformItem> = tc.args.get("items")
                            .and_then(|v| serde_json::from_value(v.clone()).ok())
                            .unwrap_or_default();
                        emit_event(&tx_path, &AnyEvent::Inform { items: items.clone(), ts: now_ms() })?;
                        inform_signal = Some(items);
                    }
                    ExecutionSignal::Continue => {
                        // skill (default) and any other serial tool
                        let prefix = tool_approval_prefixes.get(&tc.tool_call_id).cloned();
                        let (part, mut result) = execute_tool_call(tc, &tools, &tx_path, prefix).await
                            .unwrap_or_else(|e| error_tool_result(tc, e));
                        let result_str = result.result.as_str().unwrap_or("").to_string();
                        for h in &opts.hooks {
                            if let Some(new) = h.post_tool(&tc.tool_name, &result_str) {
                                result.result = serde_json::Value::String(new);
                            }
                        }
                        // Apply output filter — in-memory only; transcript keeps raw result.
                        let result_str = result.result.as_str().map(|s| s.to_string())
                            .unwrap_or_else(|| result.result.to_string());
                        if let Some(parse_ops) = tc.args.get("parse") {
                            result.result = serde_json::Value::String(apply_parse_filter(&result_str, parse_ops));
                        } else if let Some(curate_prompt) = tc.args.get("curate_prompt").and_then(|v| v.as_str()) {
                            let workspace = opts.workspace.clone();
                            let codebase_path = opts.codebase_path.clone();
                            let model = opts.model.clone();
                            let provider_clone = provider.clone();
                            result.result = Box::pin(async move {
                                apply_curate_filter(
                                    &result_str, curate_prompt,
                                    &workspace, &codebase_path, &model, provider_clone,
                                ).await
                            }).await;
                        }
                        all_results[*i] = Some((part, result));
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
            for h in &opts.hooks { h.post_turn(); }
            return Ok(TurnResult::Permissions);
        }

        // Exit turn for clarify/inform — event already in transcript+stdout via emit_event
        if let Some(items) = clarify_signal.take() {
            git::commit_session(&opts.workspace, &opts.session_id,
                &format!("session({}): clarify", opts.session_id))?;
            for h in &opts.hooks { h.post_turn(); }
            return Ok(TurnResult::Clarify { items });
        }
        if let Some(items) = inform_signal.take() {
            git::commit_session(&opts.workspace, &opts.session_id,
                &format!("session({}): inform", opts.session_id))?;
            for h in &opts.hooks { h.post_turn(); }
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
    use crate::tools::OutputTool;
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
                deny: vec![],
                system_prompt: "you are a helpful assistant".into(),
                model: "mock/test".into(),
                mcp_registry: vec![],
                mcp_clients: std::collections::HashMap::new(),
                skill_registry: vec![],
                config_enabled: std::collections::HashSet::new(),
                max_iter: None,
                hooks: vec![],
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

        // output tool now exits immediately on ExecutionSignal::TurnComplete —
        // no second LLM call, no empty-text follow-up response.
        let provider = Arc::new(MockProvider {
            responses: Arc::new(vec![
                vec![
                    StreamChunk::ToolCallComplete {
                        tool_call_id: "tc1".into(),
                        tool_name: "output".into(),
                        args: serde_json::json!({ "message": "fixed the bug", "result": { "files": 2 } }),
                    },
                    StreamChunk::Done,
                ],
            ]),
            call_idx: Arc::new(AtomicUsize::new(0)),
        });

        let output_tool = OutputTool::new();

        run_turn(
            TurnOpts {
                session_id: "stest_out".into(),
                task: Some("fix a bug".into()),
                auto: true,
                workspace: ws.path().to_path_buf(),
                codebase_path: ws.path().to_path_buf(),
                allow: vec![],
                deny: vec![],
                system_prompt: "".into(),
                model: "mock/test".into(),
                mcp_registry: vec![],
                mcp_clients: std::collections::HashMap::new(),
                skill_registry: vec![],
                config_enabled: std::collections::HashSet::new(),
                max_iter: None,
                hooks: vec![],
            },
            provider,
            vec![output_tool.boxed()],
        ).await.unwrap();

        let tx_path = ws.path().join("sessions/stest_out/transcript.jsonl");
        let events = read_events(&tx_path).unwrap();
        let tc_event = events.iter().find(|e| matches!(e, AnyEvent::TurnComplete { .. })).unwrap();
        if let AnyEvent::TurnComplete { output, .. } = tc_event {
            assert_eq!(output.message, "fixed the bug");
            assert!(output.result.is_some());
        } else {
            panic!("expected TurnComplete");
        }
    }

    #[tokio::test]
    async fn output_tool_exempt_from_permission_gating() {
        let ws = TempDir::new().unwrap();
        init_git_repo(ws.path());

        // output tool now exits immediately on ExecutionSignal::TurnComplete.
        let provider = Arc::new(MockProvider {
            responses: Arc::new(vec![
                vec![
                    StreamChunk::ToolCallComplete {
                        tool_call_id: "tc1".into(),
                        tool_name: "output".into(),
                        args: serde_json::json!({ "message": "done" }),
                    },
                    StreamChunk::Done,
                ],
            ]),
            call_idx: Arc::new(AtomicUsize::new(0)),
        });

        let output_tool = OutputTool::new();

        run_turn(
            TurnOpts {
                session_id: "stest_out2".into(),
                task: Some("do it".into()),
                auto: false,
                workspace: ws.path().to_path_buf(),
                codebase_path: ws.path().to_path_buf(),
                allow: vec![],
                deny: vec![],
                system_prompt: "".into(),
                model: "mock/test".into(),
                mcp_registry: vec![],
                mcp_clients: std::collections::HashMap::new(),
                skill_registry: vec![],
                config_enabled: std::collections::HashSet::new(),
                max_iter: None,
                hooks: vec![],
            },
            provider,
            vec![output_tool.boxed()],
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
                allow: vec!["read".into()],
                deny: vec![],
                system_prompt: "".into(),
                model: "mock/test".into(),
                mcp_registry: vec![],
                mcp_clients: std::collections::HashMap::new(),
                skill_registry: vec![],
                config_enabled: std::collections::HashSet::new(),
                max_iter: None,
                hooks: vec![],
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

    // ── unified <loadable-index> + derive_enabled_loadables ─────────────────

    #[test]
    fn build_loadable_index_shows_all_kinds() {
        use crate::types::{LoadableEntry, LoadableKind};
        let entries = vec![
            LoadableEntry { name: "slack_send".into(), kind: LoadableKind::Mcp,
                description: "post to Slack".into(), enabled: false },
            LoadableEntry { name: "tdd".into(), kind: LoadableKind::Skill,
                description: "test-driven workflow".into(), enabled: true },
        ];
        let idx = build_loadable_index(&entries);
        assert!(idx.contains("<loadable-index>"), "got: {idx}");
        assert!(idx.contains("mcp"), "got: {idx}");
        assert!(idx.contains("skill"), "got: {idx}");
        assert!(idx.contains("slack_send"), "got: {idx}");
        assert!(idx.contains("tdd"), "got: {idx}");
        assert!(idx.contains("enabled=false"), "got: {idx}");
        assert!(idx.contains("enabled=true"), "got: {idx}");
    }

    #[test]
    fn build_loadable_index_empty_emits_none_block() {
        let result = build_loadable_index(&[]);
        assert!(result.contains("<loadable-index>"), "got: {result}");
        assert!(result.contains("(none enabled)"), "got: {result}");
    }

    #[test]
    fn derive_enabled_loadables_seeds_from_config_and_replays_transcript() {
        use crate::session::transcript::AnyEvent;
        use std::collections::HashSet;

        // config: tdd enabled, slack not
        let config_enabled: HashSet<String> = ["tdd".into()].into();

        // transcript: load(slack, enabled=true), load(tdd, enabled=false)
        let events = vec![
            AnyEvent::ToolCall { id: "1".into(), name: "load".into(),
                input: serde_json::json!({"name": "slack_send", "enabled": true}), ts: 1 },
            AnyEvent::ToolCall { id: "2".into(), name: "load".into(),
                input: serde_json::json!({"name": "tdd", "enabled": false}), ts: 2 },
        ];
        let enabled = derive_enabled_loadables(&events, &config_enabled);
        assert!(enabled.contains("slack_send"), "slack should be enabled after load");
        assert!(!enabled.contains("tdd"), "tdd should be disabled after unload");
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
                deny: vec![],
                system_prompt: "".into(),
                model: "mock/test".into(),
                mcp_registry: vec![],
                mcp_clients: std::collections::HashMap::new(),
                skill_registry: vec![],
                config_enabled: std::collections::HashSet::new(),
                max_iter: None,
                hooks: vec![],
            },
            provider,
            vec![],
        ).await.unwrap();

        assert!(matches!(result, TurnResult::Done { .. }), "expected Done, got: {:?}", result);
        if let TurnResult::Done { message, .. } = result {
            assert!(!message.is_empty(), "message should not be empty");
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
                deny: vec![],
                system_prompt: "".into(),
                model: "mock/test".into(),
                mcp_registry: vec![],
                mcp_clients: std::collections::HashMap::new(),
                skill_registry: vec![],
                config_enabled: std::collections::HashSet::new(),
                max_iter: None,
                hooks: vec![],
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
                deny: vec![],
                system_prompt: "".into(),
                model: "mock/test".into(),
                mcp_registry: vec![],
                mcp_clients: std::collections::HashMap::new(),
                skill_registry: vec![],
                config_enabled: std::collections::HashSet::new(),
                max_iter: None,
                hooks: vec![],
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

    // ── Three-tier permission model: deny tests ───────────────────────────────

    #[tokio::test]
    async fn denied_tool_gets_blocked_result_without_prompting() {
        let ws = TempDir::new().unwrap();
        init_git_repo(ws.path());

        // MockProvider: step 1 = shell rm call, step 2 = output with summary
        let provider = Arc::new(MockProvider {
            responses: Arc::new(vec![
                vec![
                    StreamChunk::ToolCallComplete {
                        tool_call_id: "tc1".into(),
                        tool_name: "shell".into(),
                        args: serde_json::json!({ "cmd": "rm -rf /", "reason": "cleanup" }),
                    },
                    StreamChunk::Done,
                ],
                vec![
                    StreamChunk::ToolCallComplete {
                        tool_call_id: "tc2".into(),
                        tool_name: "output".into(),
                        args: serde_json::json!({ "message": "blocked" }),
                    },
                    StreamChunk::Done,
                ],
            ]),
            call_idx: Arc::new(AtomicUsize::new(0)),
        });

        let shell_tool = crate::tools::ShellTool::new(
            ws.path().to_path_buf(),
            ws.path().to_path_buf(),
        );
        let output_tool = OutputTool::new();

        run_turn(
            TurnOpts {
                session_id: "sdeny001".into(),
                task: Some("try to destroy".into()),
                auto: false,
                workspace: ws.path().to_path_buf(),
                codebase_path: ws.path().to_path_buf(),
                allow: vec![],
                deny: vec!["shell:rm *".to_string()],
                system_prompt: "".into(),
                model: "mock/test".into(),
                mcp_registry: vec![],
                mcp_clients: std::collections::HashMap::new(),
                skill_registry: vec![],
                config_enabled: std::collections::HashSet::new(),
                max_iter: None,
                hooks: vec![],
            },
            provider,
            vec![shell_tool.boxed(), output_tool.boxed()],
        ).await.unwrap();

        let tx_path = ws.path().join("sessions/sdeny001/transcript.jsonl");
        let events = read_events(&tx_path).unwrap();

        // Must have a ToolResult indicating blocked by config
        let blocked_result = events.iter().any(|e| matches!(e, AnyEvent::ToolResult { id, output, .. }
            if id == "tc1" && output.contains("Blocked by config")));
        assert!(blocked_result, "expected blocked ToolResult for tc1; events: {:?}", events);

        // NO Permissions event should be emitted (deny is a hard floor, not a prompt)
        let has_permissions = events.iter().any(|e| matches!(e, AnyEvent::Permissions { .. }));
        assert!(!has_permissions, "deny should not trigger a Permissions event");
    }

    #[tokio::test]
    async fn deny_wins_when_tool_in_both_allow_and_deny() {
        let ws = TempDir::new().unwrap();
        init_git_repo(ws.path());

        let provider = Arc::new(MockProvider {
            responses: Arc::new(vec![
                vec![
                    StreamChunk::ToolCallComplete {
                        tool_call_id: "tc1".into(),
                        tool_name: "shell".into(),
                        args: serde_json::json!({ "cmd": "rm -rf /", "reason": "cleanup" }),
                    },
                    StreamChunk::Done,
                ],
                vec![
                    StreamChunk::ToolCallComplete {
                        tool_call_id: "tc2".into(),
                        tool_name: "output".into(),
                        args: serde_json::json!({ "message": "blocked" }),
                    },
                    StreamChunk::Done,
                ],
            ]),
            call_idx: Arc::new(AtomicUsize::new(0)),
        });

        let shell_tool = crate::tools::ShellTool::new(
            ws.path().to_path_buf(),
            ws.path().to_path_buf(),
        );
        let output_tool = OutputTool::new();

        run_turn(
            TurnOpts {
                session_id: "sdeny002".into(),
                task: Some("try to destroy".into()),
                auto: false,
                workspace: ws.path().to_path_buf(),
                codebase_path: ws.path().to_path_buf(),
                // Same pattern in BOTH allow and deny — deny must win.
                allow: vec!["shell:rm *".to_string()],
                deny: vec!["shell:rm *".to_string()],
                system_prompt: "".into(),
                model: "mock/test".into(),
                mcp_registry: vec![],
                mcp_clients: std::collections::HashMap::new(),
                skill_registry: vec![],
                config_enabled: std::collections::HashSet::new(),
                max_iter: None,
                hooks: vec![],
            },
            provider,
            vec![shell_tool.boxed(), output_tool.boxed()],
        ).await.unwrap();

        let tx_path = ws.path().join("sessions/sdeny002/transcript.jsonl");
        let events = read_events(&tx_path).unwrap();

        // Same assertions as the first deny test
        let blocked_result = events.iter().any(|e| matches!(e, AnyEvent::ToolResult { id, output, .. }
            if id == "tc1" && output.contains("Blocked by config")));
        assert!(blocked_result, "expected blocked ToolResult for tc1 even with allow match; events: {:?}", events);

        let has_permissions = events.iter().any(|e| matches!(e, AnyEvent::Permissions { .. }));
        assert!(!has_permissions, "deny-wins: should not trigger Permissions event");
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
                text: Some("shell:echo *".into()), scope: None,
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
                deny: vec![],
                system_prompt: "".into(),
                model: "test".into(),
                mcp_registry: vec![],
                mcp_clients: std::collections::HashMap::new(),
                skill_registry: vec![],
                config_enabled: std::collections::HashSet::new(),
                max_iter: None,
                hooks: vec![],
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
                id: "tc-deny-1".into(), decision: "deny".into(), text: None, scope: None,
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
                deny: vec![],
                system_prompt: "".into(),
                model: "test".into(),
                mcp_registry: vec![],
                mcp_clients: std::collections::HashMap::new(),
                skill_registry: vec![],
                config_enabled: std::collections::HashSet::new(),
                max_iter: None,
                hooks: vec![],
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
    // ── build_permission_status: three-tier model prompt injection ─────────────

    #[test]
    fn build_permission_status_shows_three_tiers() {
        let allow = vec!["read".to_string(), "shell:ls *".to_string()];
        let deny = vec!["shell:rm *".to_string()];
        let status = build_permission_status(&allow, &deny);
        assert!(status.contains("<permission-status>"), "got: {status}");
        assert!(status.contains("read"), "got: {status}");
        assert!(status.contains("shell:ls *"), "got: {status}");
        assert!(status.contains("shell:rm *"), "got: {status}");
    }

    #[test]
    fn build_permission_status_empty_returns_empty() {
        assert!(build_permission_status(&[], &[]).is_empty());
    }
    #[tokio::test]
    async fn resume_with_scope_agent_shows_whitelist_prefix() {
        // When a user approved with scope="agent", the resumed tool result should
        // show "[User added to whitelist: <pattern>]" instead of "[User approved.]"
        let ws = TempDir::new().unwrap();
        init_git_repo(ws.path());
        let session_id = "sscope001";
        // Initialize the session worktree (creates the session dir + branch)
        crate::git::create_session_worktree(ws.path(), session_id).unwrap();
        let tx_path = ws.path().join("sessions").join(session_id).join("transcript.jsonl");

        // Pre-write a transcript with a Permissions+PermissionsReply already in it,
        // simulating a previous turn that was interrupted mid-permission-prompt.
        // The PermissionsReply has scope="agent" to indicate it was saved to agent config.
        let shell_args = serde_json::json!({ "cmd": "ls", "reason": "list files" });
        append_event(&tx_path, &AnyEvent::SessionStart {
            schema_version: 2, id: session_id.into(),
            model: "mock/test".into(), codebase_hash: "none".into(), codebase_dirty: false, ts: 1,
        }).unwrap();
        append_event(&tx_path, &AnyEvent::UserTurn {
            content: "list files".into(), from: None, context: None, ts: 2,
        }).unwrap();
        append_event(&tx_path, &AnyEvent::Permissions {
            items: vec![PermissionItem {
                id: "tc-scope-1".into(),
                cmd: "ls".into(),
                reason: "shell requires permission".into(),
                patterns: vec!["shell:ls *".to_string()],
                tool_name: "shell".into(),
                tool_args: shell_args.clone(),
            }],
            ts: 3,
        }).unwrap();
        append_event(&tx_path, &AnyEvent::PermissionsReply {
            from: "user".into(),
            items: vec![PermissionsReplyItem {
                id: "tc-scope-1".into(),
                decision: "approve".into(),
                text: None,
                scope: Some("agent".into()),
            }],
            ts: 4,
        }).unwrap();

        let shell_tool = ToolDef {
            name: "shell".into(),
            description: "run a shell command".into(),
            parameters: serde_json::json!({ "type": "object" }),
            execute: Box::new(|_args| Box::pin(async move {
                Ok(serde_json::Value::String("file1\nfile2\n".into()))
            })),
        };

        // After resuming and executing shell, agent calls output to complete
        let provider = Arc::new(FakeProvider {
            steps: Arc::new(vec![FakeStep::text("done")]),
            call_idx: Arc::new(AtomicUsize::new(0)),
        });

        run_turn(
            TurnOpts {
                session_id: session_id.into(),
                task: None, // resume path: no new task
                auto: false,
                workspace: ws.path().to_path_buf(),
                codebase_path: ws.path().to_path_buf(),
                allow: vec![],
                deny: vec![],
                system_prompt: "".into(),
                model: "mock/test".into(),
                mcp_registry: vec![],
                mcp_clients: std::collections::HashMap::new(),
                skill_registry: vec![],
                config_enabled: std::collections::HashSet::new(),
                max_iter: None,
                hooks: vec![],
            },
            provider,
            vec![Box::new(shell_tool)],
        ).await.unwrap();

        let events = read_events(&tx_path).unwrap();
        // The shell tool result emitted during resume should contain the whitelist prefix
        let whitelist_result = events.iter().any(|e| {
            if let AnyEvent::ToolResult { id, output, .. } = e {
                id == "tc-scope-1" && output.contains("[User added to whitelist:")
            } else { false }
        });
        assert!(whitelist_result, "agent-scope approval should show whitelist prefix; events: {:?}", events);
    }

    // ── apply_parse_filter: in-process rg + sd-style regex pipeline ──────────

    #[test]
    fn apply_parse_filter_rg_keeps_matching_lines() {
        let raw = "line1\nFAILED: test_foo\nline3\n";
        let ops = serde_json::json!([{"rg": "FAILED"}]);
        let result = apply_parse_filter(raw, &ops);
        assert!(result.contains("FAILED: test_foo"), "got: {result}");
        assert!(!result.contains("line1"), "got: {result}");
        assert!(!result.contains("line3"), "got: {result}");
    }

    #[test]
    fn apply_parse_filter_sd_replaces_text() {
        let raw = "error: something went wrong\nerror: another thing\n";
        let ops = serde_json::json!([{"sd": ["error: ", "ERR: "]}]);
        let result = apply_parse_filter(raw, &ops);
        assert!(result.contains("ERR: something"), "got: {result}");
        assert!(!result.contains("error: "), "got: {result}");
    }

    #[test]
    fn apply_parse_filter_pipeline_rg_then_sd() {
        let raw = "ok\nFAILED: test_foo\nFAILED: test_bar\n";
        let ops = serde_json::json!([{"rg": "FAILED"}, {"sd": ["FAILED: ", ""]}]);
        let result = apply_parse_filter(raw, &ops);
        assert!(result.contains("test_foo"), "got: {result}");
        assert!(result.contains("test_bar"), "got: {result}");
        assert!(!result.contains("ok"), "got: {result}");
        assert!(!result.contains("FAILED: "), "got: {result}");
    }

    #[test]
    fn apply_parse_filter_rg_no_match_returns_sentinel() {
        let raw = "line1\nline2\n";
        let ops = serde_json::json!([{"rg": "NOTFOUND"}]);
        let result = apply_parse_filter(raw, &ops);
        assert!(result.trim() == "(no matches)" || result.trim().is_empty(), "got: {result}");
    }

    #[test]
    fn apply_parse_filter_empty_ops_returns_raw() {
        let raw = "hello world";
        let ops = serde_json::json!([]);
        let result = apply_parse_filter(raw, &ops);
        assert_eq!(result, raw);
    }
}

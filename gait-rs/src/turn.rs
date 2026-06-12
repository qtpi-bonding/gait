use std::path::{Path, PathBuf};
use std::sync::Arc;
use futures_util::StreamExt;
use crate::provider::StreamChunk;
use crate::ui::split_into_blocks;
use crate::types::{Message, MessageContent, AssistantContent, AssistantPart, ToolCall, ToolResult, ToolResultKind, Usage, ExecutionMode, ExecutionSignal, ToolPermission, HookAction};
use crate::types::Tool;
use crate::session::transcript::{
    init_session, count_turn_complete, now_ms,
    InitSessionOpts, PermissionItem, TurnUsage, TurnOutput,
    ClarifyItem, InformItem,
};
use crate::proto::{events, wire};
use crate::proto::Emitter;
use crate::session::Ingestor;
use crate::session::retrace;
use crate::config::McpToolDef;
use crate::git::checkpoint::CheckpointRepo;
use crate::mcp_client::McpClient;
use crate::permission::{is_request_approved, find_matched_allow_pattern, is_request_denied, find_matched_deny_pattern};
use grep_regex::RegexMatcher;
use grep_searcher::{Searcher, Sink, SinkMatch};
use crate::proto::gait::log::v1 as pb;
use crate::proto::gait::log::v1::{event, payload, Event};

/// The substantive payload kind for a proto event, if it carries one.
fn event_kind(e: &Event) -> Option<payload::Kind> {
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
    Done { message: String, result: Option<serde_json::Value> },
    Permissions,
    Clarify { items: Vec<ClarifyItem> },
    Inform  { items: Vec<InformItem> },
}

/// Per-call inputs for one turn (the things that vary every invocation).
pub struct TurnInput {
    pub task: Option<String>,   // None on resume
    pub auto: bool,
    pub max_iter: Option<usize>,
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

/// Snapshot the codebase into the shadow db for this turn. Returns the
/// checkpoint SHA, or None if the codebase is not a git repo or the checkpoint
/// failed — a checkpoint failure must never fail the turn. Does NOT commit the
/// transcript; the caller's existing commit_session runs after emit.
fn checkpoint_codebase(
    checkpoints: &dyn CheckpointRepo,
    gait_dir: &Path,
    session_id: &str,
    codebase: &Path,
    turn: u32,
) -> Option<String> {
    match checkpoints.checkpoint(gait_dir, session_id, codebase, turn) {
        Ok(sha) if !sha.is_empty() => Some(sha),
        Ok(_) => None, // non-git codebase
        Err(e) => {
            eprintln!("checkpoint failed for session {}: {}; checkpoint unavailable for this turn", session_id, e);
            None
        }
    }
}

fn build_turn_context(codebase_hash: &str, dirty_files: &[String], cwd: &std::path::Path) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    let mut ctx = format!("<status>\nCurrent time (unix): {}\nCodebase commit: {}\n", secs, codebase_hash);
    if !dirty_files.is_empty() {
        ctx.push_str("Dirty files:\n");
        for f in dirty_files { ctx.push_str(&format!("  {}\n", f)); }
    }
    // Terminal-style prompt: reminds the agent where `shell` runs (sticky cwd).
    ctx.push_str(&format!("{} %\n", cwd.display()));
    ctx.push_str("</status>\n");
    ctx
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
    let server = def.server_name.clone();
    let tool_name = def.name.clone();
    Box::new(crate::types::ToolDef {
        name: name.clone(),
        description,
        parameters: schema,
        execute: Box::new(move |args| {
            let client = client.clone();
            let name = name.clone();
            Box::pin(async move { client.call_tool(&name, args).await })
        }),
        permission: Some(Box::new(move |_args| {
            crate::permission::mcp_permission(&server, &tool_name)
        })),
    })
}

/// Execute one tool call: emit ToolCall event, run the tool, emit ToolResult event.
/// Used identically by the normal execution loop and the permission-resume path.
/// Does NOT handle output/mcp/skill special tools — those are never gated.
async fn execute_tool_call(
    tc: &ToolCall,
    tools: &[Box<dyn Tool>],
    emitter: &crate::proto::Emitter,
    approval_prefix: Option<String>,
) -> anyhow::Result<(AssistantPart, ToolResult)> {
    let id = tc.tool_call_id.clone();
    emitter.emit(events::tool_call(id.clone(), tc.tool_name.clone(), &tc.args))?;
    let assistant_part = AssistantPart::ToolCall(ToolCall {
        tool_call_id: id.clone(),
        tool_name: tc.tool_name.clone(),
        args: tc.args.clone(),
    });
    // Generic timing for every tool. Shell no longer bakes "exit N (Xms)" into
    // its output string — duration lives in the proto `ToolResult.duration` field
    // and process exit/timed_out live in `ToolResult.process`. We measure the full
    // tool.execute() call so the timing matches what the model observed.
    let (raw, duration, process) = match tools.iter().find(|t| t.name() == tc.tool_name) {
        Some(t) => {
            let started = std::time::Instant::now();
            let res = t.execute(tc.args.clone()).await;
            let elapsed = started.elapsed();
            let duration = Some(pbjson_types::Duration {
                seconds: elapsed.as_secs() as i64,
                nanos: elapsed.subsec_nanos() as i32,
            });
            match res {
                Ok(v) => {
                    // Process-tool shape (set by shell today): unwrap to model-facing
                    // text and lift process metadata. The model never sees the JSON
                    // wrapper — turn.rs is the single place that bridges Value → ToolResult.
                    if let (Some(output), Some(exit_code), Some(timed_out)) = (
                        v.get("output"),
                        v.get("exit_code"),
                        v.get("timed_out"),
                    ) {
                        let output = output.as_str().unwrap_or("").to_string();
                        let exit_code = exit_code.as_i64().unwrap_or(0) as i32;
                        let timed_out = timed_out.as_bool().unwrap_or(false);
                        (
                            output,
                            duration,
                            Some(pb::ProcessResult { exit_code, timed_out }),
                        )
                    } else if v.is_string() {
                        (v.as_str().unwrap().to_owned(), duration, None)
                    } else {
                        (v.to_string(), duration, None)
                    }
                }
                Err(e) => (format!("error: {e}"), duration, None),
            }
        }
        None => (format!("unknown tool: {}", tc.tool_name), None, None),
    };
    let output = match approval_prefix {
        Some(prefix) => format!("{}\n{}", prefix, raw),
        None => raw,
    };
    emitter.emit(events::tool_result(id.clone(), output.clone(), duration, process))?;
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

    let in_lines = raw.lines().count();
    let out_lines = current.lines().count();
    if out_lines != in_lines {
        eprintln!("[gait] parse-filter: {}/{} lines kept", out_lines, in_lines);
    }
    current
}

const CURATE_SYSTEM_PROMPT: &str = "You are a curator. You have been given content and a curation task. Call annotate() exactly once with the relevant and irrelevant items. Do not call any other tool. Do not respond with text.";

async fn apply_curate_filter(
    raw: &str,
    curate_prompt: &str,
    gait_dir: &std::path::Path,
    codebase_path: &std::path::Path,
    model: &str,
    provider: std::sync::Arc<dyn crate::provider::Provider>,
) -> serde_json::Value {
    use crate::tools::annotate::AnnotateTool;
    use crate::session::transcript::read_events;

    let repos = crate::git::Repos::production();
    let session_id = crate::chat::new_session_id();
    let task = format!(
        "Content to curate:\n{}\n\n---\nCuration task: {}",
        raw, curate_prompt
    );

    let session = crate::session::Session {
        id: session_id.clone(),
        gait_dir: gait_dir.to_path_buf(),
        codebase_path: codebase_path.to_path_buf(),
        assigned_by: None,
        repos: repos.clone(),
    };
    let caps = crate::profile::ResolvedProfile {
        provider,
        tools: vec![AnnotateTool::new().boxed()],
        model: model.to_string(),
        instructions: CURATE_SYSTEM_PROMPT.to_string(),
        allow: vec![],
        deny: vec![],
        mcp_registry: vec![],
        mcp_clients: std::collections::HashMap::new(),
        skill_registry: vec![],
        config_enabled: std::collections::HashSet::new(),
            output_schema_retries: 3,
            result_schema: None,
            hooks: vec![],
    };
    // curate is a side turn — transcript only, no live stream consumer.
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<crate::proto::gait::log::v1::StreamItem>();
    let curate_emitter = Emitter::for_session(&session.gait_dir, &session.id, tx);
    let _ = run_turn(&session, &curate_emitter, caps,
        TurnInput { task: Some(task), auto: true, max_iter: Some(1) }).await;

    let _ = repos.checkpoints.remove(gait_dir, &session_id);

    let tx_path = gait_dir.join("sessions").join(&session_id).join("log.jsonl");
    read_events(&tx_path)
        .ok()
        .and_then(|events| {
            events.into_iter().rev().find_map(|e| {
                if let Some(crate::proto::gait::log::v1::payload::Kind::ToolCall(tc)) = event_kind(&e) {
                    if tc.name == "annotate" {
                        Some(tc.input.as_ref()
                            .map(|s| serde_json::to_value(s).unwrap_or(serde_json::Value::Null))
                            .unwrap_or(serde_json::Value::Null))
                    } else { None }
                } else {
                    None
                }
            })
        })
        .map(|v| {
            let relevant_items: Vec<&str> = v.get("relevant")
                .and_then(|r| r.as_array())
                .map(|a| a.iter().filter_map(|s| s.as_str()).collect())
                .unwrap_or_default();
            let irrelevant_count = v.get("irrelevant")
                .and_then(|r| r.as_array())
                .map(|a| a.len())
                .unwrap_or(0);
            eprintln!("[gait] curate-filter: {}/{} items kept", relevant_items.len(), relevant_items.len() + irrelevant_count);
            let body = relevant_items.join("\n");
            if irrelevant_count > 0 {
                serde_json::Value::String(format!(
                    "[curate-filter: {} kept, {} dropped]\n{}",
                    relevant_items.len(), irrelevant_count, body
                ))
            } else {
                serde_json::Value::String(body)
            }
        })
        .unwrap_or_else(|| serde_json::Value::String(raw.to_string()))
}

fn validate_against_schema(schema: &serde_json::Value, value: &serde_json::Value) -> Result<(), String> {
    let compiled = jsonschema::JSONSchema::compile(schema)
        .map_err(|e| format!("invalid schema: {e}"))?;
    let result = compiled.validate(value);
    if let Err(errors) = result {
        let msgs: Vec<String> = errors.map(|e| format!("  - {e}")).collect();
        return Err(msgs.join("\n"));
    }
    Ok(())
}


pub async fn run_turn(
    session: &crate::session::Session,
    emitter: &Emitter,
    caps: crate::profile::ResolvedProfile,
    input: TurnInput,
) -> anyhow::Result<TurnResult> {
    // Build the live Agent config from the whole profile BEFORE `caps` is
    // destructured, so `events::agent_msg` stays the single source of truth.
    // Emitted later, under the drift guard.
    let agent = crate::proto::events::agent_msg(&caps);
    let provider = caps.provider.clone();
    let tools = caps.tools;
    let session_id = session.id.clone();
    let gait_dir = session.gait_dir.clone();
    let codebase_path = session.codebase_path.clone();
    let model = caps.model.clone();
    let repos = session.repos.clone();
    let allow = caps.allow;
    let deny = caps.deny;
    let system_prompt = caps.instructions;
    let mcp_registry = caps.mcp_registry;
    let mcp_clients = caps.mcp_clients;
    let skill_registry = caps.skill_registry;
    let config_enabled = caps.config_enabled;
    let hooks = caps.hooks;
    let result_schema = caps.result_schema;
    let mut schema_retries_remaining: u8 = caps.output_schema_retries;

    // Build the agent-dimension seed (M3): config-derived genesis for ActiveAgent.
    // Approach (a): build directly from the resolved fields already in scope.
    // config_enabled = enabled mcp_servers ∪ enabled skills (profile.rs:75-78);
    // fold it into both sets to reproduce the old ActiveLoadables flat seed exactly.
    let agent_seed = {
        let mut s = retrace::AgentState {
            model: model.clone(),
            instructions: system_prompt.clone(),
            tools: Default::default(),
            skills: skill_registry.iter().map(|sk| sk.name.clone()).collect(),
            mcp: Default::default(),
            allow: allow.iter().cloned().collect(),
            deny: deny.iter().cloned().collect(),
        };
        for n in &config_enabled { s.skills.insert(n.clone()); s.mcp.insert(n.clone()); }
        s
    };
    let task = input.task;
    let auto = input.auto;
    let max_iter = input.max_iter;

    repos.traces.create_session_worktree(&gait_dir, &session_id)?;
    repos.checkpoints.init(&gait_dir, &session_id, &codebase_path)?;

    // Persist codebase_path (write-once) so `gait rewind` can locate the codebase.
    {
        use crate::session::session_json::{read_session_json, write_session_json};
        let mut sj = read_session_json(&gait_dir, &session_id);
        if sj.codebase_path.is_none() {
            sj.id = session_id.clone();
            if sj.created_at == 0 { sj.created_at = now_ms(); }
            sj.codebase_path = Some(codebase_path.to_string_lossy().to_string());
            write_session_json(&gait_dir, &sj)?;
        }
    }

    let session_dir = gait_dir.join("sessions").join(&session_id);
    let tx_path = session_dir.join("log.jsonl");
    let ingestor = Ingestor::from_session(&gait_dir, &session_id);

    let _lock = SessionLock::acquire(&session_dir)?;

    let codebase_hash = repos.codebase.head_hash(&codebase_path);
    let dirty_files = repos.codebase.dirty_files(&codebase_path);
    let codebase_dirty = !dirty_files.is_empty();

    // Init transcript on first turn
    let existing = std::fs::read_to_string(&tx_path).unwrap_or_default();
    if existing.trim().is_empty() {
        init_session(InitSessionOpts {
            transcript_path: tx_path.clone(),
            id: session_id.clone(),
            model: model.clone(),
            codebase_hash: codebase_hash.clone(),
            codebase_dirty,
        })?;
    }

    // Effective allow = config baseline (global+agent, in `allow`) + log-derived
    // session/once grants, folded mutation-aware from the full session via the
    // ActivePermissions projection. Replaces the once-approval temp file entirely:
    // once/session approvals now live in the Log as PermissionsReply events.
    let effective_allow: Vec<String> = {
        let evs = ingestor.events()?;
        let mut muts = retrace::active_mutations(&evs);
        retrace::apply_cascade(&mut muts, &evs);
        let seed: std::collections::HashSet<String> = allow.iter().cloned().collect();
        let st = retrace::run_projection(&retrace::ActivePermissions { seed }, &evs, &muts);
        st.session.union(&st.once).cloned().collect()
    };

    // Detect resume after permission approval: last two transcript events are
    // Permissions → PermissionsReply. On resume we directly execute the
    // approved tools (or emit denied ToolCall/ToolResult for rejected ones),
    // re-read the transcript, and reconstruct messages — all without an
    // extra LLM round-trip.
    let pre_events = ingestor.events()?;
    let mut messages: Vec<Message> = Vec::new();
    let mut tools = tools; // take ownership so we can mutate (mcp load/unload)
    let did_resume = if matches!(event_kind(pre_events.last().unwrap_or(&Event::default())), Some(payload::Kind::PermissionsReply(_))) {
        // Find the Permissions event immediately preceding the reply
        let perm_event = pre_events.iter().rev()
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
            let decisions: std::collections::HashMap<&str, Decision> = reply_items.iter()
                .map(|r| (r.id.as_str(), Decision::try_from(r.decision).unwrap_or(Decision::Unspecified)))
                .collect();

            // Deduplicate by id: collect unique tool_call_ids and their items
            let mut by_id: std::collections::BTreeMap<String, Vec<&crate::proto::gait::log::v1::PermissionItem>> =
                std::collections::BTreeMap::new();
            for item in &perm_items {
                by_id.entry(item.id.clone()).or_default().push(item);
            }

            // Execute or deny each unique tool call
            for (id, items) in &by_id {
                // All-or-nothing: all segments of this tool_call_id must be approved
                let all_approved = items.iter().all(|item| {
                    decisions.get(item.id.as_str()).copied() == Some(Decision::Approve)
                });

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
                        let scope = reply_items.iter()
                            .find(|r| r.id == *id)
                            .map(|r| Scope::try_from(r.scope).unwrap_or(Scope::Unspecified));
                        match scope {
                            Some(Scope::Once) => Some("[User approved once.]".to_string()),
                            Some(Scope::Agent) | Some(Scope::Global) => {
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
                    if let Err(e) = execute_tool_call(&tc, &tools, emitter, prefix).await {
                        // Emit error as tool result so model sees it
                        emitter.emit(events::tool_result(
                            id.clone(),
                            format!("error executing approved tool: {e}"),
                            None, None,
                        ))?;
                    }
                } else {
                    // At least one segment denied — emit ToolCall FIRST, then ToolResult.
                    // Without a preceding ToolCall, context.rs silently drops the orphan
                    // ToolResult during reconstruction, and the model retries forever (gate loop).
                    emitter.emit(events::tool_call(
                        id.clone(),
                        items[0].tool_name.clone(),
                        &input_value(&items[0].tool_args),
                    ))?;
                    emitter.emit(events::tool_result(
                        id.clone(),
                        "permission denied by user".into(),
                        None, None,
                    ))?;
                }
            }

            // Re-read events after execution and reconstruct messages.
            // This is the ONLY source of truth — do not manually push to messages.
            let events_after = ingestor.events()?;
            messages = retrace::messages(&events_after)?;
            // Do NOT inject UserTurn or turn_ctx — model already has context from the prior turn.
            true
        } else {
            false
        }
    } else {
        false
    };

    // Emit the live Agent config iff it differs from the last one in the log
    // (drift guard). First run of a session has no prior Agent → emit.
    // Identical resumes skip the write; a real change (e.g. model swap, new
    // MCP load) is recorded. Placed AFTER init_session + resume setup, BEFORE
    // the first UserTurn is appended, so the Agent opens (or refreshes)
    // the working state projection for the turn.
    {
        let events_for_guard = ingestor.events()?;
        let mut guard_muts = retrace::active_mutations(&events_for_guard);
        retrace::apply_cascade(&mut guard_muts, &events_for_guard);
        let guard_state = retrace::run_projection(
            &retrace::ActiveAgent { seed: agent_seed.clone() },
            &events_for_guard,
            &guard_muts,
        );
        let rebaselined = retrace::rebaseline_agent(agent.clone(), &guard_state);
        if retrace::active_agent_checkpoint(&events_for_guard) != Some(rebaselined.clone()) {
            emitter.emit(wire::body(payload::Kind::Agent(rebaselined)))?;
        }
    }

    if !did_resume {
        if let Some(task) = &task {
            let ctx = crate::session::transcript::UserTurnContext {
                codebase_hash: Some(codebase_hash.clone()),
                codebase_dirty: Some(codebase_dirty),
                dirty_files: if dirty_files.is_empty() { None } else { Some(dirty_files.clone()) },
                injected_files: None,
            };
            emitter.emit(events::user_turn(task.clone(), None, Some(&ctx)))?;
        }
        let events = ingestor.events()?;
        messages = retrace::messages(&events)?;
        let cwd = retrace::active_cwd(&events, &codebase_path);
        let turn_ctx = build_turn_context(&codebase_hash, &dirty_files, &cwd);
        messages.insert(0, Message::User { content: MessageContent::Text(turn_ctx) });
    }

    // Derive enabled loadables from the FULL session (mutation-aware, via the
    // ActiveAgent projection). Loadables are a "state" projection and persist
    // across compaction — only the message context (ActiveMessages) resets on
    // compaction; consistent with permissions and agent config.
    let all_events = ingestor.events()?;
    let mut muts = retrace::active_mutations(&all_events);
    retrace::apply_cascade(&mut muts, &all_events);
    let agent_state = retrace::run_projection(
        &retrace::ActiveAgent { seed: agent_seed.clone() },
        &all_events,
        &muts,
    );
    let enabled_names = agent_state.enabled_loadables();
    for server_name in &enabled_names {
        for def in mcp_registry.iter().filter(|d| &d.server_name == server_name) {
            if let Some(client) = mcp_clients.get(&def.server_name) {
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
    for def in &mcp_registry {
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
    for skill in &skill_registry {
        entries.push(LoadableEntry {
            name: skill.name.clone(),
            kind: LoadableKind::Skill,
            description: skill.description.clone(),
            enabled: enabled_names.contains(&skill.name),
        });
    }
    let loadable_idx = build_loadable_index(&entries);
    let permission_status = build_permission_status(&effective_allow, &deny);
    let system_prompt = hooks.iter().fold(
        format!("{}{}{}", system_prompt, loadable_idx, permission_status),
        |sp, h| h.pre_turn(&sp).unwrap_or(sp),
    );

    let mut total_usage = Usage::default();
    let mut clarify_signal: Option<Vec<ClarifyItem>> = None;
    let mut inform_signal: Option<Vec<InformItem>> = None;
    let mut all_turn_text = String::new();

    for _step in 0..max_iter.unwrap_or(100) {
        let outgoing = hooks.iter().fold(messages.clone(), |m, h| h.pre_step(&m).unwrap_or(m));
        let mut chunk_stream = provider.stream(&system_prompt, &outgoing, &tools).await
            .map_err(|e| anyhow::anyhow!("provider error: {}", e))?;

        let mut step_text = String::new();
        let mut step_tool_calls: Vec<ToolCall> = vec![];
        let mut post_tool_text: Vec<String> = vec![];
        let mut seen_tool_complete = false;

        while let Some(chunk) = chunk_stream.next().await {
            match chunk.map_err(|e| anyhow::anyhow!("stream error: {}", e))? {
                StreamChunk::Text(t) => {
                    if seen_tool_complete {
                        post_tool_text.push(t.clone());
                    } else {
                        emitter.delta(t.clone());
                    }
                    step_text.push_str(&t);
                }
                StreamChunk::ToolCallComplete { tool_call_id, tool_name, args } => {
                    seen_tool_complete = true;
                    step_tool_calls.push(ToolCall { tool_call_id, tool_name, args });
                }
                StreamChunk::StepUsage(u) => {
                    total_usage = total_usage.add(&u);
                }
                StreamChunk::Done => break,
                _ => {}
            }
        }
        all_turn_text.push_str(&step_text);
        for h in &hooks { h.post_step(&step_text, &step_tool_calls); }

        // Flush assistant text — only as a standalone event when there are NO tool calls.
        // When tool calls are present, the text is included as AssistantPart::Text in the
        // Parts message below. Writing a separate AssistantText event here would produce
        // two consecutive assistant messages in context reconstruction, which Anthropic rejects.
        if !step_text.is_empty() && step_tool_calls.is_empty() {
            emitter.emit(events::assistant_text(step_text.clone()))?;
        }

        // No tool calls → auto-wrap the text as an implicit output() call.
        // output() is always the turn-completion path; plain text is just a short-hand.
        if step_tool_calls.is_empty() {
            let message = step_text.clone();
            let events_now = ingestor.events()?;
            let turn_n = count_turn_complete(&events_now) as u32 + 1;
            let code_checkpoint = checkpoint_codebase(
                repos.checkpoints.as_ref(),
                &gait_dir, &session_id, &codebase_path, turn_n,
            );
            let usage = TurnUsage {
                input: total_usage.input_tokens as u64,
                output: total_usage.output_tokens as u64,
                cache_read: total_usage.cache_read_tokens as u64,
                cache_write: total_usage.cache_write_tokens as u64,
            };
            let output = TurnOutput { message: message.clone(), result: None };
            emitter.emit(events::turn_complete(
                &usage, codebase_hash.clone(), codebase_dirty, code_checkpoint,
                split_into_blocks(&all_turn_text), &output,
            ))?;
            repos.traces.commit_session(&gait_dir, &session_id,
                &format!("session({}): turn {}", session_id, turn_n))?;
            for h in &hooks { h.post_turn(); }
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
                    .any(|req| is_request_denied(&tc.tool_name, req, &deny)),
            };

            if is_denied {
                denied_ids.insert(tc.tool_call_id.clone());
                continue;
            }

            // Tier 2: check allow list
            let is_allowed = auto || match &perm {
                ToolPermission::Passthrough => true,
                ToolPermission::RequiresApproval(requests) => requests.iter()
                    .all(|req| is_request_approved(&tc.tool_name, req, &effective_allow)),
            };

            if is_allowed {
                let prefix: Option<String> = if auto {
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
                    .find_map(|req| find_matched_deny_pattern(&tc.tool_name, req, &deny))
                    .unwrap_or_else(|| tc.tool_name.clone())
            } else {
                tc.tool_name.clone()
            };
            let output = format!("[Blocked by config: {}] permission denied", pattern);
            emitter.emit(events::tool_call(
                tc.tool_call_id.clone(), tc.tool_name.clone(), &tc.args,
            ))?;
            emitter.emit(events::tool_result(
                tc.tool_call_id.clone(), output.clone(),
                None, None,
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
                    let block_reason = hooks.iter()
                        .find_map(|h| if let HookAction::Block { reason } = h.pre_tool(tc) { Some(reason) } else { None });
                    let prefix = tool_approval_prefixes.get(&tc.tool_call_id).cloned();
                    let tc = *tc;
                    let tools = &tools;
                    async move {
                        if let Some(reason) = block_reason {
                            Err(anyhow::anyhow!("[Blocked by hook] {}", reason))
                        } else {
                            execute_tool_call(tc, tools, emitter, prefix).await
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
                for h in &hooks {
                    if let Some(new) = h.post_tool(&tc.tool_name, &tc.tool_call_id, &tc.args, &result_str).await {
                        result.result = serde_json::Value::String(new);
                    }
                }
                // Apply output filter — in-memory only; transcript keeps raw result.
                let result_str = result.result.as_str().map(|s| s.to_string())
                    .unwrap_or_else(|| result.result.to_string());
                if let Some(parse_ops) = tc.args.get("parse") {
                    result.result = serde_json::Value::String(apply_parse_filter(&result_str, parse_ops));
                } else if let Some(curate_prompt) = tc.args.get("curate_prompt").and_then(|v| v.as_str()) {
                    let gait_dir = gait_dir.clone();
                    let codebase_path = codebase_path.clone();
                    let model = model.clone();
                    let provider_clone = provider.clone();
                    result.result = Box::pin(async move {
                        apply_curate_filter(
                            &result_str, curate_prompt,
                            &gait_dir, &codebase_path, &model, provider_clone,
                        ).await
                    }).await;
                }
                all_results[*i] = Some((part, result));
            }

            for (i, tc) in &serial_items {
                // pre_tool hook — first block wins
                if let Some(reason) = hooks.iter()
                    .find_map(|h| if let HookAction::Block { reason } = h.pre_tool(tc) { Some(reason) } else { None })
                {
                    let output = format!("[Blocked by hook] {}", reason);
                    emitter.emit(events::tool_call(
                        tc.tool_call_id.clone(), tc.tool_name.clone(), &tc.args,
                    ))?;
                    emitter.emit(events::tool_result(
                        tc.tool_call_id.clone(), output.clone(), None, None,
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
                                for def in mcp_registry.iter().filter(|d| d.server_name == item_name) {
                                    if let Some(client) = mcp_clients.get(&def.server_name) {
                                        if !tools.iter().any(|t| t.name() == def.name) {  // duplicate guard
                                            tools.push(make_mcp_tool_def(def, client.clone()));
                                        }
                                    }
                                }
                                // Skill: no tools push needed — content is returned by LoadTool.execute()
                            } else {
                                // Remove all tools from this server
                                tools.retain(|t| {
                                    mcp_registry.iter()
                                        .find(|d| d.name == t.name())
                                        .map(|d| d.server_name != item_name)
                                        .unwrap_or(true)
                                });
                            }
                            let toggle_kind = if mcp_registry.iter().any(|d| d.server_name == item_name) {
                                "mcp"
                            } else {
                                "skill"
                            };
                            emitter.emit(events::toggle(toggle_kind, item_name, on))?;
                        }
                        let prefix = tool_approval_prefixes.get(&tc.tool_call_id).cloned();
                        let (part, mut result) = execute_tool_call(tc, &tools, emitter, prefix).await
                            .unwrap_or_else(|e| error_tool_result(tc, e));
                        let result_str = result.result.as_str().unwrap_or("").to_string();
                        for h in &hooks {
                            if let Some(new) = h.post_tool(&tc.tool_name, &tc.tool_call_id, &tc.args, &result_str).await {
                                result.result = serde_json::Value::String(new);
                            }
                        }
                        // Apply output filter — in-memory only; transcript keeps raw result.
                        let result_str = result.result.as_str().map(|s| s.to_string())
                            .unwrap_or_else(|| result.result.to_string());
                        if let Some(parse_ops) = tc.args.get("parse") {
                            result.result = serde_json::Value::String(apply_parse_filter(&result_str, parse_ops));
                        } else if let Some(curate_prompt) = tc.args.get("curate_prompt").and_then(|v| v.as_str()) {
                            let gait_dir = gait_dir.clone();
                            let codebase_path = codebase_path.clone();
                            let model = model.clone();
                            let provider_clone = provider.clone();
                            result.result = Box::pin(async move {
                                apply_curate_filter(
                                    &result_str, curate_prompt,
                                    &gait_dir, &codebase_path, &model, provider_clone,
                                ).await
                            }).await;
                        }
                        all_results[*i] = Some((part, result));
                    }
                    ExecutionSignal::TurnComplete => {
                        let message = tc.args.get("message").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        let result = tc.args.get("result").cloned();

                        // Validate result against schema if configured.
                        if let Some(ref schema) = result_schema {
                            let value_to_validate = result.clone().unwrap_or(serde_json::Value::Null);
                            if let Err(errors) = validate_against_schema(schema, &value_to_validate) {
                                if schema_retries_remaining > 0 {
                                    schema_retries_remaining -= 1;
                                    emitter.emit(events::tool_call(
                                        tc.tool_call_id.clone(), tc.tool_name.clone(), &tc.args,
                                    ))?;
                                    let error_msg = format!(
                                        "result schema validation failed ({} retries remaining):\n{}",
                                        schema_retries_remaining, errors
                                    );
                                    emitter.emit(events::tool_result(
                                        tc.tool_call_id.clone(), error_msg, None, None,
                                    ))?;
                                    continue;
                                } else {
                                    return Err(anyhow::anyhow!("result schema validation failed after all retries:\n{}", errors));
                                }
                            }
                        }

                        // Emit call+result pair for transcript auditability before ending the turn.
                        emitter.emit(events::tool_call(
                            tc.tool_call_id.clone(), tc.tool_name.clone(), &tc.args,
                        ))?;
                        emitter.emit(events::tool_result(
                            tc.tool_call_id.clone(), "recorded".to_string(), None, None,
                        ))?;
                        for t in post_tool_text.drain(..) {
                            emitter.delta(t);
                        }
                        let events_now = ingestor.events()?;
                        let turn_n = count_turn_complete(&events_now) as u32 + 1;
                        let code_checkpoint = checkpoint_codebase(
                            repos.checkpoints.as_ref(),
                            &gait_dir, &session_id, &codebase_path, turn_n,
                        );
                        let usage = TurnUsage {
                            input: total_usage.input_tokens as u64,
                            output: total_usage.output_tokens as u64,
                            cache_read: total_usage.cache_read_tokens as u64,
                            cache_write: total_usage.cache_write_tokens as u64,
                        };
                        let turn_output = TurnOutput { message: message.clone(), result: result.clone() };
                        emitter.emit(events::turn_complete(
                            &usage, codebase_hash.clone(), codebase_dirty, code_checkpoint,
                            split_into_blocks(&all_turn_text), &turn_output,
                        ))?;
                        repos.traces.commit_session(&gait_dir, &session_id,
                            &format!("session({}): turn {}", session_id, turn_n))?;
                        for h in &hooks { h.post_turn(); }
                        return Ok(TurnResult::Done { message, result });
                    }
                    ExecutionSignal::Clarify => {
                        let items: Vec<ClarifyItem> = tc.args.get("items")
                            .and_then(|v| serde_json::from_value(v.clone()).ok())
                            .unwrap_or_default();
                        emitter.emit(events::clarify(&items))?;
                        clarify_signal = Some(items);
                    }
                    ExecutionSignal::Inform => {
                        let items: Vec<InformItem> = tc.args.get("items")
                            .and_then(|v| serde_json::from_value(v.clone()).ok())
                            .unwrap_or_default();
                        emitter.emit(events::inform(&items))?;
                        inform_signal = Some(items);
                    }
                    ExecutionSignal::Continue => {
                        // skill (default) and any other serial tool
                        let prefix = tool_approval_prefixes.get(&tc.tool_call_id).cloned();
                        let (part, mut result) = execute_tool_call(tc, &tools, emitter, prefix).await
                            .unwrap_or_else(|e| error_tool_result(tc, e));
                        let result_str = result.result.as_str().unwrap_or("").to_string();
                        for h in &hooks {
                            if let Some(new) = h.post_tool(&tc.tool_name, &tc.tool_call_id, &tc.args, &result_str).await {
                                result.result = serde_json::Value::String(new);
                            }
                        }
                        // Apply output filter — in-memory only; transcript keeps raw result.
                        let result_str = result.result.as_str().map(|s| s.to_string())
                            .unwrap_or_else(|| result.result.to_string());
                        if let Some(parse_ops) = tc.args.get("parse") {
                            result.result = serde_json::Value::String(apply_parse_filter(&result_str, parse_ops));
                        } else if let Some(curate_prompt) = tc.args.get("curate_prompt").and_then(|v| v.as_str()) {
                            let gait_dir = gait_dir.clone();
                            let codebase_path = codebase_path.clone();
                            let model = model.clone();
                            let provider_clone = provider.clone();
                            result.result = Box::pin(async move {
                                apply_curate_filter(
                                    &result_str, curate_prompt,
                                    &gait_dir, &codebase_path, &model, provider_clone,
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

        // Emit any model text that arrived after tool calls in this step.
        for t in post_tool_text.drain(..) {
            emitter.delta(t);
        }

        // Gated tools: emit Permissions and exit so the user can decide.
        // (In auto mode gated_ids is always empty — this block is never reached.)
        if !pending_permissions.is_empty() {
            emitter.emit(events::permissions(&pending_permissions))?;
            repos.traces.commit_session(&gait_dir, &session_id,
                &format!("session({}): permissions", session_id))?;
            for h in &hooks { h.post_turn(); }
            return Ok(TurnResult::Permissions);
        }

        // Exit turn for clarify/inform — event already in transcript+stdout via emit_event
        if let Some(items) = clarify_signal.take() {
            repos.traces.commit_session(&gait_dir, &session_id,
                &format!("session({}): clarify", session_id))?;
            for h in &hooks { h.post_turn(); }
            return Ok(TurnResult::Clarify { items });
        }
        if let Some(items) = inform_signal.take() {
            repos.traces.commit_session(&gait_dir, &session_id,
                &format!("session({}): inform", session_id))?;
            for h in &hooks { h.post_turn(); }
            return Ok(TurnResult::Inform { items });
        }

        // Append this step to messages for next iteration
        messages.push(Message::Assistant { content: AssistantContent::Parts(assistant_parts) });
        messages.push(Message::Tool { content: tool_results });
    }

    anyhow::bail!("reached max steps ({}) without turn_complete", max_iter.unwrap_or(100))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::trace::{GitTraceRepo, TraceRepo};
    use crate::session::transcript::{read_events, PermissionsReplyItem};
    use crate::provider::Provider;
    use tempfile::TempDir;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use crate::provider::ProviderError;
    use crate::types::ToolDef;
    use crate::tools::OutputTool;
    use async_stream::stream;
    #[test]
    fn turn_context_is_wrapped_in_status_tag() {
        let ctx = build_turn_context("deadbeef", &["src/a.rs".to_string()], std::path::Path::new("/repo/sub"));
        assert!(ctx.starts_with("<status>"), "got: {ctx}");
        assert!(ctx.trim_end().ends_with("</status>"), "got: {ctx}");
        assert!(ctx.contains("Codebase commit: deadbeef"));
        assert!(ctx.contains("src/a.rs"));
        assert!(ctx.contains("/repo/sub %"), "expected terminal-style cwd prompt, got: {ctx}");
    }


    /// Bridges old per-field test call sites to the new run_turn(&Session, ResolvedProfile, TurnInput).
    async fn run_turn_test_call(
        gait_dir: std::path::PathBuf,
        codebase_path: std::path::PathBuf,
        session_id: String,
        provider: std::sync::Arc<dyn Provider>,
        tools: Vec<Box<dyn Tool>>,
        allow: Vec<String>,
        deny: Vec<String>,
        input: TurnInput,
        repos: crate::git::Repos,
    ) -> anyhow::Result<TurnResult> {
        let session = crate::session::Session {
            id: session_id,
            gait_dir,
            codebase_path,
            assigned_by: None,
            repos,
        };
        let caps = crate::profile::ResolvedProfile::for_test(provider, tools, allow, deny);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<crate::proto::gait::log::v1::StreamItem>();
        let emitter = Emitter::for_session(&session.gait_dir, &session.id, tx);
        run_turn(&session, &emitter, caps, input).await
    }


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
        // Codebase dir (can be same as gait_dir for test)
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
            permission: None,
        };

        run_turn_test_call(
            ws.path().to_path_buf(), codebase, "stest001".into(),
            provider, vec![Box::new(read_tool)],
            vec!["read".into()], vec![],
            TurnInput { task: Some("do the thing".into()), auto: true, max_iter: None },
            crate::git::Repos::production(),
        ).await.unwrap();

        // Verify transcript has expected events
        let tx_path = ws.path().join("sessions/stest001/log.jsonl");
        let events = read_events(&tx_path).unwrap();
        let types: Vec<&str> = events.iter().map(|e| match event_kind(e) {
            Some(payload::Kind::SessionStart(_)) => "session_start",
            Some(payload::Kind::UserTurn(_)) => "user_turn",
            Some(payload::Kind::ToolCall(_)) => "tool_call",
            Some(payload::Kind::ToolResult(_)) => "tool_result",
            Some(payload::Kind::AssistantText(_)) => "assistant_text",
            Some(payload::Kind::TurnComplete(_)) => "turn_complete",
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
                        tool_name: "structured_done".into(),
                        args: serde_json::json!({ "message": "fixed the bug", "result": { "files": 2 } }),
                    },
                    StreamChunk::Done,
                ],
            ]),
            call_idx: Arc::new(AtomicUsize::new(0)),
        });

        let output_tool = OutputTool::new();

        run_turn_test_call(
            ws.path().to_path_buf(), ws.path().to_path_buf(), "stest_out".into(),
            provider, vec![output_tool.boxed()],
            vec![], vec![],
            TurnInput { task: Some("fix a bug".into()), auto: true, max_iter: None },
            crate::git::Repos::production(),
        ).await.unwrap();

        let tx_path = ws.path().join("sessions/stest_out/log.jsonl");
        let events = read_events(&tx_path).unwrap();
        let tc = events.iter().find_map(|e| match event_kind(e) {
            Some(payload::Kind::TurnComplete(tc)) => Some(tc),
            _ => None,
        }).unwrap();
        let output = tc.output.as_ref().unwrap();
        assert_eq!(output.message, "fixed the bug");
        assert!(output.result.is_some());
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
                        tool_name: "structured_done".into(),
                        args: serde_json::json!({ "message": "done" }),
                    },
                    StreamChunk::Done,
                ],
            ]),
            call_idx: Arc::new(AtomicUsize::new(0)),
        });

        let output_tool = OutputTool::new();

        run_turn_test_call(
            ws.path().to_path_buf(), ws.path().to_path_buf(), "stest_out2".into(),
            provider, vec![output_tool.boxed()],
            vec![], vec![],
            TurnInput { task: Some("do it".into()), auto: false, max_iter: None },
            crate::git::Repos::production(),
        ).await.unwrap();

        let tx_path = ws.path().join("sessions/stest_out2/log.jsonl");
        let events = read_events(&tx_path).unwrap();
        let has_tc = events.iter().any(|e| matches!(event_kind(e), Some(payload::Kind::TurnComplete(_))));
        let has_permissions = events.iter().any(|e| matches!(event_kind(e), Some(payload::Kind::Permissions(_))));
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

        run_turn_test_call(
            ws.path().to_path_buf(), ws.path().to_path_buf(), "stest002".into(),
            provider, vec![],
            vec!["read".into()], vec![],
            TurnInput { task: Some("do something dangerous".into()), auto: false, max_iter: None },
            crate::git::Repos::production(),
        ).await.unwrap();

        let tx_path = ws.path().join("sessions/stest002/log.jsonl");
        let events = read_events(&tx_path).unwrap();
        let has_permissions = events.iter().any(|e| matches!(event_kind(e), Some(payload::Kind::Permissions(_))));
        let has_turn_complete = events.iter().any(|e| matches!(event_kind(e), Some(payload::Kind::TurnComplete(_))));
        assert!(has_permissions, "should have permissions event");
        assert!(!has_turn_complete, "should NOT have turn_complete — turn was paused");
    }

    // ── unified <loadable-index> + retrace::enabled_loadables ─────────────────

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

        let result = run_turn_test_call(
            ws.path().to_path_buf(), ws.path().to_path_buf(), "sdone001".into(),
            provider, vec![],
            vec![], vec![],
            TurnInput { task: Some("do something".into()), auto: true, max_iter: None },
            crate::git::Repos::production(),
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

        let result = run_turn_test_call(
            ws.path().to_path_buf(), ws.path().to_path_buf(), "sperm001".into(),
            provider, vec![],
            vec![], vec![],
            TurnInput { task: Some("list files".into()), auto: false, max_iter: None },
            crate::git::Repos::production(),
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

        run_turn_test_call(
            ws.path().to_path_buf(), ws.path().to_path_buf(), "sdelta01".into(),
            provider, vec![],
            vec![], vec![],
            TurnInput { task: Some("say hello".into()), auto: true, max_iter: None },
            crate::git::Repos::production(),
        ).await.unwrap();

        let tx_path = ws.path().join("sessions/sdelta01/log.jsonl");
        let events = read_events(&tx_path).unwrap();
        let text_events: Vec<_> = events.iter().filter_map(|e| match event_kind(e) {
            Some(payload::Kind::AssistantText(a)) => Some(a),
            _ => None,
        }).collect();
        assert_eq!(text_events.len(), 1);
        assert_eq!(text_events[0].content, "hello world");
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
                        tool_name: "structured_done".into(),
                        args: serde_json::json!({ "message": "blocked" }),
                    },
                    StreamChunk::Done,
                ],
            ]),
            call_idx: Arc::new(AtomicUsize::new(0)),
        });

        let shell_tool = crate::tools::ShellTool::new(
            crate::tools::ToolCtx::new(ws.path().to_path_buf(), ws.path().to_path_buf(), "test".into()),
        );
        let output_tool = OutputTool::new();

        run_turn_test_call(
            ws.path().to_path_buf(), ws.path().to_path_buf(), "sdeny001".into(),
            provider, vec![shell_tool.boxed(), output_tool.boxed()],
            vec![], vec!["shell:rm *".to_string()],
            TurnInput { task: Some("try to destroy".into()), auto: false, max_iter: None },
            crate::git::Repos::production(),
        ).await.unwrap();

        let tx_path = ws.path().join("sessions/sdeny001/log.jsonl");
        let events = read_events(&tx_path).unwrap();

        // Must have a ToolResult indicating blocked by config
        let blocked_result = events.iter().any(|e| matches!(event_kind(e), Some(payload::Kind::ToolResult(tr))
            if tr.call_id == "tc1" && tr.output.contains("Blocked by config")));
        assert!(blocked_result, "expected blocked ToolResult for tc1; events: {:?}", events);

        // NO Permissions event should be emitted (deny is a hard floor, not a prompt)
        let has_permissions = events.iter().any(|e| matches!(event_kind(e), Some(payload::Kind::Permissions(_))));
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
                        tool_name: "structured_done".into(),
                        args: serde_json::json!({ "message": "blocked" }),
                    },
                    StreamChunk::Done,
                ],
            ]),
            call_idx: Arc::new(AtomicUsize::new(0)),
        });

        let shell_tool = crate::tools::ShellTool::new(
            crate::tools::ToolCtx::new(ws.path().to_path_buf(), ws.path().to_path_buf(), "test".into()),
        );
        let output_tool = OutputTool::new();

        run_turn_test_call(
            ws.path().to_path_buf(), ws.path().to_path_buf(), "sdeny002".into(),
            provider, vec![shell_tool.boxed(), output_tool.boxed()],
            vec!["shell:rm *".to_string()], vec!["shell:rm *".to_string()],
            TurnInput { task: Some("try to destroy".into()), auto: false, max_iter: None },
            crate::git::Repos::production(),
        ).await.unwrap();

        let tx_path = ws.path().join("sessions/sdeny002/log.jsonl");
        let events = read_events(&tx_path).unwrap();

        // Same assertions as the first deny test
        let blocked_result = events.iter().any(|e| matches!(event_kind(e), Some(payload::Kind::ToolResult(tr))
            if tr.call_id == "tc1" && tr.output.contains("Blocked by config")));
        assert!(blocked_result, "expected blocked ToolResult for tc1 even with allow match; events: {:?}", events);

        let has_permissions = events.iter().any(|e| matches!(event_kind(e), Some(payload::Kind::Permissions(_))));
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
        GitTraceRepo.create_session_worktree(ws.path(), session_id).unwrap();
        let session_dir = ws.path().join("sessions").join(session_id);
        let tx_path = session_dir.join("log.jsonl");

        // Pre-populate transcript: SessionStart + UserTurn + Permissions + PermissionsReply
        let perm_item = PermissionItem {
            id: "tc-resume-1".into(),
            cmd: "echo hello".into(),
            reason: "shell requires permission".into(),
            patterns: vec!["echo *".into()],
            tool_name: "shell".into(),
            tool_args: serde_json::json!({"cmd": "echo hello", "reason": "test"}),
        };
        wire::append(&tx_path, events::session_start(session_id.into(), None)).unwrap();
        wire::append(&tx_path, events::user_turn("run echo".into(), None, None)).unwrap();
        wire::append(&tx_path, events::permissions(&[perm_item])).unwrap();
        wire::append(&tx_path, events::permissions_reply("user".into(), &[PermissionsReplyItem {
            id: "tc-resume-1".into(), decision: "approve".into(),
            text: Some("shell:echo *".into()), scope: None,
        }])).unwrap();

        let shell_tool = ToolDef {
            name: "shell".into(),
            description: "run a shell command".into(),
            parameters: serde_json::json!({ "type": "object" }),
            execute: Box::new(|_args| Box::pin(async move {
                Ok(serde_json::Value::String("hello\n".into()))
            })),
            permission: None,
        };
        let provider = Arc::new(FakeProvider {
            steps: Arc::new(vec![FakeStep::text("done")]),
            call_idx: Arc::new(AtomicUsize::new(0)),
        });
        let result = run_turn_test_call(
            ws.path().to_path_buf(), codebase.path().to_path_buf(), session_id.into(),
            provider, vec![Box::new(shell_tool)],
            vec!["shell:echo *".into()], vec![],
            TurnInput { task: None, auto: false, max_iter: None },
            crate::git::Repos::production(),
        ).await.unwrap();

        assert!(matches!(result, TurnResult::Done { .. }), "expected Done, got: {:?}", result);

        let events = read_events(&tx_path).unwrap();
        // ToolCall + ToolResult must appear (from direct execution, not re-request)
        assert!(events.iter().any(|e| matches!(event_kind(e), Some(payload::Kind::ToolCall(tc)) if tc.name == "shell")),
            "expected ToolCall from resume");
        assert!(events.iter().any(|e| matches!(event_kind(e), Some(payload::Kind::ToolResult(_)))),
            "expected ToolResult from resume");
        // No extra Permissions event (tool was executed, not gated again)
        let perm_count = events.iter().filter(|e| matches!(event_kind(e), Some(payload::Kind::Permissions(_)))).count();
        assert_eq!(perm_count, 1, "should have only the pre-existing Permissions event");
    }

    #[tokio::test]
    async fn resume_emits_denied_tool_result_for_rejected_call() {
        let ws = TempDir::new().unwrap();
        init_git_repo(ws.path());
        let codebase = TempDir::new().unwrap();
        let session_id = "resume-deny-test";
        // Initialize the session worktree (creates the session dir + branch)
        GitTraceRepo.create_session_worktree(ws.path(), session_id).unwrap();
        let session_dir = ws.path().join("sessions").join(session_id);
        let tx_path = session_dir.join("log.jsonl");

        let perm_item = PermissionItem {
            id: "tc-deny-1".into(), cmd: "rm -rf /".into(),
            reason: "shell requires permission".into(), patterns: vec!["rm *".into()],
            tool_name: "shell".into(),
            tool_args: serde_json::json!({"cmd": "rm -rf /", "reason": "cleanup"}),
        };
        wire::append(&tx_path, events::session_start(session_id.into(), None)).unwrap();
        wire::append(&tx_path, events::user_turn("clean up".into(), None, None)).unwrap();
        wire::append(&tx_path, events::permissions(&[perm_item])).unwrap();
        wire::append(&tx_path, events::permissions_reply("user".into(), &[PermissionsReplyItem {
            id: "tc-deny-1".into(), decision: "deny".into(), text: None, scope: None,
        }])).unwrap();

        let shell_tool = ToolDef {
            name: "shell".into(),
            description: "run a shell command".into(),
            parameters: serde_json::json!({ "type": "object" }),
            execute: Box::new(|_args| Box::pin(async move {
                Ok(serde_json::Value::String("should not run".into()))
            })),
            permission: None,
        };
        let provider = Arc::new(FakeProvider {
            steps: Arc::new(vec![FakeStep::text("ok I won't do that")]),
            call_idx: Arc::new(AtomicUsize::new(0)),
        });
        let result = run_turn_test_call(
            ws.path().to_path_buf(), codebase.path().to_path_buf(), session_id.into(),
            provider, vec![Box::new(shell_tool)],
            vec![], vec![],
            TurnInput { task: None, auto: false, max_iter: None },
            crate::git::Repos::production(),
        ).await.unwrap();

        assert!(matches!(result, TurnResult::Done { .. }), "expected Done, got: {:?}", result);

        // Must have a ToolResult for the denied call (API invariant)
        let events_after = read_events(&tx_path).unwrap();
        let denied_result = events_after.iter().any(|e| matches!(event_kind(e), Some(payload::Kind::ToolResult(tr)) if tr.call_id == "tc-deny-1"));
        assert!(denied_result, "expected ToolResult for denied call");
        if let Some(payload::Kind::ToolResult(tr)) = events_after.iter().find_map(|e| match event_kind(e) {
            Some(payload::Kind::ToolResult(tr)) if tr.call_id == "tc-deny-1" => Some(payload::Kind::ToolResult(tr)),
            _ => None,
        }) {
            assert!(tr.output.contains("denied"), "got: {}", tr.output);
        }

        // Reconstruction must yield paired messages — not drop the orphan ToolResult.
        // Without a preceding ToolCall the ToolResult is silently skipped by context.rs,
        // so the model never sees the denial and retries the tool call in a loop.
        let messages = retrace::messages(&events_after).unwrap();
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
        GitTraceRepo.create_session_worktree(ws.path(), session_id).unwrap();
        let tx_path = ws.path().join("sessions").join(session_id).join("log.jsonl");

        // Pre-write a transcript with a Permissions+PermissionsReply already in it,
        // simulating a previous turn that was interrupted mid-permission-prompt.
        // The PermissionsReply has scope="agent" to indicate it was saved to agent config.
        let shell_args = serde_json::json!({ "cmd": "ls", "reason": "list files" });
        wire::append(&tx_path, events::session_start(session_id.into(), None)).unwrap();
        wire::append(&tx_path, events::user_turn("list files".into(), None, None)).unwrap();
        wire::append(&tx_path, events::permissions(&[PermissionItem {
            id: "tc-scope-1".into(),
            cmd: "ls".into(),
            reason: "shell requires permission".into(),
            patterns: vec!["shell:ls *".to_string()],
            tool_name: "shell".into(),
            tool_args: shell_args.clone(),
        }])).unwrap();
        wire::append(&tx_path, events::permissions_reply("user".into(), &[PermissionsReplyItem {
            id: "tc-scope-1".into(),
            decision: "approve".into(),
            text: None,
            scope: Some("agent".into()),
        }])).unwrap();

        let shell_tool = ToolDef {
            name: "shell".into(),
            description: "run a shell command".into(),
            parameters: serde_json::json!({ "type": "object" }),
            execute: Box::new(|_args| Box::pin(async move {
                Ok(serde_json::Value::String("file1\nfile2\n".into()))
            })),
            permission: None,
        };

        // After resuming and executing shell, agent calls output to complete
        let provider = Arc::new(FakeProvider {
            steps: Arc::new(vec![FakeStep::text("done")]),
            call_idx: Arc::new(AtomicUsize::new(0)),
        });

        run_turn_test_call(
            ws.path().to_path_buf(), ws.path().to_path_buf(), session_id.into(),
            provider, vec![Box::new(shell_tool)],
            vec![], vec![],
            TurnInput { task: None, auto: false, max_iter: None },
            crate::git::Repos::production(),
        ).await.unwrap();

        let events = read_events(&tx_path).unwrap();
        // The shell tool result emitted during resume should contain the whitelist prefix
        let whitelist_result = events.iter().any(|e| {
            if let Some(payload::Kind::ToolResult(tr)) = event_kind(e) {
                tr.call_id == "tc-scope-1" && tr.output.contains("[User added to whitelist:")
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

    #[test]
    fn checkpoint_codebase_returns_sha_and_records_call() {
        use crate::git::checkpoint::MockCheckpointRepo;
        let ws = tempfile::TempDir::new().unwrap();
        let codebase = ws.path().to_path_buf();
        let checkpoints = MockCheckpointRepo::new();
        let sha = checkpoint_codebase(&checkpoints, ws.path(), "scp1", &codebase, 1);
        assert_eq!(sha.as_deref(), Some("mock-sha-1"));
        assert_eq!(checkpoints.checkpoint_calls(), vec![("scp1".to_string(), 1u32)]);
    }

    #[test]
    fn checkpoint_codebase_tolerates_error() {
        use crate::git::checkpoint::MockCheckpointRepo;
        let ws = tempfile::TempDir::new().unwrap();
        let codebase = ws.path().to_path_buf();
        let checkpoints = MockCheckpointRepo::new();
        checkpoints.inject_checkpoint_error("disk full");
        let sha = checkpoint_codebase(&checkpoints, ws.path(), "scp2", &codebase, 1);
        assert!(sha.is_none());
    }

    #[tokio::test]
    async fn run_turn_records_code_checkpoint_for_git_codebase() {
        let ws = TempDir::new().unwrap();
        init_git_repo(ws.path());

        // Build a SEPARATE codebase dir that is its own git repo with one commit
        let codebase_dir = TempDir::new().unwrap();
        {
            let repo = git2::Repository::init(codebase_dir.path()).unwrap();
            let mut cfg = repo.config().unwrap();
            cfg.set_str("user.name", "test").unwrap();
            cfg.set_str("user.email", "test@test.com").unwrap();
            std::fs::write(codebase_dir.path().join("hello.txt"), "hello\n").unwrap();
            let mut idx = repo.index().unwrap();
            idx.add_path(std::path::Path::new("hello.txt")).unwrap();
            idx.write().unwrap();
            let tree_id = idx.write_tree().unwrap();
            let tree = repo.find_tree(tree_id).unwrap();
            let sig = git2::Signature::now("test", "test@test.com").unwrap();
            repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[]).unwrap();
        }

        let session_id = "scpgit001";

        let provider = Arc::new(MockProvider {
            responses: Arc::new(vec![
                vec![
                    StreamChunk::Text("done".into()),
                    StreamChunk::Done,
                ],
            ]),
            call_idx: Arc::new(AtomicUsize::new(0)),
        });

        run_turn_test_call(
            ws.path().to_path_buf(), codebase_dir.path().to_path_buf(), session_id.into(),
            provider, vec![],
            vec![], vec![],
            TurnInput { task: Some("checkpoint test".into()), auto: true, max_iter: None },
            crate::git::Repos::production(),
        ).await.unwrap();

        let tx_path = ws.path().join("sessions").join(session_id).join("log.jsonl");
        let events = read_events(&tx_path).unwrap();
        let cp = events.iter().rev().find_map(|e| match event_kind(e) {
            Some(payload::Kind::TurnComplete(tc)) => Some(tc.checkpoint.clone()),
            _ => None,
        }).flatten();
        assert!(cp.is_some(), "expected a code checkpoint SHA");
        assert!(crate::git::checkpoint_git_dir(ws.path(), session_id).exists());
    }

    #[tokio::test]
    async fn run_turn_persists_codebase_path_in_session_json() {
        let ws = TempDir::new().unwrap();
        init_git_repo(ws.path());

        // Build a SEPARATE codebase dir that is its own git repo with one commit
        let codebase_dir = TempDir::new().unwrap();
        {
            let repo = git2::Repository::init(codebase_dir.path()).unwrap();
            let mut cfg = repo.config().unwrap();
            cfg.set_str("user.name", "test").unwrap();
            cfg.set_str("user.email", "test@test.com").unwrap();
            std::fs::write(codebase_dir.path().join("main.rs"), "fn main() {}\n").unwrap();
            let mut idx = repo.index().unwrap();
            idx.add_path(std::path::Path::new("main.rs")).unwrap();
            idx.write().unwrap();
            let tree_id = idx.write_tree().unwrap();
            let tree = repo.find_tree(tree_id).unwrap();
            let sig = git2::Signature::now("test", "test@test.com").unwrap();
            repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[]).unwrap();
        }

        let session_id = "scbpath001";

        let provider = Arc::new(MockProvider {
            responses: Arc::new(vec![
                vec![
                    StreamChunk::Text("all done".into()),
                    StreamChunk::Done,
                ],
            ]),
            call_idx: Arc::new(AtomicUsize::new(0)),
        });

        run_turn_test_call(
            ws.path().to_path_buf(), codebase_dir.path().to_path_buf(), session_id.into(),
            provider, vec![],
            vec![], vec![],
            TurnInput { task: Some("test codebase_path persistence".into()), auto: true, max_iter: None },
            crate::git::Repos::production(),
        ).await.unwrap();

        // After the turn, session.json should have codebase_path persisted
        use crate::session::session_json::read_session_json;
        let sj = read_session_json(ws.path(), session_id);
        assert!(sj.codebase_path.is_some(), "codebase_path should be persisted in session.json");
        assert_eq!(sj.codebase_path.as_deref(), Some(codebase_dir.path().to_string_lossy().as_ref()),
            "codebase_path value should match the input");
    }

    #[tokio::test]
    async fn run_turn_calls_rewind_init_and_checkpoint_via_injected_repo() {
        use crate::git::checkpoint::MockCheckpointRepo;

        let ws = TempDir::new().unwrap();
        init_git_repo(ws.path());

        // Build a SEPARATE codebase dir that is its own git repo with one commit
        let codebase_dir = TempDir::new().unwrap();
        {
            let repo = git2::Repository::init(codebase_dir.path()).unwrap();
            let mut cfg = repo.config().unwrap();
            cfg.set_str("user.name", "test").unwrap();
            cfg.set_str("user.email", "test@test.com").unwrap();
            std::fs::write(codebase_dir.path().join("hello.txt"), "hello\n").unwrap();
            let mut idx = repo.index().unwrap();
            idx.add_path(std::path::Path::new("hello.txt")).unwrap();
            idx.write().unwrap();
            let tree_id = idx.write_tree().unwrap();
            let tree = repo.find_tree(tree_id).unwrap();
            let sig = git2::Signature::now("test", "test@test.com").unwrap();
            repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[]).unwrap();
        }

        let session_id = "rewindinject001";

        let provider = Arc::new(MockProvider {
            responses: Arc::new(vec![
                vec![
                    StreamChunk::Text("done".into()),
                    StreamChunk::Done,
                ],
            ]),
            call_idx: Arc::new(AtomicUsize::new(0)),
        });

        let mock = Arc::new(MockCheckpointRepo::new());
        let repos = crate::git::Repos {
            codebase: Arc::new(crate::git::codebase::GitCodebaseRepo),
            traces: Arc::new(crate::git::trace::GitTraceRepo),
            checkpoints: mock.clone(),
        };

        run_turn_test_call(
            ws.path().to_path_buf(), codebase_dir.path().to_path_buf(), session_id.into(),
            provider, vec![],
            vec![], vec![],
            TurnInput { task: Some("checkpoint test".into()), auto: true, max_iter: None },
            repos,
        ).await.unwrap();

        assert_eq!(mock.init_calls().len(), 1, "rewind.init should be called exactly once at session start");
        assert_eq!(mock.checkpoint_calls(), vec![(session_id.to_string(), 1u32)],
                   "rewind.checkpoint should be called for turn 1");
    }

    #[tokio::test]
    async fn load_skill_emits_toggle_event() {
        let ws = TempDir::new().unwrap();
        init_git_repo(ws.path());

        let provider = Arc::new(MockProvider {
            responses: Arc::new(vec![
                vec![
                    StreamChunk::ToolCallComplete {
                        tool_call_id: "tc1".into(),
                        tool_name: "load".into(),
                        args: serde_json::json!({ "name": "tdd", "enabled": true }),
                    },
                    StreamChunk::Done,
                ],
                vec![
                    StreamChunk::ToolCallComplete {
                        tool_call_id: "tc2".into(),
                        tool_name: "structured_done".into(),
                        args: serde_json::json!({ "message": "done" }),
                    },
                    StreamChunk::Done,
                ],
            ]),
            call_idx: Arc::new(AtomicUsize::new(0)),
        });

        let skill = crate::config::SkillDef {
            name: "tdd".into(),
            description: String::new(),
            content: "TDD skill content".into(),
            enabled: false,
        };
        let load_tool = crate::tools::LoadTool::new(vec![], vec![skill]);
        let output_tool = OutputTool::new();

        run_turn_test_call(
            ws.path().to_path_buf(), ws.path().to_path_buf(), "slogtgl01".into(),
            provider, vec![Box::new(load_tool), Box::new(output_tool)],
            vec!["load".into()], vec![],
            TurnInput { task: Some("enable tdd".into()), auto: true, max_iter: None },
            crate::git::Repos::production(),
        ).await.unwrap();

        let tx_path = ws.path().join("sessions/slogtgl01/log.jsonl");
        let events = crate::proto::wire::read_events(&tx_path).unwrap();
        let toggles: Vec<_> = events.iter().filter_map(|e| match event_kind(e) {
            Some(payload::Kind::Toggle(t)) => Some(t),
            _ => None,
        }).collect();
        assert_eq!(toggles.len(), 1, "exactly one Toggle event emitted");
        assert_eq!(toggles[0].name, "tdd");
        assert!(toggles[0].enabled, "Toggle enabled=true");
        use crate::proto::gait::log::v1::ToggleKind;
        assert_eq!(toggles[0].kind, ToggleKind::Skill as i32, "skill kind — not in mcp_registry");
    }
}

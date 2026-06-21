use std::path::{Path, PathBuf};

use crate::provider::StreamChunk;
use crate::text::split_into_blocks;
#[allow(unused_imports)] // `Tool` is used by `#[cfg(test)]` in tests.rs
use crate::types::{
    AssistantContent, AssistantPart, ExecutionMode, ExecutionSignal, HookAction, HookContext,
    InjectRole, Message, MessageContent, MessageMutation, MutationPayload, Tool, ToolCall,
    ToolPermission, ToolResult, ToolResultKind, Usage,
};
use futures_util::StreamExt;

use crate::config::McpToolDef;
use crate::proto::gait::log::v1::TurnTokenUsage;
use crate::proto::Recorder;
use crate::proto::{events, wire};
use crate::session::log::{count_turn_complete, init_session, now_ms, InitSessionOpts};
use crate::session::retrace::{
    self, active_mutations, active_unlock_state, apply_cascade, coalesce, run_projection,
    ActiveRawMessages,
};
use crate::session::Ingestor;

use crate::git::checkpoint::CheckpointRepo;

use crate::permission::find_matched_deny_pattern;

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

#[derive(serde::Deserialize)]
struct InformArg {
    id: String,
    content: String,
    #[serde(default)]
    context: Option<String>,
}

mod curate;
mod exec;
mod gate;
pub mod nerve;
mod prompt;
pub mod spine;
pub(crate) mod synapse;

// Re-exports
#[allow(unused_imports)] // used by `#[cfg(test)]` in tests.rs
pub(crate) use exec::apply_parse_filter;
pub(crate) use prompt::build_turn_context;
pub use prompt::{
    build_loadable_catalog, build_loadable_enabled_state, build_permission_status,
    build_session_state,
};

// Internal call-site imports — let run_turn reach the moved helpers
// without prefixing every call. `super::*` would re-export our own
// `curate` and `exec` modules' items, which is undesirable; explicit
// `pub(super)` items imported by name keeps the surface narrow.
use exec::{apply_output_filter, error_tool_result, execute_tool_call, stream_preview_if_always};
use gate::{classify as gate_classify, GateDecision};

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
        changes: Option<crate::proto::gait::log::v1::Changes>,
    },
    Permissions,
    Clarify {
        items: Vec<pb::ClarifyItem>,
    },
    Inform {
        items: Vec<pb::InformItem>,
    },
    Unlock {
        items: Vec<pb::UnlockItem>,
    },
    Cancelled,
}

/// Per-call inputs for one turn (the things that vary every invocation).
pub struct TurnInput {
    pub auto: bool,
    pub max_iter: Option<usize>,
    pub preview_policy: crate::config::PreviewPolicy,
    pub cancel: Option<tokio_util::sync::CancellationToken>,
    pub sandbox_on: bool,
    /// Phase 3 Task 4a: when Some, execute_tool_call uses RelaySynapse.
    /// None for direct runs (spine path sets this from LoopConfig).
    pub relay: Option<crate::turn::nerve::NerveWriter>,
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
                if std::path::Path::new(&format!("/proc/{}", pid)).exists() || {
                    // SAFETY: kill(pid, 0) sends no signal; it only checks if the process exists. Safe to call from any thread.
                    unsafe { libc::kill(pid as i32, 0) == 0 }
                } {
                    anyhow::bail!(
                        "session {} is already running (PID {})",
                        session_dir.display(),
                        pid
                    );
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
            eprintln!(
                "checkpoint failed for session {}: {}; checkpoint unavailable for this turn",
                session_id, e
            );
            None
        }
    }
}

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

/// Resolves when the cancellation token is cancelled; never resolves if no token.
async fn cancel_token_fut(token: &Option<tokio_util::sync::CancellationToken>) {
    match token {
        Some(t) => t.cancelled().await,
        None => std::future::pending::<()>().await,
    }
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
    // Build the live Agent config from the whole profile BEFORE `caps` is
    // destructured, so `events::agent_msg` stays the single source of truth.
    // Emitted later, under the drift guard.
    let agent = crate::proto::events::agent_msg(&caps);
    let provider = caps.provider.clone();
    let (mut tools, allow, deny) = caps.permissioned.into_parts();
    let session_id = session.id.clone();
    let gait_dir = session.gait_dir.clone();
    let codebase_path = session.codebase_path.clone();
    let model = caps.model.clone();
    let repos = session.repos.clone();
    let system_prompt = caps.instructions;
    let mcp_registry = caps.mcp_registry;
    let mcp_clients = caps.mcp_clients;
    let skill_registry = caps.skill_registry;
    let plugin_registry = caps.plugin_registry;
    let config_enabled = caps.config_enabled;
    let hooks = caps.hooks;
    let result_schema = caps.result_schema;
    let mut schema_retries_remaining: u8 = caps.output_schema_retries;
    let llm_params = caps.llm_params.clone();

    // Build the agent-dimension seed (M3): config-derived genesis for ActiveAgent.
    // Built manually from resolved fields rather than via `AgentState::from_config`
    // (retrace.rs) for two reasons:
    //   1. We don't have a `&GaitConfig` in scope here — only the destructured
    //      `ResolvedProfile` — and plumbing the config through `run_turn` is a
    //      larger architectural change than this seed warrants.
    //   2. The seed semantics here are deliberately different from `from_config`:
    //      this path pre-fills `skills` with the FULL skill registry (not just
    //      enabled ones) so disabled-but-registered skills can be turned on
    //      later, and unions `config_enabled` (= enabled mcp_servers ∪ enabled
    //      skills, see profile.rs:114-118) into BOTH `skills` and `mcp` to
    //      reproduce the legacy `ActiveLoadables` flat-seed shape exactly.
    //      `from_config` keeps those two sets separate, so swapping it in would
    //      change the fold result. Track the migration in tech-debt #17.
    let agent_seed = {
        let mut s = retrace::AgentState {
            model: model.clone(),
            instructions: system_prompt.clone(),
            tools: Default::default(),
            skills: skill_registry.iter().map(|sk| sk.name.clone()).collect(),
            mcp: Default::default(),
            plugins: Default::default(),
            allow: allow.iter().cloned().collect(),
            deny: deny.iter().cloned().collect(),
            llm_params: None,
        };
        for n in &config_enabled {
            s.skills.insert(n.clone());
            s.mcp.insert(n.clone());
        }
        s
    };
    let auto = input.auto;
    let max_iter = input.max_iter;
    let preview_policy = input.preview_policy;
    let cancel = input.cancel;
    let sandbox_on = input.sandbox_on;
    let relay = input.relay;

    // When running under a spine (relay.is_some()), the spine performs all
    // startup disk writes before spawning the brain. The brain's seal has
    // writable: [] so it cannot write to gait-dir. Skip these writes to avoid
    // EPERM — the spine already did (or will do) them.
    if relay.is_none() {
        repos
            .traces
            .create_session_worktree(&gait_dir, &session_id)?;
        repos
            .checkpoints
            .init(&gait_dir, &session_id, &codebase_path)?;

        // Persist codebase_path (write-once) so `gait rewind` can locate the codebase.
        {
            use crate::session::session_json::{read_session_json, write_session_json};
            let mut sj = read_session_json(&gait_dir, &session_id)?;
            if sj.codebase_path.is_none() {
                sj.id = session_id.clone();
                if sj.created_at == 0 {
                    sj.created_at = now_ms();
                }
                sj.codebase_path = Some(codebase_path.to_string_lossy().to_string());
                write_session_json(&gait_dir, &sj)?;
            }
        }
    }

    let session_dir = gait_dir.join("sessions").join(&session_id);
    let tx_path = session_dir.join("log.jsonl");
    let ingestor = Ingestor::from_session(&gait_dir, &session_id);

    // SessionLock: brain doesn't hold the lock when under a spine (the spine holds it).
    let _lock = if relay.is_none() {
        Some(SessionLock::acquire(&session_dir)?)
    } else {
        None
    };

    let codebase_hash = repos.codebase.head_hash(&codebase_path).unwrap_or_default();

    let dirty_files = repos.codebase.dirty_files(&codebase_path);
    let codebase_dirty = !dirty_files.is_empty();

    // Init transcript (SessionStart) once per session. Gate on the SessionStart
    // event's presence rather than log-emptiness: under Phase 4 Option A the input
    // adapter appends this turn's user_turn BEFORE run_turn, so the log is no longer
    // empty on the first turn.
    // When running under a spine, the spine writes SessionStart — skip to avoid
    // disk writes (brain has writable: []).
    if relay.is_none() {
        let has_session_start = ingestor
            .events()
            .unwrap_or_default()
            .iter()
            .any(|e| matches!(event_kind(e), Some(payload::Kind::SessionStart(_))));
        if !has_session_start {
            init_session(InitSessionOpts {
                log_path: tx_path.clone(),
                id: session_id.clone(),
                model: model.clone(),
                codebase_hash: codebase_hash.clone(),
                codebase_dirty,
            })?;
        }
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
    // Stamp emitter position so every event recorded from this point on carries
    // the correct (turn, step). The current turn is the count of already-completed
    // turns in the log; the step starts at 0 and advances inside the loop.
    let current_turn = count_turn_complete(&pre_events) as u32;
    emitter.set_position(current_turn, 0);
    let mut messages: Vec<Message> = Vec::new();
    // Synthetic turn context (built once, prepended to every step's outgoing in
    // the step loop per plan §5d). None on the resume path — the model already
    // has context from the prior turn.
    let mut turn_ctx: Option<String> = None;
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
                    if let Err(e) = execute_tool_call(
                        &tc,
                        &tools,
                        emitter,
                        prefix,
                        &codebase_path,
                        &gait_dir,
                        &session_id,
                        sandbox_on,
                        relay.as_ref(),
                    )
                    .await
                    {
                        // Emit error as tool result so model sees it
                        emitter.record(events::tool_result(
                            id.clone(),
                            format!("error executing approved tool: {e}"),
                            None,
                            None,
                        ))?;
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
                    emitter.record(events::tool_result(
                        id.clone(),
                        "permission denied by user".into(),
                        None,
                        None,
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

    if !did_resume {
        // The input event (user_turn / reply) is appended by the caller before the
        // turn (Phase 4 Option A); here we only fold the log into messages.
        let events = ingestor.events()?;
        messages = retrace::messages(&events)?;
        let cwd = retrace::active_cwd(&events, &codebase_path);
        // turn_ctx is prepended to `outgoing` INSIDE the step loop (Task 5, plan §5d)
        // so that pre_step mutations logged on this turn land in the transcript alongside
        // a fresh turn_ctx for THIS step, not the prior turn's.
        turn_ctx = Some(build_turn_context(&codebase_hash, &dirty_files, &cwd));
    }

    // Derive enabled loadables from the FULL session (mutation-aware, via the
    // ActiveAgent projection). Loadables are a "state" projection and persist
    // across compaction — only the message context (ActiveMessages) resets on
    // compaction; consistent with permissions and agent config.
    let all_events = ingestor.events()?;
    let mut muts = retrace::active_mutations(&all_events);
    retrace::apply_cascade(&mut muts, &all_events);
    let agent_state = retrace::run_projection(
        &retrace::ActiveAgent {
            seed: agent_seed.clone(),
        },
        &all_events,
        &muts,
    );
    let enabled_names = agent_state.enabled_loadables();
    for server_name in &enabled_names {
        for def in mcp_registry
            .iter()
            .filter(|d| &d.server_name == server_name)
        {
            if let Some(client) = mcp_clients.get(&def.server_name) {
                if !tools.iter().any(|t| t.name() == def.name) {
                    // duplicate guard
                    tools.push(prompt::make_mcp_tool_def(def, client.clone()));
                }
            }
        }
    }
    // Skills are not pushed here — they surface via LoadTool.execute() return value.
    // Phase 8: tool-gating — if the agent declared an explicit tools enable-set,
    // filter the tool list to only those tools. Empty = no restriction (all pass through).
    if !agent_state.tools.is_empty() {
        tools.retain(|t| agent_state.tools.contains(t.name()));
    }

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
        mcp_tools_by_server
            .entry(def.server_name.as_str())
            .or_default()
            .push(def);
    }
    let mut entries: Vec<LoadableEntry> = Vec::new();
    // MCP server entries — one per server. Description comes from
    // McpServerDef.description (config layer); fall back to tool count.
    // Note: we don't have direct access to McpServerDef in run_turn, so
    // we build the description from the discovered tool count when
    // there's no explicit description available. Run-level only.
    for (server_name, defs) in &mcp_tools_by_server {
        let description = format!(
            "{} tool{}",
            defs.len(),
            if defs.len() == 1 { "" } else { "s" }
        );
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
    // Plugin entries — one per wasm plugin.
    for plugin in &plugin_registry {
        let basename = {
            let p = plugin.path.as_str();
            p.rsplit('/').next().unwrap_or(p)
        };
        entries.push(LoadableEntry {
            name: plugin.name.clone(),
            kind: LoadableKind::WasmPlugin,
            description: format!("wasm plugin ({})", basename),
            enabled: enabled_names.contains(&plugin.name),
        });
    }
    let mut system_prompt = prompt::build_dynamic_system_prompt(&system_prompt, &entries, &hooks);

    // MUST happen AFTER `build_dynamic_system_prompt` so `Agent.instructions`
    // captures the effective prompt (loadable index + permissions included),
    // not just the raw profile value. Capture the event ID so subsequent
    // pre_turn hooks can emit a Replace targeting it when they modify the prompt.
    let agent_event_id: String = {
        let events_for_guard = ingestor.events()?;
        let mut guard_muts = retrace::active_mutations(&events_for_guard);
        retrace::apply_cascade(&mut guard_muts, &events_for_guard);
        let guard_state = retrace::run_projection(
            &retrace::ActiveAgent {
                seed: agent_seed.clone(),
            },
            &events_for_guard,
            &guard_muts,
        );
        // Rebuild agent with the dynamic instructions for the drift check.
        let mut agent_with_dynamic = agent.clone();
        agent_with_dynamic.instructions = system_prompt.clone();
        let rebaselined = retrace::rebaseline_agent(agent_with_dynamic, &guard_state);
        if retrace::active_agent_checkpoint(&events_for_guard) != Some(rebaselined.clone()) {
            emitter
                .record(wire::body(payload::Kind::Agent(rebaselined)))?
                .id
        } else {
            // No new Agent event emitted; find the last one in the log to get its ID
            // so subsequent Replace mutations can target it.
            events_for_guard
                .iter()
                .rev()
                .find(|e| matches!(event_kind(e), Some(payload::Kind::Agent(_))))
                .map(|e| e.id.clone())
                .unwrap_or_default()
        }
    };
    // pre_turn — each hook sees the system_prompt as modified by the previous hook.
    // Snapshot the prompt so we can detect whether any hook changed it (and
    // therefore needs a Replace event against the Agent event above).
    let system_prompt_before_hooks = system_prompt.clone();
    for h in &hooks {
        let ctx = HookContext {
            event: "pre_turn".to_string(),
            session_id: session_id.clone(),
            gait_dir: gait_dir.clone(),
            codebase: codebase_path.clone(),
            system_prompt: system_prompt.clone(),
            ..Default::default()
        };
        if let Some(new_sp) = h.pre_turn(&ctx, emitter) {
            system_prompt = new_sp;
        }
    }

    // If pre_turn hooks modified the system_prompt, emit a Replace on the Agent
    // event so retrace can reconstruct the hook-modified instructions on replay.
    // Skip when `agent_event_id` is empty (no prior Agent event to target —
    // shouldn't happen because the drift guard above emits one on first run,
    // but guard defensively in case of log corruption).
    if system_prompt != system_prompt_before_hooks && !agent_event_id.is_empty() {
        // Build the replacement Agent from the last checkpoint (preserves model,
        // permissions, etc.) and overlay the hook-modified instructions.
        let replacement_agent = match retrace::active_agent_checkpoint(&ingestor.events()?) {
            Some(mut prev) => {
                prev.instructions = system_prompt.clone();
                prev
            }
            None => pb::Agent {
                instructions: system_prompt.clone(),
                ..Default::default()
            },
        };
        let _ = emitter.record(crate::proto::events::replace(
            agent_event_id.clone(),
            pb::Payload {
                kind: Some(pb::payload::Kind::Agent(replacement_agent)),
            },
            Some("pre_turn hook modified system_prompt".into()),
        ));
    }

    let mut total_usage = Usage::default();
    let mut clarify_signal: Option<Vec<pb::ClarifyItem>> = None;
    let mut inform_signal: Option<Vec<pb::InformItem>> = None;
    let mut all_turn_text = String::new();

    // 0 covers the zero-iteration case; the loop overwrites it each iteration.
    #[allow(unused_assignments)]
    let mut final_step_index = 0usize;
    for step_index in 0..max_iter.unwrap_or(100) {
        // Check cancellation before starting this step
        final_step_index = step_index;
        // Stamp (turn, step) on the emitter so any event recorded this step
        // carries the correct position. Steps advance monotonically per turn.
        emitter.set_position(current_turn, step_index as u32);
        if cancel.as_ref().map(|c| c.is_cancelled()).unwrap_or(false) {
            emitter.record(events::cancellation_signal())?;
            if relay.is_none() {
                repos.traces.commit_session(
                    &gait_dir,
                    &session_id,
                    &format!("session({}): cancelled", session_id),
                )?;
            }
            return Ok(TurnResult::Cancelled);
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
        let outgoing = coalesce(outgoing_raw);

        // 4. Build the session-state tail message (cache-safe). The status
        // (`<status>`) was previously prepended as a synthetic message at index
        // 0 — that placed it ahead of history and bust the message-prefix
        // cache every turn. Now we assemble status + permissions + loadable
        // enabled-state into one string and pass it to the provider as
        // `tail_context`; the provider appends it as an uncached trailing
        // block (Anthropic) or final message (OpenAI), so churn in the tail
        // never invalidates the cached [tools + system + history] prefix.
        let permission_status = prompt::build_permission_status(&effective_allow, &deny);
        let enabled_state = prompt::build_loadable_enabled_state(&entries);
        let session_state_body = prompt::build_session_state(
            turn_ctx.as_deref().unwrap_or(""),
            &permission_status,
            &enabled_state,
        );
        // `outgoing` is the per-step message list sent to the LLM (rebuilt from
        // the log every step under the log-first design).
        let mut chunk_stream = provider
            .stream(
                &system_prompt,
                &outgoing,
                &tools,
                &llm_params,
                Some(session_state_body.as_str()),
            )
            .await
            .map_err(|e| anyhow::anyhow!("provider error: {}", e))?;

        let mut step_text = String::new();
        let mut step_tool_calls: Vec<ToolCall> = vec![];
        let mut post_tool_text: Vec<String> = vec![];
        let mut seen_tool_complete = false;
        let mut step_thinking_text = String::new();
        let mut step_thinking_signature = String::new();

        loop {
            let chunk_opt = tokio::select! {
                biased;
                _ = cancel_token_fut(&cancel) => None,
                chunk = chunk_stream.next()   => chunk,
            };
            let Some(chunk_result) = chunk_opt else { break };
            match chunk_result.map_err(|e| anyhow::anyhow!("stream error: {}", e))? {
                StreamChunk::Text(t) => {
                    if seen_tool_complete {
                        post_tool_text.push(t.clone());
                    } else {
                        emitter.publish_delta(t.clone());
                    }
                    step_text.push_str(&t);
                }

                StreamChunk::ToolCallComplete {
                    tool_call_id,
                    tool_name,
                    args,
                } => {
                    seen_tool_complete = true;
                    step_tool_calls.push(ToolCall {
                        tool_call_id,
                        tool_name,
                        args,
                    });
                }
                StreamChunk::StepUsage(u) => {
                    total_usage = total_usage.add(&u);
                }
                StreamChunk::Done => break,
                StreamChunk::ThinkingDelta(t) => {
                    step_thinking_text.push_str(&t);
                }
                StreamChunk::ThinkingSignature { signature, .. } => {
                    step_thinking_signature = signature;
                }
                StreamChunk::ToolCallDelta { .. } => {}
            }
        }
        // ALWAYS emit ThinkingBlock to the log — show_thinking in GaitConfig
        // controls terminal rendering only; the event is always written.
        if !step_thinking_text.is_empty() {
            emitter.record(events::thinking_block(
                std::mem::take(&mut step_thinking_text),
                std::mem::take(&mut step_thinking_signature),
            ))?;
        }
        all_turn_text.push_str(&step_text);
        // Was Esc pressed during this step's stream?
        if cancel.as_ref().map(|c| c.is_cancelled()).unwrap_or(false) {
            emitter.record(events::cancellation_signal())?;
            if relay.is_none() {
                repos.traces.commit_session(
                    &gait_dir,
                    &session_id,
                    &format!("session({}): cancelled", session_id),
                )?;
            }
            return Ok(TurnResult::Cancelled);
        }
        {
            let ctx = HookContext {
                event: "post_step".to_string(),
                session_id: session_id.clone(),
                gait_dir: gait_dir.clone(),
                codebase: codebase_path.clone(),
                system_prompt: system_prompt.clone(),
                step_index,
                message_count: messages.len(),
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
        if step_tool_calls.is_empty() {
            let message = step_text.clone();
            let events_now = ingestor.events()?;
            let turn_n = count_turn_complete(&events_now) as u32 + 1;
            let code_checkpoint = checkpoint_codebase(
                repos.checkpoints.as_ref(),
                &gait_dir,
                &session_id,
                &codebase_path,
                turn_n,
            );
            let changes = code_checkpoint.as_ref().and_then(|sha| {
                repos
                    .checkpoints
                    .diff(&gait_dir, &session_id, sha)
                    .ok()
                    .flatten()
            });
            let usage = TurnTokenUsage {
                input: total_usage.input_tokens as u64,
                output: total_usage.output_tokens as u64,
                cache_read: total_usage.cache_read_tokens as u64,
                cache_write: total_usage.cache_write_tokens as u64,
            };
            emitter.record(events::turn_complete(events::TurnCompleteParams {
                usage,
                codebase_hash: codebase_hash.clone(),
                codebase_dirty,
                checkpoint: code_checkpoint,
                blocks: split_into_blocks(&all_turn_text),
                message: message.clone(),
                result: None,
                changes: changes.clone(),
            }))?;
            if relay.is_none() {
                repos.traces.commit_session(
                    &gait_dir,
                    &session_id,
                    &format!("session({}): turn {}", session_id, turn_n),
                )?;
            }
            {
                let post_turn_ctx = HookContext {
                    event: "post_turn".to_string(),
                    session_id: session_id.clone(),
                    gait_dir: gait_dir.clone(),
                    codebase: codebase_path.clone(),
                    system_prompt: system_prompt.clone(),
                    step_count: step_index + 1,
                    turn_outcome: "done".to_string(),
                    final_response: message.clone(),
                    ..Default::default()
                };
                for h in &hooks {
                    h.post_turn(&post_turn_ctx, emitter);
                }
            }
            return Ok(TurnResult::Done {
                message,
                result: None,
                changes,
            });
        }

        // Scope pre-check (supervised mode + sandbox on only): for each world-touching
        // tool call that exposes resolvable path targets, verify every target falls
        // within the sandbox ceiling (codebase root + default config). If any target
        // is outside the ceiling, pause immediately with TurnResult::Unlock — the tool
        // is NOT run. In auto mode the kernel seal (EACCES) handles out-of-scope writes
        // via Task 6; the pre-check is a supervised-only UX feature.
        if !auto && sandbox_on {
            // Build the active unlock set from the session log so that
            // once/session grants (not persisted to config) pass through.
            let active_unlock = {
                let evs = ingestor.events().unwrap_or_default();
                active_unlock_state(&evs)
            };
            let mut pending_unlocks: Vec<pb::UnlockItem> = vec![];
            for tc in &step_tool_calls {
                if let Some(tool) = tools.iter().find(|t| t.name() == tc.tool_name) {
                    let items = check_own_tool_scope(tc, tool.as_ref(), &codebase_path);
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
                return pause_for_unlock(UnlockPauseCtx {
                    emitter,
                    items: pending_unlocks,
                    repos: &repos,
                    gait_dir: &gait_dir,
                    session_id: &session_id,
                    codebase_path: &codebase_path,
                    system_prompt: &system_prompt,
                    hooks: &hooks,
                    step_count: step_index + 1,
                    skip_commit: relay.is_some(),
                });
            }
        }

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
            let tool = tools
                .iter()
                .find(|t| t.name() == tc.tool_name)
                .map(|t| t.as_ref());
            match gate_classify(tc, tool, &effective_allow, &deny, auto) {
                GateDecision::Deny => {
                    denied_ids.insert(tc.tool_call_id.clone());
                }
                GateDecision::Allow { approval_prefix } => {
                    stream_preview_if_always(preview_policy, tool, &tc.args, emitter);
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
            let perm = tools
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
                    .find_map(|req| find_matched_deny_pattern(&tc.tool_name, req, &deny))
                    .unwrap_or_else(|| tc.tool_name.clone())
            } else {
                tc.tool_name.clone()
            };
            let output = format!("[Blocked by config: {}] permission denied", pattern);
            emitter.record(events::tool_call(
                tc.tool_call_id.clone(),
                tc.tool_name.clone(),
                &tc.args,
            ))?;
            emitter.record(events::tool_result(
                tc.tool_call_id.clone(),
                output.clone(),
                None,
                None,
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
                tools
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
                    session_id: session_id.clone(),
                    gait_dir: gait_dir.clone(),
                    codebase: codebase_path.clone(),
                    system_prompt: system_prompt.clone(),
                    step_index,
                    tool_name: tc.tool_name.clone(),
                    tool_call_id: tc.tool_call_id.clone(),
                    tool_args: tc.args.clone(),
                    ..Default::default()
                };
                let block_reason = hooks.iter().find_map(|h| {
                    if let HookAction::Block { reason } = h.pre_tool(&pre_tool_ctx, emitter) {
                        Some(reason)
                    } else {
                        None
                    }
                });
                let prefix = tool_approval_prefixes.get(&tc.tool_call_id).cloned();
                let tc = *tc;
                let tools = &tools;
                let codebase_path = codebase_path.clone();
                let gait_dir = gait_dir.clone();
                let session_id = session_id.clone();
                let relay_clone = relay.clone();
                async move {
                    if let Some(reason) = block_reason {
                        Err(anyhow::anyhow!("[Blocked by hook] {}", reason))
                    } else {
                        execute_tool_call(
                            tc,
                            tools,
                            emitter,
                            prefix,
                            &codebase_path,
                            &gait_dir,
                            &session_id,
                            sandbox_on,
                            relay_clone.as_ref(),
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
                        session_id: session_id.clone(),
                        gait_dir: gait_dir.clone(),
                        codebase: codebase_path.clone(),
                        system_prompt: system_prompt.clone(),
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
                        &hooks,
                        emitter,
                        &post_tool_ctx,
                        &model,
                        provider.clone(),
                    )
                    .await;
                }
                // Task 6: if the hand signalled a seal denial and we're in supervised
                // mode, pause immediately so the user can grant the unlock.
                // In auto mode the tool failure result propagates normally.
                if !auto && sandbox_on {
                    if let Some(denial) = exec.denied.take() {
                        return pause_for_unlock(UnlockPauseCtx {
                            emitter,
                            items: vec![denial],
                            repos: &repos,
                            gait_dir: &gait_dir,
                            session_id: &session_id,
                            codebase_path: &codebase_path,
                            system_prompt: &system_prompt,
                            hooks: &hooks,
                            step_count: step_index + 1,
                            skip_commit: relay.is_some(),
                        });
                    }
                }
                all_results[*i] = Some((exec.part, exec.result));
            }

            for (i, tc) in &serial_items {
                // pre_tool hook — first block wins
                let serial_pre_tool_ctx = HookContext {
                    event: "pre_tool".to_string(),
                    session_id: session_id.clone(),
                    gait_dir: gait_dir.clone(),
                    codebase: codebase_path.clone(),
                    system_prompt: system_prompt.clone(),
                    step_index,
                    tool_name: tc.tool_name.clone(),
                    tool_call_id: tc.tool_call_id.clone(),
                    tool_args: tc.args.clone(),
                    ..Default::default()
                };
                if let Some(reason) = hooks.iter().find_map(|h| {
                    if let HookAction::Block { reason } = h.pre_tool(&serial_pre_tool_ctx, emitter)
                    {
                        Some(reason)
                    } else {
                        None
                    }
                }) {
                    let output = format!("[Blocked by hook] {}", reason);
                    emitter.record(events::tool_call(
                        tc.tool_call_id.clone(),
                        tc.tool_name.clone(),
                        &tc.args,
                    ))?;
                    emitter.record(events::tool_result(
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

                let signal = tools
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
                            if on {
                                // MCP: push all tools from this server (server-granularity)
                                for def in
                                    mcp_registry.iter().filter(|d| d.server_name == item_name)
                                {
                                    if let Some(client) = mcp_clients.get(&def.server_name) {
                                        if !tools.iter().any(|t| t.name() == def.name) {
                                            // duplicate guard
                                            tools.push(prompt::make_mcp_tool_def(
                                                def,
                                                client.clone(),
                                            ));
                                        }
                                    }
                                }
                                // Skill: no tools push needed — content is returned by LoadTool.execute()
                            } else {
                                // Remove all tools from this server
                                tools.retain(|t| {
                                    mcp_registry
                                        .iter()
                                        .find(|d| d.name == t.name())
                                        .map(|d| d.server_name != item_name)
                                        .unwrap_or(true)
                                });
                            }
                            let toggle_kind =
                                if mcp_registry.iter().any(|d| d.server_name == item_name) {
                                    "mcp"
                                } else if plugin_registry.iter().any(|d| d.name == item_name) {
                                    "plugin"
                                } else {
                                    "skill"
                                };
                            emitter.record(events::toggle(toggle_kind, item_name, on))?;
                        }
                        let prefix = tool_approval_prefixes.get(&tc.tool_call_id).cloned();
                        let mut exec = execute_tool_call(
                            tc,
                            &tools,
                            emitter,
                            prefix,
                            &codebase_path,
                            &gait_dir,
                            &session_id,
                            sandbox_on,
                            relay.as_ref(),
                        )
                        .await
                        .unwrap_or_else(|e| error_tool_result(tc, e));
                        {
                            let raw_result = exec.result.result.as_str().unwrap_or("").to_string();
                            let post_tool_ctx = HookContext {
                                event: "post_tool".to_string(),
                                session_id: session_id.clone(),
                                gait_dir: gait_dir.clone(),
                                codebase: codebase_path.clone(),
                                system_prompt: system_prompt.clone(),
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
                                &hooks,
                                emitter,
                                &post_tool_ctx,
                                &model,
                                provider.clone(),
                            )
                            .await;
                        }
                        // Task 6: seal denial in supervised mode → pause for unlock.
                        if !auto && sandbox_on {
                            if let Some(denial) = exec.denied.take() {
                                return pause_for_unlock(UnlockPauseCtx {
                                    emitter,
                                    items: vec![denial],
                                    repos: &repos,
                                    gait_dir: &gait_dir,
                                    session_id: &session_id,
                                    codebase_path: &codebase_path,
                                    system_prompt: &system_prompt,
                                    hooks: &hooks,
                                    step_count: step_index + 1,
                                    skip_commit: relay.is_some(),
                                });
                            }
                        }
                        all_results[*i] = Some((exec.part, exec.result));
                    }
                    ExecutionSignal::TurnComplete => {
                        let message = tc
                            .args
                            .get("message")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let result = tc.args.get("result").cloned();

                        // Validate result against schema if configured.
                        if let Some(ref schema) = result_schema {
                            let value_to_validate =
                                result.clone().unwrap_or(serde_json::Value::Null);
                            if let Err(errors) = validate_against_schema(schema, &value_to_validate)
                            {
                                if schema_retries_remaining > 0 {
                                    schema_retries_remaining -= 1;
                                    emitter.record(events::tool_call(
                                        tc.tool_call_id.clone(),
                                        tc.tool_name.clone(),
                                        &tc.args,
                                    ))?;
                                    let error_msg = format!(
                                        "result schema validation failed ({} retries remaining):\n{}",
                                        schema_retries_remaining, errors
                                    );
                                    emitter.record(events::tool_result(
                                        tc.tool_call_id.clone(),
                                        error_msg,
                                        None,
                                        None,
                                    ))?;
                                    continue;
                                } else {
                                    return Err(anyhow::anyhow!(
                                        "result schema validation failed after all retries:\n{}",
                                        errors
                                    ));
                                }
                            }
                        }

                        // Emit call+result pair for transcript auditability before ending the turn.
                        emitter.record(events::tool_call(
                            tc.tool_call_id.clone(),
                            tc.tool_name.clone(),
                            &tc.args,
                        ))?;
                        emitter.record(events::tool_result(
                            tc.tool_call_id.clone(),
                            "recorded".to_string(),
                            None,
                            None,
                        ))?;
                        for t in post_tool_text.drain(..) {
                            emitter.publish_delta(t);
                        }
                        let events_now = ingestor.events()?;
                        let turn_n = count_turn_complete(&events_now) as u32 + 1;
                        let code_checkpoint = checkpoint_codebase(
                            repos.checkpoints.as_ref(),
                            &gait_dir,
                            &session_id,
                            &codebase_path,
                            turn_n,
                        );
                        let changes = code_checkpoint.as_ref().and_then(|sha| {
                            repos
                                .checkpoints
                                .diff(&gait_dir, &session_id, sha)
                                .ok()
                                .flatten()
                        });
                        let usage = TurnTokenUsage {
                            input: total_usage.input_tokens as u64,
                            output: total_usage.output_tokens as u64,
                            cache_read: total_usage.cache_read_tokens as u64,
                            cache_write: total_usage.cache_write_tokens as u64,
                        };
                        emitter.record(events::turn_complete(events::TurnCompleteParams {
                            usage,
                            codebase_hash: codebase_hash.clone(),
                            codebase_dirty,
                            checkpoint: code_checkpoint,
                            blocks: split_into_blocks(&all_turn_text),
                            message: message.clone(),
                            result: result.clone(),
                            changes: changes.clone(),
                        }))?;
                        if relay.is_none() {
                            repos.traces.commit_session(
                                &gait_dir,
                                &session_id,
                                &format!("session({}): turn {}", session_id, turn_n),
                            )?;
                        }
                        {
                            let post_turn_ctx = HookContext {
                                event: "post_turn".to_string(),
                                session_id: session_id.clone(),
                                gait_dir: gait_dir.clone(),
                                codebase: codebase_path.clone(),
                                system_prompt: system_prompt.clone(),
                                step_count: step_index + 1,
                                turn_outcome: "done".to_string(),
                                final_response: message.clone(),
                                ..Default::default()
                            };
                            for h in &hooks {
                                h.post_turn(&post_turn_ctx, emitter);
                            }
                        }
                        return Ok(TurnResult::Done {
                            message,
                            result,
                            changes,
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
                        emitter.record(events::clarify(&items))?;
                        clarify_signal = Some(items);
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
                        emitter.record(events::inform(&items))?;
                        inform_signal = Some(items);
                    }
                    ExecutionSignal::Continue => {
                        // skill (default) and any other serial tool
                        let prefix = tool_approval_prefixes.get(&tc.tool_call_id).cloned();
                        let mut exec = execute_tool_call(
                            tc,
                            &tools,
                            emitter,
                            prefix,
                            &codebase_path,
                            &gait_dir,
                            &session_id,
                            sandbox_on,
                            relay.as_ref(),
                        )
                        .await
                        .unwrap_or_else(|e| error_tool_result(tc, e));
                        {
                            let raw_result = exec.result.result.as_str().unwrap_or("").to_string();
                            let post_tool_ctx = HookContext {
                                event: "post_tool".to_string(),
                                session_id: session_id.clone(),
                                gait_dir: gait_dir.clone(),
                                codebase: codebase_path.clone(),
                                system_prompt: system_prompt.clone(),
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
                                &hooks,
                                emitter,
                                &post_tool_ctx,
                                &model,
                                provider.clone(),
                            )
                            .await;
                        }
                        // Task 6: seal denial in supervised mode → pause for unlock.
                        if !auto && sandbox_on {
                            if let Some(denial) = exec.denied.take() {
                                return pause_for_unlock(UnlockPauseCtx {
                                    emitter,
                                    items: vec![denial],
                                    repos: &repos,
                                    gait_dir: &gait_dir,
                                    session_id: &session_id,
                                    codebase_path: &codebase_path,
                                    system_prompt: &system_prompt,
                                    hooks: &hooks,
                                    step_count: step_index + 1,
                                    skip_commit: relay.is_some(),
                                });
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
            emitter.publish_delta(t);
        }

        // Gated tools: emit Permissions and exit so the user can decide.
        // (In auto mode gated_ids is always empty — this block is never reached.)
        if !pending_permissions.is_empty() {
            emitter.record(events::permissions(&pending_permissions))?;
            if relay.is_none() {
                repos.traces.commit_session(
                    &gait_dir,
                    &session_id,
                    &format!("session({}): permissions", session_id),
                )?;
            }
            {
                let post_turn_ctx = HookContext {
                    event: "post_turn".to_string(),
                    session_id: session_id.clone(),
                    gait_dir: gait_dir.clone(),
                    codebase: codebase_path.clone(),
                    system_prompt: system_prompt.clone(),
                    step_count: final_step_index + 1,
                    turn_outcome: "permissions".to_string(),
                    ..Default::default()
                };
                for h in &hooks {
                    h.post_turn(&post_turn_ctx, emitter);
                }
            }
            return Ok(TurnResult::Permissions);
        }

        // Exit turn for clarify/inform — event already in transcript+stdout via emit_event
        if let Some(items) = clarify_signal.take() {
            if relay.is_none() {
                repos.traces.commit_session(
                    &gait_dir,
                    &session_id,
                    &format!("session({}): clarify", session_id),
                )?;
            }
            {
                let post_turn_ctx = HookContext {
                    event: "post_turn".to_string(),
                    session_id: session_id.clone(),
                    gait_dir: gait_dir.clone(),
                    codebase: codebase_path.clone(),
                    system_prompt: system_prompt.clone(),
                    step_count: final_step_index + 1,
                    turn_outcome: "clarify".to_string(),
                    ..Default::default()
                };
                for h in &hooks {
                    h.post_turn(&post_turn_ctx, emitter);
                }
            }
            return Ok(TurnResult::Clarify { items });
        }
        if let Some(items) = inform_signal.take() {
            if relay.is_none() {
                repos.traces.commit_session(
                    &gait_dir,
                    &session_id,
                    &format!("session({}): inform", session_id),
                )?;
            }
            {
                let post_turn_ctx = HookContext {
                    event: "post_turn".to_string(),
                    session_id: session_id.clone(),
                    gait_dir: gait_dir.clone(),
                    codebase: codebase_path.clone(),
                    system_prompt: system_prompt.clone(),
                    step_count: final_step_index + 1,
                    turn_outcome: "inform".to_string(),
                    ..Default::default()
                };
                for h in &hooks {
                    h.post_turn(&post_turn_ctx, emitter);
                }
            }
            return Ok(TurnResult::Inform { items });
        }

        // Append this step to messages for next iteration
        messages.push(Message::Assistant {
            content: AssistantContent::Parts(assistant_parts),
            id: String::new(),
        });
        messages.push(Message::ToolResult {
            content: tool_results,
            id: String::new(),
        });
    }

    anyhow::bail!(
        "reached max steps ({}) without turn_complete",
        max_iter.unwrap_or(100)
    )
}

/// Check whether a single tool call targets any path outside its sandbox ceiling.
///
/// Returns a list of `UnlockItem`s — one per out-of-scope target. Returns an
/// empty vec when all targets are within scope (or the tool exposes no resolvable
/// targets via `scope_targets`).
///
/// Uses `SandboxConfig::default()` for the ceiling (no extra config-level
/// widening beyond codebase + temp_jail). The `temp_jail` path is `/tmp/gait-jail`
/// as a placeholder — for the pre-check we only care whether the target is under
/// the codebase, which `compute_scope` delivers correctly for default config.
fn check_own_tool_scope(
    tc: &ToolCall,
    tool: &dyn Tool,
    codebase: &std::path::Path,
) -> Vec<pb::UnlockItem> {
    use crate::sandbox::ceiling::compute_scope;
    use crate::types::Dimension;

    let targets = tool.scope_targets(&tc.args);
    if targets.is_empty() {
        return vec![];
    }

    let temp_jail = std::path::Path::new("/tmp/gait-jail");
    let cfg = crate::config::SandboxConfig::default();
    let ceiling = compute_scope(tool.access(), &cfg, codebase, temp_jail);

    let mut items = vec![];
    for (path, dim) in targets {
        // Canonicalize the target so the starts_with comparison works correctly
        // on macOS where /tmp → /private/tmp. compute_scope already canonicalizes
        // the codebase path, so a non-canonical target path will spuriously fail
        // starts_with and trigger a false-positive TurnResult::Unlock.
        let target = if path.exists() {
            path.canonicalize().unwrap_or_else(|_| path.clone())
        } else {
            // For not-yet-existing targets, canonicalize the parent directory.
            path.parent()
                .and_then(|p| p.canonicalize().ok())
                .map(|p| p.join(path.file_name().unwrap_or_default()))
                .unwrap_or_else(|| path.clone())
        };
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
            items.push(pb::UnlockItem {
                id: uuid::Uuid::new_v4().to_string(),
                tool_name: tc.tool_name.clone(),
                resource: path.to_string_lossy().into_owned(),
                dimension: dim.as_str().to_string(),
                preview: None,
            });
        }
    }
    items
}

/// Context for `pause_for_unlock` to reduce argument count.
pub(crate) struct UnlockPauseCtx<'a> {
    emitter: &'a Recorder,
    items: Vec<pb::UnlockItem>,
    repos: &'a crate::git::Repos,
    gait_dir: &'a std::path::Path,
    session_id: &'a str,
    codebase_path: &'a std::path::Path,
    system_prompt: &'a str,
    hooks: &'a [Box<dyn crate::types::Hook>],
    step_count: usize,
    /// When Some, the brain is running under a spine — skip commit_session
    /// (the spine owns all gait-dir writes).
    skip_commit: bool,
}

/// Emit the sandbox-unlock pause and return `TurnResult::Unlock`.
///
/// Mirrors the Permissions pause block in `run_turn` (near "Gated tools"):
/// records an `unlock` event, commits the session worktree, fires the
/// `post_turn` hook with `turn_outcome: "unlock"`, then returns
/// `TurnResult::Unlock { items }`.
///
/// Called by Tasks 5 (pre-check path) and 6 (EACCES path). Not called
/// in auto-mode (caller must gate on `sandbox_on && !auto`).
pub(crate) fn pause_for_unlock(ctx: UnlockPauseCtx) -> anyhow::Result<TurnResult> {
    ctx.emitter.record(events::unlock(&ctx.items))?;
    if !ctx.skip_commit {
        ctx.repos.traces.commit_session(
            ctx.gait_dir,
            ctx.session_id,
            &format!("session({}): unlock", ctx.session_id),
        )?;
    }
    {
        let post_turn_ctx = HookContext {
            event: "post_turn".to_string(),
            session_id: ctx.session_id.to_string(),
            gait_dir: ctx.gait_dir.to_path_buf(),
            codebase: ctx.codebase_path.to_path_buf(),
            system_prompt: ctx.system_prompt.to_string(),
            step_count: ctx.step_count,
            turn_outcome: "unlock".to_string(),
            ..Default::default()
        };
        for h in ctx.hooks {
            h.post_turn(&post_turn_ctx, ctx.emitter);
        }
    }
    Ok(TurnResult::Unlock { items: ctx.items })
}

#[cfg(test)]
mod tests;

//! Turn setup — Stage 4 / Task 15, plus the post-resume half folded in by
//! Task 19 (driver slimming).
//!
//! `init()` owns everything `run_turn` needs before the permission-resume
//! check: the destructured `ResolvedProfile`, the session-lock/worktree/
//! SessionStart bootstrap, effective-allow computation, and the pre-resume
//! event snapshot (`pre_events`/`current_turn`).
//!
//! `finish_after_resume()` owns the loadables/`ActiveAgent` projection, the
//! dynamic system-prompt build, the Agent drift-guard, and the pre_turn hook
//! loop — everything that runs *after* `resume::apply_if_pending` but
//! *before* the step loop. It genuinely can't live in `init()`: the
//! projection reads the log, which resume may have just written
//! approved/denied tool events into, so it must run after resume, not before.
//!
//! `SetupData` owns everything; `init` acquires the session lock. Building
//! `TurnEnv`/`Persistence` from a `&SetupData` happens in the caller —
//! `Persistence` can't be a `SetupData` field itself (it borrows from the
//! very struct that would contain it), so `init` builds one internally, only
//! to acquire the workspace lock, then discards it.

use std::path::{Path, PathBuf};

use crate::git::Repos;
use crate::proto::events;
use crate::proto::gait::log::v1 as pb;
use crate::proto::gait::log::v1::{payload, Event};
use crate::proto::Recorder;
use crate::session::log::{init_session, InitSessionOpts};
use crate::session::retrace::{self, AgentState};
use crate::session::{Ingestor, Session};
use crate::types::{Hook, HookContext, LoadableEntry, LoadableKind};
use gait_provider::provider::Provider;

use super::persist::Persistence;
use super::state::TurnState;
use super::{event_kind, TurnInput};
use crate::session::SessionLock;

/// Everything `run_turn`'s pre-resume setup produces, owned by the caller.
pub(crate) struct SetupData {
    pub state: TurnState,
    pub agent: crate::proto::gait::log::v1::Agent,
    pub provider: std::sync::Arc<dyn Provider>,
    pub session_id: String,
    pub gait_dir: PathBuf,
    pub codebase_path: PathBuf,
    pub model: String,
    pub repos: Repos,
    pub system_prompt: String,
    pub mcp_registry: Vec<crate::config::McpToolDef>,
    pub mcp_clients: crate::profile::McpClients,
    pub skill_registry: Vec<crate::config::SkillDef>,
    pub plugin_registry: Vec<crate::config::WasmPluginDef>,
    pub hooks: Vec<Box<dyn Hook>>,
    pub result_schema: Option<serde_json::Value>,
    pub llm_params: crate::config::LlmParamsConfig,
    pub context_policy: crate::profile::ContextPolicy,
    pub agent_seed: AgentState,
    pub auto: bool,
    pub max_iter: Option<usize>,
    pub preview_policy: crate::config::PreviewPolicy,
    pub cancel: Option<tokio_util::sync::CancellationToken>,
    pub sandbox_on: bool,
    pub sandbox_cfg: crate::config::SandboxConfig,
    pub relay: Option<crate::turn::nerve::NerveWriter>,
    pub lock: Option<SessionLock>,
    pub ingestor: Ingestor,
    pub codebase_hash: String,
    pub dirty_files: Vec<String>,
    pub codebase_dirty: bool,
    pub effective_allow: Vec<String>,
    pub deny: Vec<String>,
    pub pre_events: Vec<Event>,
    pub current_run: u32,
    pub current_turn: u32,
    pub daemons: Vec<(String, i32, bool, String)>,
    pub initial_git: Option<super::GitSnapshot>,
    /// Phase 3 (scoped registry): survives `init()` -> `finish_after_resume()`
    /// (unlike `config_enabled`, which stops at `init()`) so the catalog gate
    /// can resolve the full agent ∪ session visible set. See `crate::registry`.
    pub agent_whitelist: crate::registry::AgentWhitelist,
}

/// Runs the pre-resume setup: destructures `caps`, acquires the worktree +
/// checkpoint-init + session lock (via a throwaway `Persistence` — the real
/// one the caller uses is rebuilt from the returned fields), writes
/// `SessionStart` if this is the first turn, and computes `effective_allow`
/// plus the pre-resume event snapshot.
pub(crate) fn init(
    session: &Session,
    emitter: &Recorder,
    caps: crate::profile::ResolvedProfile,
    input: TurnInput,
) -> anyhow::Result<SetupData> {
    // Build the live Agent config from the whole profile BEFORE `caps` is
    // destructured, so `events::agent_msg` stays the single source of truth.
    // Emitted later, under the drift guard.
    let agent = super::agent_event::agent_msg(&caps);
    let mut state = TurnState::new(&caps);
    let provider = caps.provider.clone();
    let (tools, allow, deny) = caps.permissioned.into_parts();
    state.tools = tools;
    let session_id = session.id.clone();
    let gait_dir = session.gait_dir.clone();
    let codebase_path = session.codebase_path.clone();
    let model = caps.model.clone();
    let repos = session.repos.clone();
    let system_prompt = caps.instructions;
    let mcp_registry = caps.mcp_registry;
    // Under `min` (no `mcp`/`code-mode` feature), `McpClients` is `()`, so this
    // binding is a unit value on that build — still correct, just not worth a
    // conditional to silence the lint on a line that binds a real value otherwise.
    #[allow(clippy::let_unit_value)]
    let mcp_clients = caps.mcp_clients;
    let skill_registry = caps.skill_registry;
    let plugin_registry = caps.plugin_registry;
    let config_enabled = caps.config_enabled;
    let hooks = caps.hooks;
    let result_schema = caps.result_schema;
    let llm_params = caps.llm_params.clone();
    let context_policy = caps.context_policy;
    let agent_whitelist = caps.agent_whitelist;

    // Build the agent-dimension seed (M3): config-derived genesis for ActiveAgent.
    // Built manually from resolved fields rather than via `AgentState::from_config`
    // (retrace.rs) for two reasons:
    //   1. We don't have a `&GaitConfig` in scope here — only the destructured
    //      `ResolvedProfile` — and plumbing the config through `run_turn` is a
    //      larger architectural change than this seed warrants.
    //   2. The seed semantics here are deliberately different from `from_config`:
    //      this path unions `config_enabled` (= enabled mcp_servers ∪ enabled
    //      skills, see profile.rs:114-118) into BOTH `skills` and `mcp` to
    //      reproduce the legacy `ActiveLoadables` flat-seed shape exactly.
    //      `from_config` keeps those two sets separate, so swapping it in would
    //      change the fold result. Track the migration in tech-debt #17.
    //
    //   Phase 3 (scoped registry): `skills` used to unconditionally include
    //   every registered skill name regardless of its own disk `enabled` bit
    //   -- a pre-existing bug distinct from MCP (which starts empty and only
    //   gets `config_enabled`-listed servers below, so it never had this
    //   problem). Fixed here: seed only names that are BOTH in
    //   `agent_whitelist` (or everything, if it contains `"*"`) AND have
    //   their own disk `enabled: true`. The `config_enabled` union below
    //   re-derives the same enabled-skill names via a different (legacy,
    //   kind-blind) path, so it must apply the same whitelist_allows check
    //   to `s.skills` or it silently re-admits everything the filter above
    //   just excluded -- MCP's insertion into `s.mcp` stays unconditional,
    //   since MCP has no whitelist gate at seed time by design. Session-scope
    //   whitelist entries do NOT affect this seed -- they can't,
    //   structurally (no `agent_state` exists yet at this pre-fold point) --
    //   that's fine, a session-scope registration extends what's *visible*
    //   (the catalog gate, below in `finish_after_resume`), not what
    //   auto-loads before the session has even started.
    let agent_seed = {
        let mut s = AgentState {
            model: model.clone(),
            instructions: system_prompt.clone(),
            tools: Default::default(),
            skills: skill_registry
                .iter()
                .filter(|sk| {
                    sk.enabled
                        && sk.model_invocable
                        && crate::registry::whitelist_allows(&agent_whitelist.skills, &sk.name)
                })
                .map(|sk| sk.name.clone())
                .collect(),
            mcp: Default::default(),
            plugins: Default::default(),
            allow: allow.iter().cloned().collect(),
            deny: deny.iter().cloned().collect(),
            inference: None,
            session_skills: Default::default(),
            session_mcp: Default::default(),
            session_plugins: Default::default(),
            accepted_senders: Default::default(),
        };
        for n in &config_enabled {
            let model_invocable = skill_registry
                .iter()
                .find(|skill| skill.name == *n)
                .map(|skill| skill.model_invocable)
                .unwrap_or(true);
            if model_invocable && crate::registry::whitelist_allows(&agent_whitelist.skills, n) {
                s.skills.insert(n.clone());
            }
            s.mcp.insert(n.clone());
        }
        s
    };
    let auto = input.auto;
    let max_iter = input.max_iter;
    let preview_policy = input.preview_policy;
    let cancel = input.cancel;
    let sandbox_on = input.sandbox_on;
    let sandbox_cfg = input.sandbox_cfg;
    let relay = input.relay;

    // When running under a spine (relay.is_some()), the spine performs all
    // startup disk writes before spawning the brain. The brain's seal has
    // writable: [] so it cannot write to gait-dir. `Persistence` skips these
    // writes to avoid EPERM — the spine already did (or will do) them.
    //
    // This `persist` is throwaway: only used to acquire the lock. The real
    // one the rest of `run_turn` uses is rebuilt from `SetupData`'s owned
    // fields (repos/gait_dir/session_id/codebase_path/relay), since
    // `Persistence` borrows from them and can't itself be a `SetupData` field.
    let init_persist =
        Persistence::from_relay(&relay, &repos, &gait_dir, &session_id, &codebase_path);
    let lock = init_persist.init_workspace(repos.checkpoints.as_ref())?;

    let tx_path = crate::session::live::log_path(&gait_dir, &session_id);
    let ingestor = match emitter.log_mirror() {
        Some(mirror) => Ingestor::from_shared(mirror),
        None => Ingestor::from_session(&gait_dir, &session_id),
    };

    let (codebase_hash, dirty_files) = match &input.initial_git {
        Some(g) => (g.head_hash.clone(), g.dirty_files.clone()),
        None => (
            repos.codebase.head_hash(&codebase_path).unwrap_or_default(),
            repos.codebase.dirty_files(&codebase_path),
        ),
    };
    let codebase_dirty = !dirty_files.is_empty();

    // Init transcript (SessionStart) once per session. Gate on the SessionStart
    // event's presence rather than log-emptiness: under Phase 4 Option A the input
    // adapter appends this turn's user_turn BEFORE run_turn, so the log is no longer
    // empty on the first turn.
    // When running under a spine, the spine writes SessionStart — skip to avoid
    // disk writes (brain has writable: []).
    if init_persist.is_unsealed() {
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
                assigned_by: None,
                codebase_path: Some(codebase_path.to_string_lossy().to_string()),
                notify_done: false,
            })?;
        }
    }

    // Effective allow = config baseline (global+agent, in `allow`) + log-derived
    // session/once grants, folded mutation-aware from the full session via the
    // ActivePermissions projection. Replaces the once-approval temp file entirely:
    // once/session approvals now live in the Log as PermissionsReply events.
    let (effective_allow, deny): (Vec<String>, Vec<String>) = {
        let evs = ingestor.events()?;
        let mut muts = retrace::active_mutations(&evs);
        retrace::apply_cascade(&mut muts, &evs);
        let seed: std::collections::HashSet<String> = allow.iter().cloned().collect();
        let ea = crate::permissions::grants::effective_allow(seed, &evs, &muts);
        // Session-scoped Rejects (mirrors session-scoped Allows above): union
        // them into `deny` so a repeat of an identical, already-rejected
        // request short-circuits at the gate's deny tier instead of asking
        // again — see AllowState::deny_session's doc comment.
        let deny_session = crate::permissions::grants::effective_deny_session(&evs, &muts);
        let deny: Vec<String> = deny.into_iter().chain(deny_session).collect();
        let mode = retrace::active_mode(&evs, &muts);
        crate::permissions::grants::overlay_mode_permissions(
            ea,
            deny,
            mode.as_ref().map(|m| &m.deny),
        )
    };

    // Detect resume after permission approval: last two transcript events are
    // Permissions → PermissionsReply — the caller (run_turn) checks this via
    // resume::apply_if_pending immediately after setup::init returns.
    let pre_events = ingestor.events()?;
    // The spine writes TurnStart before launching the sealed brain. Its scoped
    // coordinate is therefore the authoritative position for every step event.
    // Also recover the triggering message's attested agent id (if any) from
    // `trigger_event_id` -- see the `set_brain_identity` call below.
    let (current_run, current_turn, trigger_agent_id) = pre_events
        .iter()
        .rev()
        .find_map(|event| match crate::proto::event_kind(event) {
            Some(payload::Kind::TurnStart(ts)) => {
                let agent_id = ts.trigger_event_id.as_deref().and_then(|trigger_id| {
                    pre_events
                        .iter()
                        .find(|e| e.id == trigger_id)
                        .and_then(|e| e.provenance.first())
                        .and_then(|p| p.attested_author_agent_id.clone())
                });
                Some((event.run?, event.turn?, agent_id))
            }
            _ => None,
        })
        .unwrap_or((0, 0, None));
    emitter.set_scope(crate::proto::envelope::EventScope::Step {
        run: current_run,
        turn: current_turn,
        step: 0,
    });
    // Thread the same turn number + model that `log.jsonl` stamps into the
    // brain's always-on diagnostic ping (no-op outside the brain process, where
    // the identity cell is never installed). Without this the per-phase
    // heartbeat frames carry `turn:0` / `model:""`. `trigger_agent_id` is
    // `Some` only when this turn was driven by a verified multimatrix
    // delivery -- see `an internal design note`.
    crate::diagnostic::set_brain_identity(current_turn, &model, trigger_agent_id);

    Ok(SetupData {
        state,
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
        lock,
        ingestor,
        codebase_hash,
        dirty_files,
        codebase_dirty,
        effective_allow,
        deny,
        pre_events,
        current_run,
        current_turn,
        daemons: input.daemons,
        initial_git: input.initial_git,
        agent_whitelist,
    })
}

/// Everything after `resume::apply_if_pending` and before the step loop:
/// the loadables/`ActiveAgent` projection (mutates `state.tools`/`state.entries`),
/// the dynamic system-prompt build, the Agent drift-guard (detects whether
/// the effective prompt or a pre_turn hook changed the checkpointed Agent and
/// emits a Replace if so), and the pre_turn hook loop itself. Returns the
/// final system_prompt for this turn (after any hook modifications).
#[allow(clippy::too_many_arguments)]
pub(crate) fn finish_after_resume(
    ingestor: &Ingestor,
    emitter: &Recorder,
    agent: &pb::Agent,
    agent_seed: &AgentState,
    mcp_registry: &[crate::config::McpToolDef],
    mcp_clients: &crate::profile::McpClients,
    skill_registry: &[crate::config::SkillDef],
    plugin_registry: &[crate::config::WasmPluginDef],
    effective_allow: &[String],
    deny: &[String],
    hooks: &[Box<dyn Hook>],
    session_id: &str,
    gait_dir: &Path,
    relay: Option<crate::turn::nerve::NerveWriter>,
    codebase_path: &Path,
    system_prompt: String,
    state: &mut TurnState,
    agent_whitelist: &crate::registry::AgentWhitelist,
) -> anyhow::Result<String> {
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
    // Skills are not pushed here — they surface via LoadTool.execute() return value.
    // Phase 8: tool-gating — if the agent declared an explicit tools enable-set,
    // filter the tool list to only those tools. Empty = no restriction (all pass through).
    if !agent_state.tools.is_empty() {
        state.tools.retain(|t| agent_state.tools.contains(t.name()));
    }

    // Code Mode uses this turn-scoped dispatcher for capability calls. It is
    // deliberately kept below the transcript boundary: invoke() does not
    // emit ToolCall/ToolResult events. The native map is a one-shot snapshot
    // of `state.tools` taken here, after the tool-gating retain() above and
    // before any in-loop mutation — `state.tools` is never mutated again
    // after this point in the turn (LoadContext toggles MCP servers via
    // `capability_dispatcher.set_enabled`, not by touching `state.tools`).
    // If that invariant ever changes, this snapshot must be rebuilt to match.
    {
        use std::sync::Arc;
        #[cfg(feature = "mcp")]
        let clients: std::collections::HashMap<
            String,
            Arc<dyn super::capabilities::McpInvoker>,
        > = mcp_clients
            .iter()
            .map(|(name, client)| {
                (
                    name.clone(),
                    client.clone() as Arc<dyn super::capabilities::McpInvoker>,
                )
            })
            .collect();
        // No live MCP clients to wrap under `min` — `mcp_clients` is `()`.
        #[cfg(not(feature = "mcp"))]
        let clients: std::collections::HashMap<
            String,
            Arc<dyn super::capabilities::McpInvoker>,
        > = {
            let _ = mcp_clients;
            std::collections::HashMap::new()
        };
        let dispatcher = Arc::new(super::capabilities::CapabilityDispatcher::new(
            state
                .tools
                .iter()
                .filter(|t| !super::capabilities::is_reserved_native(t.name()))
                .cloned(),
            mcp_registry.to_vec(),
            clients,
            enabled_names.iter().cloned(),
            relay.clone(),
            deny.to_vec(),
        ));
        let erased: Arc<dyn std::any::Any + Send + Sync> = dispatcher.clone();
        for tool in &state.tools {
            tool.set_capability_dispatcher(erased.clone());
        }
        state.capability_dispatcher = dispatcher;
    }

    // Warn about allow-list entries that don't match any builtin tool name.
    // Catches stale names (e.g. "mcp", "skill") after tool renames.
    {
        let builtin_names: std::collections::HashSet<&str> =
            crate::tools::builtin_tool_names().iter().copied().collect();
        for entry in effective_allow {
            if !entry.contains(':') && !builtin_names.contains(entry.as_str()) {
                #[allow(clippy::print_stderr)]
                // tracked debt: sealed-brain stderr bypasses the Nerve/event channel — see docs/superpowers/plans/2026-07-05-repl-stderr-race-fix.md
                {
                    eprintln!("[gait] warning: allow entry {:?} doesn't match any builtin tool — will never auto-approve", entry);
                }
            }
        }
    }

    // Phase 3 (scoped registry) catalog gate: only entries within the
    // resolved visible set (agent_whitelist ∪ session_whitelist) even appear
    // in <loadable-index> at all -- distinct from `enabled_names` above,
    // which only controls the `enabled: bool` flag on an entry that's already
    // decided to be visible. Computed per kind since each has its own
    // whitelist axis. `agent_state.session_*()` are the Phase 2 accessors
    // (only reachable here, post-fold -- not at the genesis seed above).
    // Under `min` (no `mcp` feature), `mcp_registry` is always empty (Task 6a
    // gates its only producer, `ResolvedProfile::resolve`'s MCP-discovery
    // step), and the only consumer of `visible_mcp` (the MCP-entries block
    // below) is itself gated out under `min` — so `visible_mcp` isn't
    // computed at all under `min`, not just computed over an empty registry.
    #[cfg(feature = "mcp")]
    let visible_mcp = crate::registry::resolve_visible_set(
        &mcp_registry
            .iter()
            .map(|d| d.server_name.clone())
            .collect::<Vec<_>>(),
        &agent_whitelist.mcp,
        agent_state.session_mcp(),
    );
    let visible_skills = crate::registry::resolve_visible_set(
        &skill_registry
            .iter()
            .filter(|s| s.model_invocable)
            .map(|s| s.name.clone())
            .collect::<Vec<_>>(),
        &agent_whitelist.skills,
        agent_state.session_skills(),
    );
    let visible_plugins = crate::registry::resolve_visible_set(
        &plugin_registry
            .iter()
            .map(|p| p.name.clone())
            .collect::<Vec<_>>(),
        &agent_whitelist.plugins,
        agent_state.session_plugins(),
    );

    // Step 5: Augment system prompt with unified <loadable-index>.
    // Build entries: one per whitelisted MCP server, one per whitelisted skill.
    // MCP server entries — one per server (visible-set gated), gated on the
    // `mcp` feature (mirrors Task 6a's per-field `ResolvedProfile` gating):
    // under `min`, `mcp_registry` is always empty anyway (Task 6a gates its
    // only producer), so this whole block would push nothing at runtime —
    // gating it here means no MCP-entry code exists in a `min` binary at all,
    // not just that it happens to run over an empty registry. Description
    // comes from McpServerDef.description (config layer); fall back to tool
    // count. Note: we don't have direct access to McpServerDef in run_turn,
    // so we build the description from the discovered tool count when
    // there's no explicit description available. Run-level only.
    #[cfg(feature = "mcp")]
    {
        // Group MCP tool defs by server name so we can show one entry per server.
        let mut mcp_tools_by_server: std::collections::HashMap<&str, Vec<&crate::config::McpToolDef>> =
            std::collections::HashMap::new();
        for def in mcp_registry {
            mcp_tools_by_server
                .entry(def.server_name.as_str())
                .or_default()
                .push(def);
        }
        for (server_name, defs) in &mcp_tools_by_server {
            if !visible_mcp.contains(*server_name) {
                continue;
            }
            let description = format!(
                "{} tool{}",
                defs.len(),
                if defs.len() == 1 { "" } else { "s" }
            );
            state.entries.push(LoadableEntry {
                name: server_name.to_string(),
                kind: LoadableKind::Mcp,
                description,
                enabled: enabled_names.contains(*server_name),
            });
        }
    }
    // Skill entries — one per skill (visible-set gated).
    for skill in skill_registry {
        if !skill.model_invocable || !visible_skills.contains(&skill.name) {
            continue;
        }
        state.entries.push(LoadableEntry {
            name: skill.name.clone(),
            kind: LoadableKind::Skill,
            description: skill.description.clone(),
            enabled: enabled_names.contains(&skill.name),
        });
    }
    // Plugin entries — one per wasm plugin (visible-set gated).
    for plugin in plugin_registry {
        if !visible_plugins.contains(&plugin.name) {
            continue;
        }
        let basename = {
            let p = plugin.path.as_str();
            p.rsplit('/').next().unwrap_or(p)
        };
        state.entries.push(LoadableEntry {
            name: plugin.name.clone(),
            kind: LoadableKind::WasmPlugin,
            description: format!("wasm plugin ({})", basename),
            enabled: enabled_names.contains(&plugin.name),
        });
    }
    // `mcp_registry` is naturally empty under `min` (see above), but gate
    // both arguments explicitly anyway so no MCP-index/Code-Mode-reference
    // fragment is ever a live code path in a `min` binary — only the
    // already-empty `entries`/`mcp_registry_arg` reach `build_dynamic_
    // system_prompt`, whose own arity/behavior this task leaves unchanged.
    #[cfg(feature = "mcp")]
    let enabled_mcp: std::collections::HashSet<String> = enabled_names
        .iter()
        .filter(|name| mcp_registry.iter().any(|d| &d.server_name == *name))
        .cloned()
        .collect();
    #[cfg(not(feature = "mcp"))]
    let enabled_mcp: std::collections::HashSet<String> = std::collections::HashSet::new();

    #[cfg(feature = "mcp")]
    let mcp_registry_arg: &[crate::config::McpToolDef] = mcp_registry;
    #[cfg(not(feature = "mcp"))]
    let mcp_registry_arg: &[crate::config::McpToolDef] = &[];

    let mut system_prompt = super::prompt::build_dynamic_system_prompt(
        &system_prompt,
        &state.entries,
        mcp_registry_arg,
        &enabled_mcp,
        hooks,
    );

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
                .record(crate::proto::wire::body(payload::Kind::Agent(rebaselined)))?
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
    for h in hooks {
        let ctx = HookContext {
            event: "pre_turn".to_string(),
            session_id: session_id.to_string(),
            gait_dir: gait_dir.to_path_buf(),
            codebase: codebase_path.to_path_buf(),
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
        let _ = emitter.record(events::replace(
            agent_event_id.clone(),
            pb::Payload {
                kind: Some(pb::payload::Kind::Agent(replacement_agent)),
            },
            Some("pre_turn hook modified system_prompt".into()),
        ));
    }

    Ok(system_prompt)
}

#[cfg(test)]
pub(crate) async fn build_test_turn_state(
    session: &Session,
    emitter: &Recorder,
    caps: crate::profile::ResolvedProfile,
    input: TurnInput,
    relay_arg: Option<crate::turn::nerve::NerveWriter>,
) -> anyhow::Result<TurnState> {
    let mut input = input;
    input.relay = relay_arg;
    let data = init(session, emitter, caps, input)?;
    let SetupData {
        mut state,
        agent,
        session_id,
        gait_dir,
        codebase_path,
        mcp_registry,
        mcp_clients,
        skill_registry,
        plugin_registry,
        hooks,
        agent_seed,
        effective_allow,
        deny,
        pre_events,
        sandbox_on,
        relay,
        codebase_hash,
        dirty_files,
        daemons,
        agent_whitelist,
        ingestor,
        system_prompt,
        ..
    } = data;
    super::resume::apply_if_pending(
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
    finish_after_resume(
        &ingestor,
        emitter,
        &agent,
        &agent_seed,
        &mcp_registry,
        &mcp_clients,
        &skill_registry,
        &plugin_registry,
        &effective_allow,
        &deny,
        &hooks,
        &session_id,
        &gait_dir,
        relay.clone(),
        &codebase_path,
        system_prompt,
        &mut state,
        &agent_whitelist,
    )?;
    Ok(state)
}

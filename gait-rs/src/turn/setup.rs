//! Turn setup — Stage 4 / Task 15.
//!
//! Owns everything `run_turn` needs before the permission-resume check: the
//! destructured `ResolvedProfile`, the session-lock/worktree/SessionStart
//! bootstrap, effective-allow computation, and the pre-resume event snapshot
//! (`pre_events`/`current_turn`).
//!
//! **Scope note (deviation from the plan-01 sketch, re-derived against live
//! code):** the plan's Task 15 also folds in the post-resume loadables/
//! `ActiveAgent` projection block (former `:588–681`). That block genuinely
//! depends on `resume::apply_if_pending` having already run — the projection
//! reads the log, which resume may have just written approved/denied tool
//! events into. Moving it into this same `init()` would require either
//! calling resume from inside setup.rs (wrong: resume needs an `&mut
//! TurnState` and `execute_tool_call`, layering concerns setup.rs shouldn't
//! own) or splitting `init()` around a resume call the caller doesn't control
//! (equally awkward). It stays a separate, later block in `run_turn` for now;
//! this file owns strictly the pre-resume setup.
//!
//! `SetupData` owns everything; `init` acquires the session lock. Building
//! `TurnEnv`/`Persistence` from a `&SetupData` happens in the caller (Task 19
//! will formalize this as `TurnEnv::borrow`/`Persistence::from_setup`) —
//! `Persistence` can't be a `SetupData` field itself (it borrows from the
//! very struct that would contain it), so `init` builds one internally, only
//! to acquire the workspace lock, then discards it.

use std::path::PathBuf;

use crate::git::Repos;
use crate::proto::events;
use crate::proto::gait::log::v1::{payload, Event};
use crate::proto::Recorder;
use crate::provider::Provider;
use crate::session::log::{count_turn_complete, init_session, InitSessionOpts};
use crate::session::retrace::{self, AgentState};
use crate::session::{Ingestor, Session};
use crate::types::Hook;

use super::persist::Persistence;
use super::state::TurnState;
use super::{event_kind, SessionLock, TurnInput};

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
    pub mcp_clients: std::collections::HashMap<String, std::sync::Arc<crate::mcp::McpClient>>,
    pub skill_registry: Vec<crate::config::SkillDef>,
    pub plugin_registry: Vec<crate::config::WasmPluginDef>,
    pub hooks: Vec<Box<dyn Hook>>,
    pub result_schema: Option<serde_json::Value>,
    pub llm_params: crate::config::LlmParamsConfig,
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
    pub current_turn: u32,
    pub daemons: Vec<(String, i32, bool, String)>,
    pub initial_git: Option<super::GitSnapshot>,
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
    let agent = events::agent_msg(&caps);
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
    let mcp_clients = caps.mcp_clients;
    let skill_registry = caps.skill_registry;
    let plugin_registry = caps.plugin_registry;
    let config_enabled = caps.config_enabled;
    let hooks = caps.hooks;
    let result_schema = caps.result_schema;
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
        let mut s = AgentState {
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

    let session_dir = gait_dir.join("sessions").join(&session_id);
    let tx_path = session_dir.join("log.jsonl");
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
    let effective_allow: Vec<String> = {
        let evs = ingestor.events()?;
        let mut muts = retrace::active_mutations(&evs);
        retrace::apply_cascade(&mut muts, &evs);
        let seed: std::collections::HashSet<String> = allow.iter().cloned().collect();
        crate::permissions::grants::effective_allow(seed, &evs, &muts)
    };

    // Detect resume after permission approval: last two transcript events are
    // Permissions → PermissionsReply — the caller (run_turn) checks this via
    // resume::apply_if_pending immediately after setup::init returns.
    let pre_events = ingestor.events()?;
    // Stamp emitter position so every event recorded from this point on carries
    // the correct (turn, step). The current turn is the count of already-completed
    // turns in the log; the step starts at 0 and advances inside the loop.
    let current_turn = count_turn_complete(&pre_events) as u32;
    emitter.set_position(current_turn, 0);

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
        current_turn,
        daemons: input.daemons,
        initial_git: input.initial_git,
    })
}

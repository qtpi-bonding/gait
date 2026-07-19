//! # Retrace — the `Log → working-state` fold
//!
//! Folds the immutable event `Log` into working state via [`fold`] (all projections)
//! or individual projection functions:
//!
//! - [`messages`] / [`messages_from_path`] — the **Transcript** (`Vec<Message>`).
//!   Honours `Compaction` breakpoints and `Redact`/`Replace` mutations.
//! - [`active_agent`] — folded agent-dimension state (model + enabled loadables).
//! - [`active_agent_checkpoint`] — last `Agent` payload LWW-Register (drift guard).
//! - [`active_mutations`] + [`apply_cascade`] — `Redact`/`Replace` LWW fold with
//!   ToolCall↔ToolResult cascade.

use crate::proto::gait::log::v1::{self as pb, event, payload, ClarifyKind, Event};
use crate::session::{log::read_events, SessionError};
use crate::types::{
    AssistantContent, AssistantPart, Message, MessageContent, ToolCall, ToolResult, ToolResultKind,
};
use std::collections::HashMap;
use std::path::Path;

/// Proto `Struct` tool input → `serde_json::Value` object (or Null).
fn struct_to_value(s: &Option<pbjson_types::Struct>) -> serde_json::Value {
    match s {
        Some(st) => serde_json::to_value(st).unwrap_or(serde_json::Value::Null),
        None => serde_json::Value::Null,
    }
}

fn clarify_kind_str(k: i32) -> &'static str {
    match ClarifyKind::try_from(k).unwrap_or(ClarifyKind::Unspecified) {
        ClarifyKind::Is => "is",
        ClarifyKind::Ought => "ought",
        ClarifyKind::Unspecified => "unspecified",
    }
}

/// The converged mutation state for a single target event (LWW winner).
#[derive(Debug, Clone)]
pub enum MutationState {
    Redacted {
        reason: Option<String>,
    },
    Replaced {
        payload: Box<pb::Payload>,
        reason: Option<String>,
    },
}

/// Produce the transcript (`Vec<Message>`) derived from the log and fed to the
/// LLM. Pipeline: converge Redact/Replace mutations → fold `ActiveRawMessages`
/// over the mutation-applied stream (one `Message` per event) → `coalesce()` to
/// merge consecutive tool calls/results. This is the primary transcript entry
/// point; `messages()` is kept as a backward-compatible alias.
pub fn transcript(all_events: &[Event]) -> Result<Vec<Message>, SessionError> {
    let mut muts = active_mutations(all_events);
    apply_cascade(&mut muts, all_events);
    let raw = run_projection(&ActiveRawMessages, all_events, &muts);
    Ok(coalesce(raw.base))
}

/// Backward-compatible alias for [`transcript()`].
pub fn messages(all_events: &[Event]) -> Result<Vec<Message>, SessionError> {
    transcript(all_events)
}

pub fn messages_from_path(path: &Path) -> Result<Vec<Message>, SessionError> {
    let all_events = read_events(path)?;
    transcript(&all_events)
}
/// Build the outgoing message array for one LLM step. Under Design A the
/// injected (and any other) messages already live in the log as events, so the
/// outgoing array is just `transcript()` over the full log — the call site in
/// `run_turn` is responsible for stamping the right `turn` on the emitter
/// before any step-internal events are recorded. The `_turn` argument is
/// accepted for forward-compat (e.g. a future per-turn windowing) but is
/// currently ignored.
pub fn outgoing_for_step(all_events: &[Event], _turn: u32) -> Result<Vec<Message>, SessionError> {
    let mut muts = active_mutations(all_events);
    apply_cascade(&mut muts, all_events);
    let raw = run_projection(&ActiveRawMessages, all_events, &muts);
    Ok(coalesce(raw.base))
}

/// Get the effective payload kind, applying Redact/Replace mutations.
/// Returns None if the event is Redacted.
fn effective_kind<'a>(
    e: &'a Event,
    mutations: &'a HashMap<String, MutationState>,
) -> Option<&'a payload::Kind> {
    match mutations.get(&e.id) {
        Some(MutationState::Redacted { .. }) => None,
        Some(MutationState::Replaced { payload, .. }) => payload.kind.as_ref(),
        None => crate::proto::event_kind(e),
    }
}

/// Iterate `(event, effective_kind)` over the full session, applying mutations:
/// redacted events are skipped; replaced events yield their replacement kind.
/// This is the single canonical input every `Projection` folds.
pub fn effective_kinds<'a>(
    all_events: &'a [Event],
    mutations: &'a HashMap<String, MutationState>,
) -> impl Iterator<Item = (&'a Event, &'a payload::Kind)> {
    all_events
        .iter()
        .filter_map(move |e| effective_kind(e, mutations).map(|k| (e, k)))
}

/// A derived read-model folded from the Log. Every projection folds the SAME
/// input — the mutation-applied, full-session stream produced by `effective_kinds`
/// — so two projections can never disagree about what happened. Mutations are the
/// substrate (applied before folding); compaction is NOT handled here (only the
/// `ActiveMessages` projection cares about it).
pub trait Projection {
    type State;
    /// Initial state before any events (seeded from config where relevant).
    fn seed(&self) -> Self::State;
    /// If `kind` is a checkpoint for THIS projection's dimension, return the
    /// baseline state it establishes — the fold resets to it (positionally,
    /// where the event sits in the stream; `through_seq` is NOT consulted).
    /// Default: not a checkpoint for any projection.
    fn checkpoint(&self, _kind: &payload::Kind) -> Option<Self::State> {
        None
    }
    /// Fold one event's effective payload kind into the state.
    fn apply(&self, state: &mut Self::State, event: &Event, kind: &payload::Kind);
    /// Finalize (default: identity).
    fn finish(&self, state: Self::State) -> Self::State {
        state
    }
}
/// Drive one projection over the mutation-applied full-session stream.
pub fn run_projection<P: Projection>(
    p: &P,
    all_events: &[Event],
    mutations: &HashMap<String, MutationState>,
) -> P::State {
    let mut state = p.seed();

    for (ev, kind) in effective_kinds(all_events, mutations) {
        match p.checkpoint(kind) {
            // A checkpoint resets the fold baseline positionally (C1: where the

            // event sits in the stream; `through_seq` is never consulted).
            Some(base) => state = base,

            None => p.apply(&mut state, ev, kind),
        }
    }

    p.finish(state)
}

/// The agent-dimension working state: the folded result of the latest `Agent`
/// checkpoint plus the deltas after it. `skills`/`mcp`/`tools` are ENABLED sets
/// (OR-Set semantics under the fold); `tools` is AUDIT-ONLY today (I2 — built-ins
/// are not gated by it). `allow`/`deny` are the static policy seed (runtime grants
/// live in ActivePermissions, I3).
#[derive(Debug, Clone, Default)]
pub struct AgentState {
    pub model: String,
    pub inference: Option<pb::Inference>,
    pub instructions: String,
    pub tools: std::collections::HashSet<String>,
    pub skills: std::collections::HashSet<String>,
    pub mcp: std::collections::HashSet<String>,
    pub plugins: std::collections::HashSet<String>,
    pub allow: std::collections::HashSet<String>,
    pub deny: std::collections::HashSet<String>,
    /// Session-scope registration whitelist (layer 2, distinct from the
    /// `skills`/`mcp`/`plugins` layer-3 live-load sets above). Folded from
    /// `Registry` events only — never seeded from config or an `Agent`
    /// checkpoint. See docs/plans/2026-07-06-scoped-registry-revamp.md Phase 2.
    pub session_skills: std::collections::HashSet<String>,
    pub session_mcp: std::collections::HashSet<String>,
    pub session_plugins: std::collections::HashSet<String>,
}

impl AgentState {
    /// Enabled loadable names = skills ∪ mcp ∪ plugins. Replaces the deleted `ActiveLoadables`
    /// state (a flat name-set) so `<loadable-index>` and the MCP-load loop read
    /// identical output.
    pub fn enabled_loadables(&self) -> std::collections::HashSet<String> {
        self.skills
            .union(&self.mcp)
            .chain(self.plugins.iter())
            .cloned()
            .collect()
    }

    /// Read accessor for the session-scope skills whitelist (Phase 2).
    pub fn session_skills(&self) -> &std::collections::HashSet<String> {
        &self.session_skills
    }
    /// Read accessor for the session-scope mcp whitelist (Phase 2).
    pub fn session_mcp(&self) -> &std::collections::HashSet<String> {
        &self.session_mcp
    }
    /// Read accessor for the session-scope plugins whitelist (Phase 2).
    pub fn session_plugins(&self) -> &std::collections::HashSet<String> {
        &self.session_plugins
    }

    /// The genesis seed (M3): enabled-set derived from config exactly like
    /// `Profile::from_config_and_agent` derives `config_enabled` (profile.rs:75-78)
    /// — enabled mcp_servers + enabled skills — plus the resolved model /
    /// instructions / allow / deny. So the first turn (before any `Agent` event
    /// lands) yields the right enabled-set.
    pub fn from_config(
        cfg: &crate::config::GaitConfig,
        model: String,
        instructions: String,
        allow: Vec<String>,
        deny: Vec<String>,
    ) -> Self {
        let mcp = cfg
            .mcp_servers
            .iter()
            .filter(|s| s.enabled)
            .map(|s| s.name.clone())
            .collect();
        let skills = cfg
            .skills
            .iter()
            .filter(|s| s.enabled)
            .map(|s| s.name.clone())
            .collect();
        AgentState {
            model,
            instructions,
            tools: Default::default(),
            skills,
            mcp,
            plugins: Default::default(),
            allow: allow.into_iter().collect(),
            deny: deny.into_iter().collect(),
            inference: None,
            session_skills: Default::default(),
            session_mcp: Default::default(),
            session_plugins: Default::default(),
        }
    }
}

/// Checkpoint reset path: the `Agent` proto snapshot → working state.
/// (Vec→Set for the enabled fields; permissions split into allow/deny sets.)
impl From<&pb::Agent> for AgentState {
    fn from(a: &pb::Agent) -> Self {
        let (allow, deny) = a
            .permissions
            .as_ref()
            .map(|p| {
                (
                    p.allow.iter().cloned().collect(),
                    p.deny.iter().cloned().collect(),
                )
            })
            .unwrap_or_default();
        AgentState {
            model: a
                .inference
                .as_ref()
                .map(|i| i.model.clone())
                .unwrap_or_default(),
            inference: a.inference.clone(),
            instructions: a.instructions.clone(),
            tools: a.tools.iter().cloned().collect(),
            skills: a.skills.iter().cloned().collect(),
            mcp: a.mcp_servers.iter().cloned().collect(),
            plugins: a.plugins.iter().cloned().collect(),
            allow,
            deny,
            session_skills: Default::default(),
            session_mcp: Default::default(),
            session_plugins: Default::default(),
        }
    }
}

/// The agent-dimension projection (replaces ActiveManifest + ActiveLoadables).
/// `seed` is the config-derived genesis (M3). `checkpoint` resets the baseline to
/// an `Agent` snapshot (recompose-preserving re-baseline, I4). `apply` folds the
/// deltas: in 3a that's the `load` ToolCall compat path (byte-identical to the old
/// ActiveLoadables), switched to typed `Toggle` events in 3b. Inference is
/// folded independently so an agent checkpoint cannot overwrite it.
pub struct ActiveAgent {
    pub seed: AgentState,
}
impl Projection for ActiveAgent {
    type State = AgentState;
    fn seed(&self) -> AgentState {
        self.seed.clone()
    }
    fn checkpoint(&self, kind: &payload::Kind) -> Option<AgentState> {
        if let payload::Kind::Agent(a) = kind {
            Some(AgentState::from(a))
        } else {
            None
        }
    }
    fn apply(&self, st: &mut AgentState, _ev: &Event, kind: &payload::Kind) {
        use crate::proto::gait::log::v1::ToggleKind;
        match kind {
            payload::Kind::Toggle(t) => {
                let set = match ToggleKind::try_from(t.kind).unwrap_or(ToggleKind::Unspecified) {
                    ToggleKind::Skill => &mut st.skills,
                    ToggleKind::Mcp => &mut st.mcp,
                    ToggleKind::Tool => &mut st.tools,
                    ToggleKind::Plugin => &mut st.plugins,
                    ToggleKind::Unspecified => return,
                };
                if t.enabled {
                    set.insert(t.name.clone());
                } else {
                    set.remove(&t.name);
                }
            }
            // Registry is the layer-2 registration/whitelist axis (distinct from
            // Toggle's layer-3 live-load axis above) — Tool has no meaning here
            // (mirror the Unspecified guard, but for both variants).
            payload::Kind::Registry(r) => {
                let set = match ToggleKind::try_from(r.kind).unwrap_or(ToggleKind::Unspecified) {
                    ToggleKind::Skill => &mut st.session_skills,
                    ToggleKind::Mcp => &mut st.session_mcp,
                    ToggleKind::Plugin => &mut st.session_plugins,
                    ToggleKind::Tool | ToggleKind::Unspecified => return,
                };
                if r.listed {
                    set.insert(r.name.clone());
                } else {
                    set.remove(&r.name);
                }
            }
            _ => {}
        }
    }
}

/// Last-write-wins working directory. Seeds at the codebase root; each `Cwd`
/// event overwrites. Folded over the full session (survives compaction), so the
/// sticky cwd persists across turns and `undo` reverts it (the event is truncated).
pub struct ActiveCwd {
    pub root: std::path::PathBuf,
}
impl Projection for ActiveCwd {
    type State = std::path::PathBuf;
    fn seed(&self) -> Self::State {
        self.root.clone()
    }
    fn apply(&self, state: &mut Self::State, _ev: &Event, kind: &payload::Kind) {
        if let payload::Kind::Cwd(c) = kind {
            *state = std::path::PathBuf::from(&c.path);
        }
    }
}

/// Active-mode overlay — LWW over `Mode` events across the full session.
/// Standalone (like `ActiveCwd`): NO checkpoint override, so an `Agent`
/// re-baseline never clears it (finding #1). `None` = default / no overlay.
pub struct ActiveMode;
impl Projection for ActiveMode {
    type State = Option<pb::Mode>;
    fn seed(&self) -> Self::State {
        None
    }
    fn apply(&self, state: &mut Self::State, _ev: &Event, kind: &payload::Kind) {
        if let payload::Kind::Mode(m) = kind {
            *state = if m.name.is_empty() {
                None
            } else {
                Some(m.clone())
            };
        }
    }
}

/// Fold the active mode overlay from the mutation-applied full-session stream.
pub fn active_mode(events: &[Event], muts: &HashMap<String, MutationState>) -> Option<pb::Mode> {
    run_projection(&ActiveMode, events, muts)
}

/// LWW selected inference.  This is deliberately separate from `ActiveAgent`:
/// an Agent checkpoint is an audit baseline and must not clear a session choice.
pub struct ActiveInference;
impl Projection for ActiveInference {
    type State = Option<pb::Inference>;
    fn seed(&self) -> Self::State {
        None
    }
    fn apply(&self, state: &mut Self::State, _ev: &Event, kind: &payload::Kind) {
        if let payload::Kind::Inference(inference) = kind {
            *state = (!inference.name.is_empty()).then(|| inference.clone());
        }
    }
}

pub fn active_inference(
    events: &[Event],
    muts: &HashMap<String, MutationState>,
) -> Option<pb::Inference> {
    run_projection(&ActiveInference, events, muts)
}

/// Coalescing phase for the `ActiveMessages` fold: tracks whether the previous
/// folded event was part of a consecutive tool-call run or tool-result run, so
/// consecutive `ToolCall`s collapse into one assistant message and consecutive
/// `ToolResult`s into one tool message (the shape providers expect). Any other
/// event resets the phase to `Other` (it breaks a tool run).
#[derive(Default, PartialEq, Clone, Copy)]
enum MsgPhase {
    #[default]
    Other,
    ToolCallRun,
    ToolResultRun,
}

/// Accumulator for the `ActiveRawMessages` projection — one `Message` per event
/// in source order, no phase coalescing. `Inject` lands here (Design A — a hook's
/// injection is permanent in the transcript; the LLM sees it from the next fold).
/// `ContextStatus` is metadata-only and dropped. `Summary` is NOT a checkpoint:
/// the transcript preserves all events; compaction's effect on the LLM is the
/// inject/summary event itself, not a fold reset.
pub struct RawMessagesState {
    pub base: Vec<Message>,
}
/// Accumulator for the `ActiveMessages` projection.
pub struct MessagesState {
    msgs: Vec<Message>,
    phase: MsgPhase,
}
impl MessagesState {
    /// The post-compaction baseline: a context restarted from the synthesized
    /// summary user message. Reproduces the historical clear()-then-push reset.
    fn with_summary(summary: &str) -> Self {
        MessagesState {
            msgs: vec![Message::User {
                content: MessageContent::Text(format!("[Session history summary: {}]", summary)),
                id: uuid::Uuid::new_v4().to_string(),
            }],
            phase: MsgPhase::Other,
        }
    }

    fn push_user(&mut self, text: String, id: &str) {
        self.msgs.push(Message::User {
            content: MessageContent::Text(text),
            id: id.to_string(),
        });
        self.phase = MsgPhase::Other;
    }

    fn push_assistant(&mut self, text: String, id: &str) {
        self.msgs.push(Message::Assistant {
            content: AssistantContent::Text(text),
            id: id.to_string(),
        });
        self.phase = MsgPhase::Other;
    }

    fn coalesce_tool_call(&mut self, tc: &pb::ToolCall, id: &str) {
        let part = AssistantPart::ToolCall(ToolCall {
            tool_call_id: tc.call_id.clone(),
            tool_name: tc.name.clone(),
            args: struct_to_value(&tc.input),
        });
        if self.phase == MsgPhase::ToolCallRun {
            if let Some(Message::Assistant {
                content: AssistantContent::Parts(parts),
                ..
            }) = self.msgs.last_mut()
            {
                parts.push(part);
            } else {
                self.msgs.push(Message::Assistant {
                    content: AssistantContent::Parts(vec![part]),
                    id: id.to_string(),
                });
            }
        } else {
            self.msgs.push(Message::Assistant {
                content: AssistantContent::Parts(vec![part]),
                id: id.to_string(),
            });
        }
        self.phase = MsgPhase::ToolCallRun;
    }

    fn coalesce_tool_result(&mut self, tr: &pb::ToolResult, id: &str) {
        let res = ToolResult {
            kind: ToolResultKind::ToolResult,
            tool_call_id: tr.call_id.clone(),
            result: serde_json::Value::String(tr.output.clone()),
        };
        match self.phase {
            MsgPhase::ToolCallRun => {
                self.msgs.push(Message::ToolResult {
                    content: vec![res],
                    id: id.to_string(),
                });
                self.phase = MsgPhase::ToolResultRun;
            }
            MsgPhase::ToolResultRun => {
                if let Some(Message::ToolResult { content, .. }) = self.msgs.last_mut() {
                    content.push(res);
                }
            }
            MsgPhase::Other => {}
        }
    }

    fn push_agent_update(&mut self, au: &pb::AgentUpdate, id: &str) {
        let status = pb::AgentStatus::try_from(au.status)
            .map(|s| match s {
                pb::AgentStatus::Done => "done",
                pb::AgentStatus::Clarify => "clarify",
                pb::AgentStatus::Inform => "inform",
                pb::AgentStatus::Unspecified => "unspecified",
            })
            .unwrap_or("unspecified");
        let mut content = format!(
            "[agent:{}] status: {}\nsummary: {}",
            au.session_id, status, au.summary
        );
        if let Some(items) = &au.items {
            let v = serde_json::to_value(items).unwrap_or(serde_json::Value::Null);
            content.push_str(&format!("\nitems: {}", v));
        }
        self.push_user(content, id);
    }

    fn push_daemon_update(&mut self, du: &pb::DaemonUpdate, id: &str) {
        let status = pb::DaemonStatus::try_from(du.status)
            .map(|s| match s {
                pb::DaemonStatus::Done => "done",
                pb::DaemonStatus::Killed => "killed",
                pb::DaemonStatus::Unspecified => "unspecified",
            })
            .unwrap_or("unspecified");
        let content = format!(
            "[daemon:{}] {} — exit {} — log: {}",
            du.name, status, du.exit_code, du.log_path
        );
        self.push_user(content, id);
    }

    fn push_clarify(&mut self, c: &pb::Clarify, id: &str) {
        let text = c
            .items
            .iter()
            .map(|q| {
                format!(
                    "clarify ({}): {}\nwhy: {}",
                    clarify_kind_str(q.kind),
                    q.content,
                    q.why
                )
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        self.push_assistant(text, id);
    }

    fn push_clarify_reply(&mut self, cr: &pb::ClarifyReply, id: &str) {
        let text = cr
            .items
            .iter()
            .map(|r| {
                let base = format!("{}: {}", r.id, r.content);
                if let Some(reason) = &r.reasoning {
                    format!("{}\nreason: {}", base, reason)
                } else {
                    base
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        self.push_user(text, id);
    }

    fn push_clarify_convergence(&mut self, cc: &pb::ClarifyConvergence, id: &str) {
        for item in &cc.items {
            self.msgs.push(Message::Assistant {
                content: AssistantContent::Text(format!("[proposal] {}", item.proposal)),
                id: id.to_string(),
            });
            if let Some(resp) = &item.response {
                self.msgs.push(Message::User {
                    content: MessageContent::Text(format!("[response] {}", resp)),
                    id: id.to_string(),
                });
            }
        }
        self.phase = MsgPhase::Other;
    }

    fn push_inform(&mut self, inf: &pb::Inform, id: &str) {
        let text = inf
            .items
            .iter()
            .map(|it| {
                let mut s = it.content.clone();
                if let Some(ctx) = &it.context {
                    s.push_str(&format!("\ncontext: {}", ctx));
                }
                s
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        self.push_assistant(text, id);
    }

    fn push_inform_reply(&mut self, ir: &pb::InformReply, id: &str) {
        let text = ir
            .items
            .iter()
            .map(|r| format!("{}: {}", r.id, r.content))
            .collect::<Vec<_>>()
            .join("\n");
        self.push_user(text, id);
    }

    fn push_error(&mut self, e: &pb::Error, id: &str) {
        let phase_str = pb::ErrorPhase::try_from(e.phase)
            .map(|p| match p {
                pb::ErrorPhase::Llm => "llm",
                pb::ErrorPhase::Tool => "tool",
                pb::ErrorPhase::Git => "git",
                pb::ErrorPhase::Init => "init",
                pb::ErrorPhase::Unspecified => "unspecified",
            })
            .unwrap_or("unspecified");
        self.push_user(format!("error during {}: {}", phase_str, e.message), id);
    }
}

/// The Transcript: the `Vec<Message>` the model reads, folded from the Log.
/// Resets on a `Summary` event (restart the message context from the summary;
/// state projections carry forward and ignore it). Consecutive tool
/// calls/results coalesce into single assistant/tool messages. Honors mutations
/// via the effective-event stream: redacted events never arrive here; replaced
/// events arrive as their replacement kind.
pub struct ActiveMessages;
impl Projection for ActiveMessages {
    type State = MessagesState;
    fn seed(&self) -> Self::State {
        MessagesState {
            msgs: vec![],
            phase: MsgPhase::Other,
        }
    }
    /// `Summary` is the message dimension's checkpoint: reset the context to a
    /// baseline restarted from the synthesized summary (C2 — a fold-from-payload,
    /// not empty). Reset is positional (C1): `through_seq` is never read.
    fn checkpoint(&self, kind: &payload::Kind) -> Option<Self::State> {
        if let payload::Kind::Summary(c) = kind {
            Some(MessagesState::with_summary(&c.summary))
        } else {
            None
        }
    }
    fn apply(&self, st: &mut Self::State, ev: &Event, kind: &payload::Kind) {
        match kind {
            payload::Kind::ToolCall(tc) => st.coalesce_tool_call(tc, &ev.id),
            // A tool_result renders only inside a tool run: right after a tool-call
            // run, or appended to an ongoing result run. An orphan result (phase
            // Other) is silently skipped — see coalesce_tool_result.
            payload::Kind::ToolResult(tr) => st.coalesce_tool_result(tr, &ev.id),
            payload::Kind::UserText(u) => st.push_user(u.content.clone(), &ev.id),
            payload::Kind::AssistantText(a) => st.push_assistant(a.content.clone(), &ev.id),
            payload::Kind::Inject(ti) => match ti.role.as_str() {
                "assistant" => st.push_assistant(ti.content.clone(), &ev.id),
                _ => st.push_user(ti.content.clone(), &ev.id),
            },
            payload::Kind::ContextStatus(_) => {
                let _ = ();
            } // metadata only
            payload::Kind::AgentUpdate(au) => st.push_agent_update(au, &ev.id),
            payload::Kind::DaemonUpdate(du) => st.push_daemon_update(du, &ev.id),
            payload::Kind::Clarify(c) => st.push_clarify(c, &ev.id),
            payload::Kind::ClarifyReply(cr) => st.push_clarify_reply(cr, &ev.id),
            payload::Kind::ClarifyConvergence(cc) => st.push_clarify_convergence(cc, &ev.id),
            payload::Kind::Inform(inf) => st.push_inform(inf, &ev.id),
            payload::Kind::InformReply(ir) => st.push_inform_reply(ir, &ev.id),
            payload::Kind::Error(e) => st.push_error(e, &ev.id),
            // Sidecar events transparent to tool-run coalescing: the shell tool
            // emits Cwd BETWEEN a tool_call and its tool_result; hooks emit
            // HookRun around execution. Resetting phase here orphans the following
            // tool_result ("tool call result does not follow tool call").
            // ThinkingBlock is also transparent here — the coalesced view does not
            // expose thinking as a separate message; retrace fold handles it below.
            payload::Kind::Cwd(_) | payload::Kind::HookRun(_) | payload::Kind::ThinkingBlock(_) => {
            }

            payload::Kind::CancellationSignal(_) => {
                st.push_user("[turn interrupted by user]".to_string(), &ev.id);
            }
            // SessionStart, TurnComplete, Raw, InformConvergence, Manifest,
            // Permissions/PermissionsReply — not model context; break any tool run.
            _ => {
                st.phase = MsgPhase::Other;
            }
        }
    }
}

/// The raw transcript projection: every event in source order becomes exactly
/// one `Message` in `base` — no phase coalescing (that happens in the free
/// [`coalesce`] function after the fold). `Inject` events land in `base` (Design
/// A: a hook's injection is permanent in the transcript; the next fold yields
/// the injected message to the LLM). `ContextStatus` is metadata-only and
/// ignored. Unlike `ActiveMessages`, there is NO `Summary` checkpoint reset —
/// the transcript preserves all events; compaction's effect on the LLM is the
/// synthesized summary user message itself (the `Summary` event folds via
/// `active_messages_into_log` in task 5), not a fold reset. The downstream
/// `coalesce()` merges consecutive `ToolCall` assistant messages and consecutive
/// `ToolResult` messages before they hit the provider.
pub struct ActiveRawMessages;
impl Projection for ActiveRawMessages {
    type State = RawMessagesState;

    fn seed(&self) -> Self::State {
        RawMessagesState { base: Vec::new() }
    }

    fn apply(&self, st: &mut Self::State, ev: &Event, kind: &payload::Kind) {
        match kind {
            payload::Kind::UserText(u) => st.base.push(Message::User {
                content: MessageContent::Text(u.content.clone()),
                id: ev.id.clone(),
            }),
            payload::Kind::AssistantText(a) => st.base.push(Message::Assistant {
                content: AssistantContent::Text(a.content.clone()),
                id: ev.id.clone(),
            }),
            payload::Kind::ToolCall(tc) => {
                let part = AssistantPart::ToolCall(ToolCall {
                    tool_call_id: tc.call_id.clone(),
                    tool_name: tc.name.clone(),
                    args: struct_to_value(&tc.input),
                });
                st.base.push(Message::Assistant {
                    content: AssistantContent::Parts(vec![part]),
                    id: ev.id.clone(),
                });
            }
            payload::Kind::ToolResult(tr) => {
                let res = ToolResult {
                    kind: ToolResultKind::ToolResult,
                    tool_call_id: tr.call_id.clone(),
                    result: serde_json::Value::String(tr.output.clone()),
                };
                st.base.push(Message::ToolResult {
                    content: vec![res],
                    id: ev.id.clone(),
                });
            }
            payload::Kind::Inject(ti) => {
                // Design A: goes into base, permanent in transcript
                let msg = match ti.role.as_str() {
                    "assistant" => Message::Assistant {
                        content: AssistantContent::Text(ti.content.clone()),
                        id: ev.id.clone(),
                    },
                    _ => Message::User {
                        content: MessageContent::Text(ti.content.clone()),
                        id: ev.id.clone(),
                    },
                };
                st.base.push(msg);
            }
            payload::Kind::ContextStatus(_) => {} // metadata only
            payload::Kind::CancellationSignal(_) => st.base.push(Message::User {
                content: MessageContent::Text("[turn interrupted by user]".to_string()),
                id: ev.id.clone(),
            }),
            payload::Kind::AgentUpdate(au) => {
                let status = pb::AgentStatus::try_from(au.status)
                    .map(|s| match s {
                        pb::AgentStatus::Done => "done",
                        pb::AgentStatus::Clarify => "clarify",
                        pb::AgentStatus::Inform => "inform",
                        pb::AgentStatus::Unspecified => "unspecified",
                    })
                    .unwrap_or("unspecified");
                let mut content = format!(
                    "[agent:{}] status: {}\nsummary: {}",
                    au.session_id, status, au.summary
                );
                if let Some(items) = &au.items {
                    let v = serde_json::to_value(items).unwrap_or(serde_json::Value::Null);
                    content.push_str(&format!("\nitems: {}", v));
                }
                st.base.push(Message::User {
                    content: MessageContent::Text(content),
                    id: ev.id.clone(),
                });
            }
            payload::Kind::DaemonUpdate(du) => {
                let status = pb::DaemonStatus::try_from(du.status)
                    .map(|s| match s {
                        pb::DaemonStatus::Done => "done",
                        pb::DaemonStatus::Killed => "killed",
                        pb::DaemonStatus::Unspecified => "unspecified",
                    })
                    .unwrap_or("unspecified");
                let content = format!(
                    "[daemon:{}] {} — exit {} — log: {}",
                    du.name, status, du.exit_code, du.log_path
                );
                st.base.push(Message::User {
                    content: MessageContent::Text(content),
                    id: ev.id.clone(),
                });
            }
            payload::Kind::Clarify(c) => {
                let text = c
                    .items
                    .iter()
                    .map(|q| {
                        format!(
                            "clarify ({}): {}\nwhy: {}",
                            clarify_kind_str(q.kind),
                            q.content,
                            q.why
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n\n");
                st.base.push(Message::Assistant {
                    content: AssistantContent::Text(text),
                    id: ev.id.clone(),
                });
            }
            payload::Kind::ClarifyReply(cr) => {
                let text = cr
                    .items
                    .iter()
                    .map(|r| {
                        let base = format!("{}: {}", r.id, r.content);
                        if let Some(reason) = &r.reasoning {
                            format!("{}\nreason: {}", base, reason)
                        } else {
                            base
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                st.base.push(Message::User {
                    content: MessageContent::Text(text),
                    id: ev.id.clone(),
                });
            }
            payload::Kind::ClarifyConvergence(cc) => {
                for item in &cc.items {
                    st.base.push(Message::Assistant {
                        content: AssistantContent::Text(format!("[proposal] {}", item.proposal)),
                        id: ev.id.clone(),
                    });
                    if let Some(resp) = &item.response {
                        st.base.push(Message::User {
                            content: MessageContent::Text(format!("[response] {}", resp)),
                            id: ev.id.clone(),
                        });
                    }
                }
            }
            payload::Kind::Inform(inf) => {
                let text = inf
                    .items
                    .iter()
                    .map(|it| {
                        let mut s = it.content.clone();
                        if let Some(ctx) = &it.context {
                            s.push_str(&format!("\ncontext: {}", ctx));
                        }
                        s
                    })
                    .collect::<Vec<_>>()
                    .join("\n\n");
                st.base.push(Message::Assistant {
                    content: AssistantContent::Text(text),
                    id: ev.id.clone(),
                });
            }
            payload::Kind::InformReply(ir) => {
                let text = ir
                    .items
                    .iter()
                    .map(|r| format!("{}: {}", r.id, r.content))
                    .collect::<Vec<_>>()
                    .join("\n");
                st.base.push(Message::User {
                    content: MessageContent::Text(text),
                    id: ev.id.clone(),
                });
            }
            payload::Kind::Error(e) => {
                let phase_str = pb::ErrorPhase::try_from(e.phase)
                    .map(|p| match p {
                        pb::ErrorPhase::Llm => "llm",
                        pb::ErrorPhase::Tool => "tool",
                        pb::ErrorPhase::Git => "git",
                        pb::ErrorPhase::Init => "init",
                        pb::ErrorPhase::Unspecified => "unspecified",
                    })
                    .unwrap_or("unspecified");
                st.base.push(Message::User {
                    content: MessageContent::Text(format!(
                        "error during {}: {}",
                        phase_str, e.message
                    )),
                    id: ev.id.clone(),
                });
            }
            payload::Kind::Cwd(_) | payload::Kind::HookRun(_) => {}
            payload::Kind::ThinkingBlock(tb) => {
                // Fold into the raw transcript as AssistantPart::Thinking so
                // to_anthropic_messages() can pass the signature back verbatim
                // in multi-turn replay.
                let part = crate::types::AssistantPart::Thinking {
                    content: tb.content.clone(),
                    signature: if tb.signature.is_empty() {
                        None
                    } else {
                        Some(tb.signature.clone())
                    },
                };
                st.base.push(Message::Assistant {
                    content: crate::types::AssistantContent::Parts(vec![part]),
                    id: ev.id.clone(),
                });
            }
            _ => {}
        }
    }
}

/// Merge consecutive tool-call assistant messages and consecutive tool-result
/// messages into one message each — the shape providers expect. Operates on the
/// output of `ActiveRawMessages` (one `Message` per event, no coalescing).
///
/// Coalescing rules (mirroring the historical `ActiveMessages` behaviour):
/// - Consecutive single-part `Assistant::Parts([ToolCall])` messages merge
///   into one `Assistant::Parts([ToolCall, ToolCall, ...])`.
/// - Consecutive `ToolResult` messages merge their `content` vecs.
/// - A `ToolResult` after a tool-call run starts a new `ToolResult` run.
/// - An orphan `ToolResult` (phase Other — no preceding tool call) is dropped
///   (preserves the existing "tool call result does not follow tool call" guard).
/// - Any other message resets the phase to Other and breaks the run.
pub fn coalesce(raw: Vec<Message>) -> Vec<Message> {
    let mut out: Vec<Message> = Vec::new();
    let mut phase = MsgPhase::Other;
    for msg in raw {
        match &msg {
            // Thinking-only assistant message: fold into the last Assistant(Parts)
            // if one exists, or start a new one. The following text/tool-call
            // message will be merged in by the next iteration.
            Message::Assistant {
                content: AssistantContent::Parts(parts),
                id,
            } if !parts.is_empty()
                && parts
                    .iter()
                    .all(|p| matches!(p, AssistantPart::Thinking { .. })) =>
            {
                if let Some(Message::Assistant {
                    content: AssistantContent::Parts(existing),
                    ..
                }) = out.last_mut()
                {
                    existing.extend(parts.clone());
                } else {
                    out.push(msg);
                }
                phase = MsgPhase::Other;
            }
            Message::Assistant {
                content: AssistantContent::Parts(parts),
                ..
            } if parts.len() == 1 && matches!(&parts[0], AssistantPart::ToolCall(_)) => {
                if phase == MsgPhase::ToolCallRun {
                    if let Some(Message::Assistant {
                        content: AssistantContent::Parts(existing),
                        ..
                    }) = out.last_mut()
                    {
                        existing.push(parts[0].clone());
                    } else {
                        out.push(msg);
                    }
                } else {
                    out.push(msg);
                }
                phase = MsgPhase::ToolCallRun;
            }
            // Text assistant message following a Thinking-only Part: merge.
            Message::Assistant {
                content: AssistantContent::Text(text),
                id: _,
            } => {
                if let Some(Message::Assistant {
                    content: AssistantContent::Parts(existing),
                    ..
                }) = out.last_mut().filter(|m| {
                    matches!(m, Message::Assistant { content: AssistantContent::Parts(p), .. }
                        if p.iter().any(|p| matches!(p, AssistantPart::Thinking { .. })))
                }) {
                    existing.push(AssistantPart::Text { text: text.clone() });
                } else {
                    out.push(msg);
                }
                phase = MsgPhase::Other;
            }
            // Parts assistant message following a Thinking-only Part: merge.
            Message::Assistant {
                content: AssistantContent::Parts(parts),
                id: _,
            } => {
                if let Some(Message::Assistant {
                    content: AssistantContent::Parts(existing),
                    ..
                }) = out.last_mut().filter(|m| {
                    matches!(m, Message::Assistant { content: AssistantContent::Parts(p), .. }
                        if p.iter().any(|p| matches!(p, AssistantPart::Thinking { .. })))
                }) {
                    existing.extend(parts.clone());
                    phase = MsgPhase::ToolCallRun;
                } else {
                    out.push(msg);
                    phase = MsgPhase::ToolCallRun;
                }
            }
            Message::ToolResult { content, .. } => match phase {
                MsgPhase::ToolCallRun => {
                    out.push(msg);
                    phase = MsgPhase::ToolResultRun;
                }
                MsgPhase::ToolResultRun => {
                    if let Some(Message::ToolResult {
                        content: existing, ..
                    }) = out.last_mut()
                    {
                        existing.extend(content.clone());
                    }
                }
                MsgPhase::Other => {}
            },
            _ => {
                out.push(msg);
                phase = MsgPhase::Other;
            }
        }
    }
    out
}

/// Effective allow-list state: config baseline + log-derived grants, split by
/// lifetime. `session` persists for the whole session; `once` is cleared on every
/// `TurnComplete` (active only within the turn it was granted).
#[derive(Default)]
pub struct AllowState {
    pub session: std::collections::HashSet<String>,
    pub once: std::collections::HashSet<String>,
}

/// Effective session allow-list projection. Seeds `session` from config
/// (global+agent grants, already loaded). Folds `PermissionsReply` as an
/// append-only ledger: a `session` approve inserts and a `session` deny REMOVES
/// the pattern (the honest forward revoke — both events stay in the Log; only the
/// derived set changes); a `once` approve is active until the next `TurnComplete`.
/// `agent`/`global` scopes are config-file backed and ignored here. Because it
/// folds the mutation-applied stream, undo/redact of a grant also drops it — but
/// redact is for mistakes, not normal revocation.
pub struct ActivePermissions {
    pub seed: std::collections::HashSet<String>,
}
impl Projection for ActivePermissions {
    type State = AllowState;
    fn seed(&self) -> Self::State {
        AllowState {
            session: self.seed.clone(),
            once: Default::default(),
        }
    }
    fn apply(&self, state: &mut Self::State, _ev: &Event, kind: &payload::Kind) {
        match kind {
            payload::Kind::TurnComplete(_) => state.once.clear(), // once grants expire per turn
            payload::Kind::PermissionsReply(r) => {
                for item in &r.items {
                    let Some(pat) = item.text.clone() else {
                        continue;
                    };
                    let decision =
                        pb::Decision::try_from(item.decision).unwrap_or(pb::Decision::Unspecified);
                    let scope = pb::Scope::try_from(item.scope).unwrap_or(pb::Scope::Unspecified);
                    match (scope, decision) {
                        (pb::Scope::Session, pb::Decision::Allow) => {
                            state.session.insert(pat);
                        }
                        (pb::Scope::Session, pb::Decision::Deny) => {
                            state.session.remove(&pat);
                        } // forward revoke
                        (pb::Scope::Once, pb::Decision::Allow) => {
                            state.once.insert(pat);
                        }
                        // agent/global via config seed; once-deny is a no-op.
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
}

/// State for active sandbox unlock grants: paths granted this session or once.
#[derive(Default)]
pub struct ActiveUnlockState {
    /// Paths granted for the entire session (survive `TurnComplete`).
    pub session: std::collections::HashSet<std::path::PathBuf>,
    /// Paths granted for a single turn (cleared on `TurnComplete`).
    pub once: std::collections::HashSet<std::path::PathBuf>,
}

/// Fold all `SandboxUnlockReply` events, cross-referencing `SandboxUnlock`
/// events to resolve resource paths, into an `ActiveUnlockState`. `once`
/// grants are cleared on each `TurnComplete`; `session` grants persist for
/// the session. `global`/`agent` grants are config-backed and not tracked here.
pub fn active_unlock_state(events: &[Event]) -> ActiveUnlockState {
    // Build a map: unlock_item_id → resource from SandboxUnlock events
    let mut id_to_resource: std::collections::HashMap<String, String> = Default::default();
    for e in events {
        if let Some(payload::Kind::SandboxUnlock(u)) = crate::proto::event_kind(e) {
            for item in &u.items {
                id_to_resource.insert(item.id.clone(), item.resource.clone());
            }
        }
    }

    let mut state = ActiveUnlockState::default();
    for e in events {
        match crate::proto::event_kind(e) {
            Some(payload::Kind::TurnComplete(_)) => {
                state.once.clear();
            }
            Some(payload::Kind::SandboxUnlockReply(r)) => {
                for item in &r.items {
                    let decision =
                        pb::Decision::try_from(item.decision).unwrap_or(pb::Decision::Unspecified);
                    let scope = pb::Scope::try_from(item.scope).unwrap_or(pb::Scope::Unspecified);
                    if decision != pb::Decision::Allow {
                        continue;
                    }
                    let Some(resource) = id_to_resource.get(&item.id) else {
                        continue;
                    };
                    let path = std::path::PathBuf::from(resource);
                    match scope {
                        pb::Scope::Once => {
                            state.once.insert(path);
                        }
                        pb::Scope::Session => {
                            state.session.insert(path);
                        }
                        // global/agent: config-backed, not tracked here
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    state
}

/// Fold all Redact/Replace event bodies into a LWW map keyed by target id.
/// For each target, the mutation with the highest `(seq, id)` wins.
/// Events that are not Redact/Replace bodies are ignored.
pub fn active_mutations(events: &[Event]) -> HashMap<String, MutationState> {
    // key: target id, value: (winner_seq, winner_id, MutationState)
    let mut map: HashMap<String, (u64, String, MutationState)> = HashMap::new();

    for e in events {
        let (target, state) = match &e.body {
            Some(event::Body::Redact(r)) => (
                r.target.clone(),
                MutationState::Redacted {
                    reason: r.reason.clone(),
                },
            ),
            Some(event::Body::Replace(r)) => {
                if let Some(payload) = r.replacement.clone() {
                    (
                        r.target.clone(),
                        MutationState::Replaced {
                            payload: Box::new(payload),
                            reason: r.reason.clone(),
                        },
                    )
                } else {
                    continue;
                }
            }
            _ => continue,
        };

        let entry = map
            .entry(target)
            .or_insert_with(|| (0, String::new(), state.clone()));
        // LWW: (seq, id) total order — higher seq wins; equal seq → lex-higher id wins.
        if (e.seq, e.id.as_str()) > (entry.0, entry.1.as_str()) {
            *entry = (e.seq, e.id.clone(), state);
        }
    }

    map.into_iter().map(|(k, (_, _, v))| (k, v)).collect()
}

/// Apply the ToolCall↔ToolResult cascade: if either member of a pair is
/// Redacted, also Redact the other (overriding any existing Replace on it).
/// Mutates `mutations` in-place.
pub fn apply_cascade(mutations: &mut HashMap<String, MutationState>, events: &[Event]) {
    // Build call_id → [(event_id, is_tool_call)] for original payloads.
    let mut by_call_id: HashMap<String, Vec<(String, bool)>> = HashMap::new();
    for e in events {
        match crate::proto::event_kind(e) {
            Some(payload::Kind::ToolCall(tc)) => {
                by_call_id
                    .entry(tc.call_id.clone())
                    .or_default()
                    .push((e.id.clone(), true));
            }
            Some(payload::Kind::ToolResult(tr)) => {
                by_call_id
                    .entry(tr.call_id.clone())
                    .or_default()
                    .push((e.id.clone(), false));
            }
            _ => {}
        }
    }

    // For each Redacted event that is a ToolCall or ToolResult, cascade-Redact the pair.
    // Collect cascade targets first to avoid borrow issues.
    let cascade_targets: Vec<String> = mutations
        .iter()
        .filter_map(|(id, m)| {
            if matches!(m, MutationState::Redacted { .. }) {
                // Look up the original event to determine if it's a ToolCall/ToolResult.
                let e = events.iter().find(|e| &e.id == id)?;
                let call_id = match crate::proto::event_kind(e) {
                    Some(payload::Kind::ToolCall(tc)) => tc.call_id.clone(),
                    Some(payload::Kind::ToolResult(tr)) => tr.call_id.clone(),
                    _ => return None,
                };
                Some(call_id)
            } else {
                None
            }
        })
        .collect();

    for call_id in cascade_targets {
        if let Some(peers) = by_call_id.get(&call_id) {
            for (peer_id, _) in peers {
                // Cascade Redact overrides any existing mutation (including Replace).
                if !matches!(mutations.get(peer_id), Some(MutationState::Redacted { .. })) {
                    mutations.insert(peer_id.clone(), MutationState::Redacted { reason: None });
                }
            }
        }
    }
}

/// Result of folding the full event log through all Retrace projections.
pub struct RetraceResult {
    /// The Transcript: Vec<Message> for the model context window.
    pub messages: Vec<Message>,
    /// Folded agent-dimension state (replaces enabled_loadables + active_agent).
    pub agent: AgentState,
    /// Effective session allow-list: config baseline + log-derived session/once
    /// grants. STUB until Task 6 (currently just the config seed; empty in `fold`).
    pub effective_allow: std::collections::HashSet<String>,
    /// LWW-converged mutations (Redact/Replace) keyed by target event id,
    /// with ToolCall↔ToolResult cascade applied.
    pub mutations: HashMap<String, MutationState>,
    /// Runtime-only sandbox unlock grants (once/session scope). `once` paths
    /// expire each TurnComplete; `session` paths persist for the session.
    /// `global`/`agent` grants are in gait.jsonc / agent files and not here.
    pub active_unlock: ActiveUnlockState,
}

/// Fold the full event log into all projections in a single pass.
///
/// Every projection (`ActiveMessages`, `ActiveAgent`, `ActivePermissions`) folds
/// the SAME input: the mutation-applied, full-session event stream. Summary is
/// handled only inside `ActiveMessages` (it resets the message context); the state
/// projections fold the whole session and ignore it. `agent_seed` is the M3 genesis
/// state built from config (model, instructions, enabled mcp/skills, allow/deny).
pub fn fold(all_events: &[Event], agent_seed: AgentState) -> Result<RetraceResult, SessionError> {
    // Phase 0 — substrate: converge the mutation set (LWW + cascade) once.
    let mut mutations = active_mutations(all_events);
    apply_cascade(&mut mutations, all_events);

    // Phase 1 — drive every projection over the same effective stream.
    let agent = run_projection(&ActiveAgent { seed: agent_seed }, all_events, &mutations);
    let allow_state = run_projection(
        &ActivePermissions {
            seed: Default::default(),
        },
        all_events,
        &mutations,
    );
    let effective_allow: std::collections::HashSet<String> = allow_state
        .session
        .union(&allow_state.once)
        .cloned()
        .collect();
    let messages = run_projection(&ActiveMessages, all_events, &mutations).msgs;
    let active_unlock = active_unlock_state(all_events);

    Ok(RetraceResult {
        messages,
        agent,
        effective_allow,
        mutations,
        active_unlock,
    })
}

/// Most recent `Agent` payload LWW-Register — used only by the drift-guard in
/// `run_turn` to decide whether to emit a new `Agent` event.
pub fn active_agent_checkpoint(events: &[Event]) -> Option<pb::Agent> {
    // A one-off projection that reads just the Agent checkpoints (no seed/delta).
    struct LwwAgent;
    impl Projection for LwwAgent {
        type State = Option<pb::Agent>;
        fn seed(&self) -> Self::State {
            None
        }
        fn apply(&self, state: &mut Self::State, _ev: &Event, kind: &payload::Kind) {
            if let payload::Kind::Agent(r) = kind {
                *state = Some(r.clone());
            }
        }
    }
    run_projection(&LwwAgent, events, &HashMap::new())
}

/// Overlay the dynamic capability state from `st` onto the static `live` Agent.
/// Static fields (model, instructions, tools, permissions, through_seq) come from `live`;
/// skills, mcp_servers, and plugins are replaced with the fold-result sets from `st`, sorted for
/// determinism (the drift guard uses equality, so ordering must be stable).
pub fn rebaseline_agent(mut live: pb::Agent, st: &AgentState) -> pb::Agent {
    let mut skills: Vec<String> = st.skills.iter().cloned().collect();
    skills.sort();
    let mut mcp_servers: Vec<String> = st.mcp.iter().cloned().collect();
    mcp_servers.sort();
    let mut plugins: Vec<String> = st.plugins.iter().cloned().collect();
    plugins.sort();
    live.skills = skills;
    live.mcp_servers = mcp_servers;
    live.plugins = plugins;
    live
}

/// Folded agent-dimension state (config seed + Agent checkpoints + deltas).
pub fn active_agent(events: &[Event], seed: AgentState) -> AgentState {
    run_projection(&ActiveAgent { seed }, events, &HashMap::new())
}

/// Current sticky working directory (LWW over `Cwd` events, seeded at `root`).
/// Mutations are ignored; the shell tool and the `<status>` builder both refold
/// this so they never diverge.
pub fn active_cwd(events: &[Event], root: &std::path::Path) -> std::path::PathBuf {
    run_projection(
        &ActiveCwd {
            root: root.to_path_buf(),
        },
        events,
        &HashMap::new(),
    )
}

/// Sum of every `TurnComplete.usage` over the log — the session's cumulative
/// token usage. Replaces the dead `session.json.token_totals` sidecar; usage is
/// derived from the log, never stored twice. Folds the whole session (ignores
/// compaction — total cost includes compacted turns).
pub fn total_usage(events: &[Event]) -> crate::proto::gait::log::v1::TurnTokenUsage {
    use crate::proto::gait::log::v1::TurnTokenUsage;
    struct Usage;
    impl Projection for Usage {
        type State = TurnTokenUsage;
        fn seed(&self) -> Self::State {
            TurnTokenUsage {
                input: 0,
                output: 0,
                cache_read: 0,
                cache_write: 0,
            }
        }
        fn apply(&self, state: &mut Self::State, _ev: &Event, kind: &payload::Kind) {
            if let payload::Kind::TurnComplete(tc) = kind {
                if let Some(u) = &tc.usage {
                    state.input += u.input;
                    state.output += u.output;
                    state.cache_read += u.cache_read;
                    state.cache_write += u.cache_write;
                }
            }
        }
    }
    run_projection(&Usage, events, &HashMap::new())
}

/// Blocks from the LAST `TurnComplete` event in the log, in source order.
/// Returns an empty Vec if there is no TurnComplete. Used by the CLI
/// `>(N)` quote-reply expander, which needs the prior turn's blocks to
/// reference them by index.
pub fn last_turn_blocks(events: &[Event]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for e in events {
        if let Some(payload::Kind::TurnComplete(tc)) = crate::proto::event_kind(e) {
            out = tc.blocks.clone();
        }
    }
    out
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod cancel_tests {

    use crate::proto::{envelope, events};

    #[test]

    fn cancellation_signal_folds_into_user_message() {
        // Build a minimal log: session_start + user_turn + cancellation_signal

        let log: Vec<crate::proto::gait::log::v1::Event> = vec![
            envelope::mint(
                events::session_start("s1".into(), None, None, false),
                None,
                0,
                0,
            ),
            envelope::mint(events::user_text("hello".into(), None), None, 0, 0),
            envelope::mint(events::cancellation_signal(), None, 0, 0),
        ];

        let msgs = super::messages(&log).expect("fold ok");

        // The last message should be a user role message containing the interrupt marker

        let last = msgs.last().expect("at least one message");

        match last {
            crate::types::Message::User {
                content: crate::types::MessageContent::Text(t),
                ..
            } => {
                assert!(t.contains("interrupted by user"), "got: {t}");
            }

            other => panic!("expected User message, got {other:?}"),
        }
    }
}

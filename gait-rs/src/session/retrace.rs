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

use std::collections::HashMap;
use std::path::Path;
use crate::types::{Message, MessageContent, AssistantContent, AssistantPart, ToolCall, ToolResult, ToolResultKind};
use crate::session::transcript::read_events;
use crate::proto::gait::log::v1::{self as pb, event, payload, ClarifyKind, Event};

/// The substantive payload kind for an event, if it carries one.
fn kind(e: &Event) -> Option<&payload::Kind> {
    match &e.body {
        Some(event::Body::Payload(p)) => p.kind.as_ref(),
        _ => None,
    }
}

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
    Redacted { reason: Option<String> },
    Replaced { payload: pb::Payload, reason: Option<String> },
}

pub fn messages(all_events: &[Event]) -> anyhow::Result<Vec<Message>> {
    Ok(run_projection(&ActiveMessages, all_events, &HashMap::new()).msgs)
}

pub fn messages_from_path(path: &Path) -> anyhow::Result<Vec<Message>> {
    let all_events = read_events(path)?;
    Ok(run_projection(&ActiveMessages, &all_events, &HashMap::new()).msgs)
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
        None => kind(e),
    }
}

/// Iterate `(event, effective_kind)` over the full session, applying mutations:
/// redacted events are skipped; replaced events yield their replacement kind.
/// This is the single canonical input every `Projection` folds.
pub(crate) fn effective_kinds<'a>(
    all_events: &'a [Event],
    mutations: &'a HashMap<String, MutationState>,
) -> impl Iterator<Item = (&'a Event, &'a payload::Kind)> {
    all_events.iter().filter_map(move |e| effective_kind(e, mutations).map(|k| (e, k)))
}

/// A derived read-model folded from the Log. Every projection folds the SAME
/// input — the mutation-applied, full-session stream produced by `effective_kinds`
/// — so two projections can never disagree about what happened. Mutations are the
/// substrate (applied before folding); compaction is NOT handled here (only the
/// `ActiveMessages` projection cares about it).
pub(crate) trait Projection {
    type State;
    /// Initial state before any events (seeded from config where relevant).
    fn seed(&self) -> Self::State;
    /// If `kind` is a checkpoint for THIS projection's dimension, return the
    /// baseline state it establishes — the fold resets to it (positionally,
    /// where the event sits in the stream; `through_seq` is NOT consulted).
    /// Default: not a checkpoint for any projection.
    fn checkpoint(&self, _kind: &payload::Kind) -> Option<Self::State> { None }
    /// Fold one event's effective payload kind into the state.
    fn apply(&self, state: &mut Self::State, event: &Event, kind: &payload::Kind);
    /// Finalize (default: identity).
    fn finish(&self, state: Self::State) -> Self::State { state }
}
/// Drive one projection over the mutation-applied full-session stream.
pub(crate) fn run_projection<P: Projection>(
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
    pub instructions: String,
    pub tools: std::collections::HashSet<String>,
    pub skills: std::collections::HashSet<String>,
    pub mcp: std::collections::HashSet<String>,
    pub plugins: std::collections::HashSet<String>,
    pub allow: std::collections::HashSet<String>,
    pub deny: std::collections::HashSet<String>,
}

impl AgentState {
    /// Enabled loadable names = skills ∪ mcp ∪ plugins. Replaces the deleted `ActiveLoadables`
    /// state (a flat name-set) so `<loadable-index>` and the MCP-load loop read
    /// identical output.
    pub fn enabled_loadables(&self) -> std::collections::HashSet<String> {
        self.skills.union(&self.mcp).chain(self.plugins.iter()).cloned().collect()
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
        let mcp = cfg.mcp_servers.iter().filter(|s| s.enabled)
            .map(|s| s.name.clone()).collect();
        let skills = cfg.skills.iter().filter(|s| s.enabled)
            .map(|s| s.name.clone()).collect();
        AgentState {
            model, instructions,
            tools: Default::default(),
            skills, mcp,
            plugins: Default::default(),
            allow: allow.into_iter().collect(),
            deny: deny.into_iter().collect(),
        }
    }
}

/// Checkpoint reset path: the `Agent` proto snapshot → working state.
/// (Vec→Set for the enabled fields; permissions split into allow/deny sets.)
impl From<&pb::Agent> for AgentState {
    fn from(a: &pb::Agent) -> Self {
        let (allow, deny) = a.permissions.as_ref()
            .map(|p| (p.allow.iter().cloned().collect(),
                      p.deny.iter().cloned().collect()))
            .unwrap_or_default();
        AgentState {
            model: a.model.clone(),
            instructions: a.instructions.clone(),
            tools: a.tools.iter().cloned().collect(),
            skills: a.skills.iter().cloned().collect(),
            mcp: a.mcp_servers.iter().cloned().collect(),
            plugins: a.plugins.iter().cloned().collect(),
            allow,
            deny,
        }
    }
}

/// The agent-dimension projection (replaces ActiveManifest + ActiveLoadables).
/// `seed` is the config-derived genesis (M3). `checkpoint` resets the baseline to
/// an `Agent` snapshot (recompose-preserving re-baseline, I4). `apply` folds the
/// deltas: in 3a that's the `load` ToolCall compat path (byte-identical to the old
/// ActiveLoadables), switched to typed `Toggle`/`ModelChange` in 3b.
pub(crate) struct ActiveAgent {
    pub seed: AgentState,
}
impl Projection for ActiveAgent {
    type State = AgentState;
    fn seed(&self) -> AgentState { self.seed.clone() }
    fn checkpoint(&self, kind: &payload::Kind) -> Option<AgentState> {
        if let payload::Kind::Agent(a) = kind { Some(AgentState::from(a)) } else { None }
    }
    fn apply(&self, st: &mut AgentState, _ev: &Event, kind: &payload::Kind) {
        use crate::proto::gait::log::v1::ToggleKind;
        match kind {
            payload::Kind::Toggle(t) => {
                let set = match ToggleKind::try_from(t.kind).unwrap_or(ToggleKind::Unspecified) {
                    ToggleKind::Skill => &mut st.skills,
                    ToggleKind::Mcp   => &mut st.mcp,
                    ToggleKind::Tool  => &mut st.tools,
                    ToggleKind::Plugin => &mut st.plugins,
                    ToggleKind::Unspecified => return,
                };
                if t.enabled { set.insert(t.name.clone()); }
                else         { set.remove(&t.name); }
            }
            payload::Kind::ModelChange(m) => st.model = m.model.clone(),
            _ => {}
        }
    }
}

/// Last-write-wins working directory. Seeds at the codebase root; each `Cwd`
/// event overwrites. Folded over the full session (survives compaction), so the
/// sticky cwd persists across turns and `undo` reverts it (the event is truncated).
pub(crate) struct ActiveCwd {
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

/// Accumulator for the `ActiveMessages` projection.
pub(crate) struct MessagesState {
    msgs: Vec<Message>,
    phase: MsgPhase,
}
impl MessagesState {
    /// The post-compaction baseline: a context restarted from the synthesized
    /// summary user message. Reproduces the historical clear()-then-push reset.
    fn with_summary(summary: &str) -> Self {
        MessagesState {
            msgs: vec![Message::User {
                content: MessageContent::Text(
                    format!("[Session history summary: {}]", summary)),
            }],
            phase: MsgPhase::Other,
        }
    }
}

/// The Transcript: the `Vec<Message>` the model reads, folded from the Log.
/// Resets on a `Summary` event (restart the message context from the summary;
/// state projections carry forward and ignore it). Consecutive tool
/// calls/results coalesce into single assistant/tool messages. Honors mutations
/// via the effective-event stream: redacted events never arrive here; replaced
/// events arrive as their replacement kind.
pub(crate) struct ActiveMessages;
impl Projection for ActiveMessages {
    type State = MessagesState;
    fn seed(&self) -> Self::State {
        MessagesState { msgs: vec![], phase: MsgPhase::Other }
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
    fn apply(&self, st: &mut Self::State, _ev: &Event, kind: &payload::Kind) {
        match kind {
            payload::Kind::ToolCall(tc) => {
                let part = AssistantPart::ToolCall(ToolCall {
                    tool_call_id: tc.call_id.clone(),
                    tool_name: tc.name.clone(),
                    args: struct_to_value(&tc.input),
                });
                if st.phase == MsgPhase::ToolCallRun {
                    if let Some(Message::Assistant { content: AssistantContent::Parts(parts) }) =
                        st.msgs.last_mut()
                    {
                        parts.push(part);
                    } else {
                        st.msgs.push(Message::Assistant {
                            content: AssistantContent::Parts(vec![part]),
                        });
                    }
                } else {
                    st.msgs.push(Message::Assistant {
                        content: AssistantContent::Parts(vec![part]),
                    });
                }
                st.phase = MsgPhase::ToolCallRun;
            }

            payload::Kind::ToolResult(tr) => {
                // A tool_result renders only inside a tool run: right after a
                // tool-call run, or appended to an ongoing result run. An orphan
                // result (phase Other) is skipped, matching the original fold.
                let res = ToolResult {
                    kind: ToolResultKind::ToolResult,
                    tool_call_id: tr.call_id.clone(),
                    result: serde_json::Value::String(tr.output.clone()),
                };
                match st.phase {
                    MsgPhase::ToolCallRun => {
                        st.msgs.push(Message::Tool { content: vec![res] });
                        st.phase = MsgPhase::ToolResultRun;
                    }
                    MsgPhase::ToolResultRun => {
                        if let Some(Message::Tool { content }) = st.msgs.last_mut() {
                            content.push(res);
                        }
                    }
                    MsgPhase::Other => { /* orphan tool_result — skip */ }
                }
            }

            payload::Kind::UserTurn(u) => {
                st.msgs.push(Message::User {
                    content: MessageContent::Text(u.content.clone()),
                });
                st.phase = MsgPhase::Other;
            }

            payload::Kind::AssistantText(a) => {
                st.msgs.push(Message::Assistant {
                    content: AssistantContent::Text(a.content.clone()),
                });
                st.phase = MsgPhase::Other;
            }

            payload::Kind::AgentUpdate(au) => {
                let status = pb::AgentStatus::try_from(au.status)
                    .map(|s| match s {
                        pb::AgentStatus::Done => "done",
                        pb::AgentStatus::Clarify => "clarify",
                        pb::AgentStatus::Inform => "inform",
                        pb::AgentStatus::Unspecified => "unspecified",
                    })
                    .unwrap_or("unspecified");
                let mut content =
                    format!("[agent:{}] status: {}\nsummary: {}", au.session_id, status, au.summary);
                if let Some(items) = &au.items {
                    let v = serde_json::to_value(items).unwrap_or(serde_json::Value::Null);
                    content.push_str(&format!("\nitems: {}", v));
                }
                st.msgs.push(Message::User { content: MessageContent::Text(content) });
                st.phase = MsgPhase::Other;
            }

            payload::Kind::Clarify(c) => {
                let text = c.items.iter().map(|q| {
                    format!("clarify ({}): {}\nwhy: {}", clarify_kind_str(q.kind), q.content, q.why)
                }).collect::<Vec<_>>().join("\n\n");
                st.msgs.push(Message::Assistant { content: AssistantContent::Text(text) });
                st.phase = MsgPhase::Other;
            }

            payload::Kind::ClarifyReply(cr) => {
                let text = cr.items.iter().map(|r| {
                    let base = format!("{}: {}", r.id, r.content);
                    if let Some(reason) = &r.reasoning { format!("{}\nreason: {}", base, reason) }
                    else { base }
                }).collect::<Vec<_>>().join("\n");
                st.msgs.push(Message::User { content: MessageContent::Text(text) });
                st.phase = MsgPhase::Other;
            }

            payload::Kind::ClarifyConvergence(cc) => {
                for item in &cc.items {
                    st.msgs.push(Message::Assistant {
                        content: AssistantContent::Text(format!("[proposal] {}", item.proposal)),
                    });
                    if let Some(resp) = &item.response {
                        st.msgs.push(Message::User {
                            content: MessageContent::Text(format!("[response] {}", resp)),
                        });
                    }
                }
                st.phase = MsgPhase::Other;
            }

            payload::Kind::Inform(inf) => {
                let text = inf.items.iter().map(|it| {
                    let mut s = it.content.clone();
                    if let Some(ctx) = &it.context { s.push_str(&format!("\ncontext: {}", ctx)); }
                    s
                }).collect::<Vec<_>>().join("\n\n");
                st.msgs.push(Message::Assistant { content: AssistantContent::Text(text) });
                st.phase = MsgPhase::Other;
            }

            payload::Kind::InformReply(ir) => {
                let text = ir.items.iter()
                    .map(|r| format!("{}: {}", r.id, r.content))
                    .collect::<Vec<_>>().join("\n");
                st.msgs.push(Message::User { content: MessageContent::Text(text) });
                st.phase = MsgPhase::Other;
            }

            payload::Kind::Error(e) => {
                let phase = pb::ErrorPhase::try_from(e.phase)
                    .map(|p| match p {
                        pb::ErrorPhase::Llm => "llm",
                        pb::ErrorPhase::Tool => "tool",
                        pb::ErrorPhase::Git => "git",
                        pb::ErrorPhase::Init => "init",
                        pb::ErrorPhase::Unspecified => "unspecified",
                    })
                    .unwrap_or("unspecified");
                st.msgs.push(Message::User {
                    content: MessageContent::Text(format!("error during {}: {}", phase, e.message)),
                });
                st.phase = MsgPhase::Other;
            }

            // Not part of the model context: SessionStart, TurnComplete, Raw,
            // InformConvergence, Manifest, Permissions/PermissionsReply (audit).
            // Each still resets the coalescing phase (it breaks a tool run).
            _ => { st.phase = MsgPhase::Other; }
        }
    }
}

/// Effective allow-list state: config baseline + log-derived grants, split by
/// lifetime. `session` persists for the whole session; `once` is cleared on every
/// `TurnComplete` (active only within the turn it was granted).
#[derive(Default)]
pub(crate) struct AllowState {
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
pub(crate) struct ActivePermissions {
    pub seed: std::collections::HashSet<String>,
}
impl Projection for ActivePermissions {
    type State = AllowState;
    fn seed(&self) -> Self::State {
        AllowState { session: self.seed.clone(), once: Default::default() }
    }
    fn apply(&self, state: &mut Self::State, _ev: &Event, kind: &payload::Kind) {
        match kind {
            payload::Kind::TurnComplete(_) => state.once.clear(), // once grants expire per turn
            payload::Kind::PermissionsReply(r) => {
                for item in &r.items {
                    let Some(pat) = item.text.clone() else { continue };
                    let decision = pb::Decision::try_from(item.decision)
                        .unwrap_or(pb::Decision::Unspecified);
                    let scope = pb::Scope::try_from(item.scope)
                        .unwrap_or(pb::Scope::Unspecified);
                    match (scope, decision) {
                        (pb::Scope::Session, pb::Decision::Allow) => { state.session.insert(pat); }
                        (pb::Scope::Session, pb::Decision::Deny) => { state.session.remove(&pat); } // forward revoke
                        (pb::Scope::Once, pb::Decision::Allow) => { state.once.insert(pat); }
                        // agent/global via config seed; once-deny is a no-op.
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
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
                MutationState::Redacted { reason: r.reason.clone() },
            ),
            Some(event::Body::Replace(r)) => {
                if let Some(payload) = r.replacement.clone() {
                    (r.target.clone(), MutationState::Replaced { payload, reason: r.reason.clone() })
                } else {
                    continue;
                }
            }
            _ => continue,
        };

        let entry = map.entry(target).or_insert_with(|| (0, String::new(), state.clone()));
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
pub fn apply_cascade(
    mutations: &mut HashMap<String, MutationState>,
    events: &[Event],
) {
    // Build call_id → [(event_id, is_tool_call)] for original payloads.
    let mut by_call_id: HashMap<String, Vec<(String, bool)>> = HashMap::new();
    for e in events {
        match kind(e) {
            Some(payload::Kind::ToolCall(tc)) => {
                by_call_id.entry(tc.call_id.clone()).or_default().push((e.id.clone(), true));
            }
            Some(payload::Kind::ToolResult(tr)) => {
                by_call_id.entry(tr.call_id.clone()).or_default().push((e.id.clone(), false));
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
                let call_id = match kind(e) {
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
}

/// Fold the full event log into all projections in a single pass.
///
/// Every projection (`ActiveMessages`, `ActiveAgent`, `ActivePermissions`) folds
/// the SAME input: the mutation-applied, full-session event stream. Summary is
/// handled only inside `ActiveMessages` (it resets the message context); the state
/// projections fold the whole session and ignore it. `agent_seed` is the M3 genesis
/// state built from config (model, instructions, enabled mcp/skills, allow/deny).
pub fn fold(
    all_events: &[Event],
    agent_seed: AgentState,
) -> anyhow::Result<RetraceResult> {
    // Phase 0 — substrate: converge the mutation set (LWW + cascade) once.
    let mut mutations = active_mutations(all_events);
    apply_cascade(&mut mutations, all_events);

    // Phase 1 — drive every projection over the same effective stream.
    let agent = run_projection(&ActiveAgent { seed: agent_seed }, all_events, &mutations);
    let allow_state =
        run_projection(&ActivePermissions { seed: Default::default() }, all_events, &mutations);
    let effective_allow: std::collections::HashSet<String> =
        allow_state.session.union(&allow_state.once).cloned().collect();
    let messages = run_projection(&ActiveMessages, all_events, &mutations).msgs;

    Ok(RetraceResult { messages, agent, effective_allow, mutations })
}

/// Most recent `Agent` payload LWW-Register — used only by the drift-guard in
/// `run_turn` to decide whether to emit a new `Agent` event.
pub fn active_agent_checkpoint(events: &[Event]) -> Option<pb::Agent> {
    // A one-off projection that reads just the Agent checkpoints (no seed/delta).
    struct LwwAgent;
    impl Projection for LwwAgent {
        type State = Option<pb::Agent>;
        fn seed(&self) -> Self::State { None }
        fn apply(&self, state: &mut Self::State, _ev: &Event, kind: &payload::Kind) {
            if let payload::Kind::Agent(r) = kind { *state = Some(r.clone()); }
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
    run_projection(&ActiveCwd { root: root.to_path_buf() }, events, &HashMap::new())
}
/// Blocks from the LAST `TurnComplete` event in the log, in source order.
/// Returns an empty Vec if there is no TurnComplete. Used by the CLI
/// `>(N)` quote-reply expander, which needs the prior turn's blocks to
/// reference them by index.
pub fn last_turn_blocks(events: &[Event]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for e in events {
        if let Some(payload::Kind::TurnComplete(tc)) = kind(e) {
            out = tc.blocks.clone();
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::transcript::*;
    use crate::proto::{events, wire};
    use tempfile::TempDir;

    fn write_events(dir: &std::path::Path, bodies: Vec<event::Body>) -> std::path::PathBuf {
        let path = dir.join("log.jsonl");
        for b in bodies { wire::append(&path, b).unwrap(); }
        path
    }

    /// Build in-memory proto events from bodies (seq is irrelevant for these
    /// assertions; each is minted as a root).
    fn mint_events(bodies: Vec<event::Body>) -> Vec<Event> {
        bodies.into_iter()
            .map(|b| crate::proto::envelope::mint(b, None))
            .collect()
    }

    #[test]
    fn empty_transcript_returns_empty() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("log.jsonl");
        let msgs = messages_from_path(&path).unwrap();
        assert!(msgs.is_empty());
    }

    #[test]
    fn user_and_assistant_text_reconstructed() {
        let dir = TempDir::new().unwrap();
        let path = write_events(dir.path(), vec![
            events::session_start("s1".into(), None),
            events::user_turn("hello".into(), None, None),
            events::assistant_text("hi there".into()),
        ]);
        let msgs = messages_from_path(&path).unwrap();
        assert_eq!(msgs.len(), 2); // session_start is skipped
        assert!(matches!(&msgs[0], Message::User { content: MessageContent::Text(t) } if t == "hello"));
        assert!(matches!(&msgs[1], Message::Assistant { content: AssistantContent::Text(t) } if t == "hi there"));
    }

    #[test]
    fn tool_call_and_result_paired() {
        let dir = TempDir::new().unwrap();
        let path = write_events(dir.path(), vec![
            events::tool_call("id1".into(), "read".into(), &serde_json::json!({"file_path":"a.rs"})),
            events::tool_result("id1".into(), "contents".into(), None, None),
        ]);
        let msgs = messages_from_path(&path).unwrap();
        assert_eq!(msgs.len(), 2);
        if let Message::Assistant { content: AssistantContent::Parts(parts) } = &msgs[0] {
            assert_eq!(parts.len(), 1);
            assert!(matches!(&parts[0], AssistantPart::ToolCall(tc) if tc.tool_call_id == "id1"));
        } else { panic!("expected assistant parts"); }
        assert!(matches!(&msgs[1], Message::Tool { .. }));
    }

    #[test]
    fn parallel_tool_calls_grouped() {
        let dir = TempDir::new().unwrap();
        let path = write_events(dir.path(), vec![
            events::tool_call("id1".into(), "read".into(), &serde_json::json!({})),
            events::tool_call("id2".into(), "ls".into(), &serde_json::json!({})),
            events::tool_result("id1".into(), "r1".into(), None, None),
            events::tool_result("id2".into(), "r2".into(), None, None),
        ]);
        let msgs = messages_from_path(&path).unwrap();
        assert_eq!(msgs.len(), 2);
        if let Message::Assistant { content: AssistantContent::Parts(parts) } = &msgs[0] {
            assert_eq!(parts.len(), 2);
        } else { panic!("expected 2 tool calls in one assistant message"); }
        if let Message::Tool { content } = &msgs[1] {
            assert_eq!(content.len(), 2);
        } else { panic!("expected tool message with 2 results"); }
    }

    #[test]
    fn permissions_excluded_from_context_reconstruction() {
        let bodies = vec![
            events::user_turn("do x".into(), None, None),
            events::permissions(&[PermissionItem {
                id: "tc1".into(), cmd: "shell".into(), reason: "test".into(),
                patterns: vec![], tool_name: "shell".into(),
                tool_args: serde_json::Value::Null,
            }]),
            events::permissions_reply("user".into(), &[PermissionsReplyItem {
                id: "tc1".into(), decision: "allow".into(), text: None, scope: None,
            }]),
        ];
        let messages = messages(&mint_events(bodies)).unwrap();
        assert_eq!(messages.len(), 1, "only UserTurn should appear; got: {} messages", messages.len());
        assert!(matches!(&messages[0], Message::User { .. }));
    }

    #[test]
    fn compaction_breakpoint_discards_earlier_events() {
        let dir = TempDir::new().unwrap();
        let usage = TurnUsage{input:1,output:1,cache_read:0,cache_write:0};
        let output = TurnOutput{message:"s".into(),result:None};
        let cu = CompactionUsage{input:10,output:5};
        let path = write_events(dir.path(), vec![
            events::user_turn("old".into(), None, None),
            events::assistant_text("old response".into()),
            events::turn_complete(&usage, "h".into(), false, None, vec![], &output),
            events::summary("summary of old stuff".into(), 1, &cu),
            events::user_turn("new".into(), None, None),
        ]);
        let msgs = messages_from_path(&path).unwrap();
        // Should have: compaction summary + "new" user turn
        assert_eq!(msgs.len(), 2, "got: {:?}", msgs.len());
        assert!(matches!(&msgs[0], Message::User { content: MessageContent::Text(t) } if t.contains("summary of old stuff")));
        assert!(matches!(&msgs[1], Message::User { content: MessageContent::Text(t) } if t == "new"));
    }

    #[test]
    fn enabled_loadables_seeds_from_config_and_replays_toggles() {
        use std::collections::HashSet;
        let config_enabled: HashSet<String> = ["tdd".into()].into();
        let bodies = vec![
            events::toggle("mcp", "slack_send", true),
            events::toggle("skill", "tdd", false),
        ];
        let seed = AgentState { skills: config_enabled, mcp: HashSet::new(), ..Default::default() };
        let st = active_agent(&mint_events(bodies), seed);
        let enabled = st.enabled_loadables();
        assert!(enabled.contains("slack_send"), "slack should be enabled after load");
        assert!(!enabled.contains("tdd"), "tdd should be disabled after unload (LWW per entity)");
    }

    #[test]
    fn both_projections_derive_from_one_event_log() {
        let bodies = vec![
            events::user_turn("hi".into(), None, None),
            events::toggle("mcp", "slack_send", true),
        ];
        let proto_events = mint_events(bodies);
        let msgs = messages(&proto_events).unwrap();
        assert!(matches!(&msgs[0], Message::User { content: MessageContent::Text(t) } if t == "hi"));
        let enabled = active_agent(&proto_events, AgentState::default()).enabled_loadables();
        assert!(enabled.contains("slack_send"));
    }

    #[test]
    fn active_agent_checkpoint_returns_last_agent() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("tx.jsonl");

        // Write two Agent events — active_agent_checkpoint should return the second.
        wire::append(&path, wire::body(pb::payload::Kind::Agent(pb::Agent {
            model: "model-a".into(),
            tools: vec![], mcp_servers: vec![], skills: vec![], plugins: vec![],
            permissions: Some(pb::PermissionPolicy { allow: vec![], deny: vec![] }),
            instructions: String::new(), through_seq: 0,
        }))).unwrap();
        wire::append(&path, wire::body(pb::payload::Kind::Agent(pb::Agent {
            model: "model-b".into(),
            tools: vec![], mcp_servers: vec![], skills: vec![], plugins: vec![],
            permissions: Some(pb::PermissionPolicy { allow: vec![], deny: vec![] }),
            instructions: String::new(), through_seq: 0,
        }))).unwrap();

        let all_events = read_events(&path).unwrap();
        let r = active_agent_checkpoint(&all_events).expect("an agent");
        assert_eq!(r.model, "model-b");
    }

    #[test]
    fn active_agent_checkpoint_returns_none_for_no_agent() {
        let bodies = vec![
            events::user_turn("hi".into(), None, None),
            events::assistant_text("hello".into()),
        ];
        let proto_events = mint_events(bodies);
        assert!(active_agent_checkpoint(&proto_events).is_none());
    }

    #[test]
    fn active_cwd_defaults_to_root() {
        let root = std::path::Path::new("/repo/root");
        let evs = mint_events(vec![events::assistant_text("no cwd here".into())]);
        assert_eq!(active_cwd(&evs, root), root.to_path_buf());
    }

    #[test]
    fn active_cwd_last_write_wins() {
        let root = std::path::Path::new("/repo/root");
        let evs = mint_events(vec![
            events::cwd("/a"),
            events::cwd("/b"),
        ]);
        assert_eq!(active_cwd(&evs, root), std::path::PathBuf::from("/b"));
    }

    // ── AgentState + conversions (T1 + T2) ──────────────────────────────────────

    #[test]
    fn agent_state_from_pb_agent_maps_enabled_sets() {
        let a = pb::Agent {
            model: "x/y".into(),
            instructions: "sys".into(),
            tools: vec!["read".into(), "edit".into()],
            mcp_servers: vec!["slack".into()],
            skills: vec!["tdd".into()],
            plugins: vec![],
            permissions: Some(pb::PermissionPolicy {
                allow: vec!["read".into()], deny: vec!["shell:rm *".into()] }),
            through_seq: 0,
        };
        let st = AgentState::from(&a);
        assert_eq!(st.model, "x/y");
        assert_eq!(st.instructions, "sys");
        assert!(st.skills.contains("tdd"));
        assert!(st.mcp.contains("slack"));
        assert!(st.tools.contains("read") && st.tools.contains("edit"));
        assert!(st.allow.contains("read"));
        assert!(st.deny.contains("shell:rm *"));
    }

    #[test]
    fn agent_state_from_config_seeds_enabled_from_config() {
        use crate::config::{GaitConfig, McpServerDef, SkillDef};
        let cfg = GaitConfig {
            model: "anthropic/x".into(),
            mcp_servers: vec![
                McpServerDef { name: "slack".into(), command: None, args: None, env: None,
                    url: None, spawn: None, connect: None, enabled: true, description: None },
                McpServerDef { name: "off".into(), command: None, args: None, env: None,
                    url: None, spawn: None, connect: None, enabled: false, description: None },
            ],
            skills: vec![SkillDef { name: "tdd".into(), description: "d".into(),
                content: "c".into(), enabled: true }],
            ..GaitConfig::default()
        };
        let st = AgentState::from_config(&cfg, "anthropic/x".into(), "sys".into(),
            vec!["read".into()], vec!["shell:rm *".into()]);
        assert!(st.mcp.contains("slack"), "enabled mcp seeded");
        assert!(!st.mcp.contains("off"), "disabled mcp NOT seeded");
        assert!(st.skills.contains("tdd"), "enabled skill seeded");
        assert_eq!(st.model, "anthropic/x");
        assert!(st.allow.contains("read") && st.deny.contains("shell:rm *"));
    }

    // ── ActiveAgent projection (T3 + T4) ────────────────────────────────────────

    #[test]
    fn active_agent_folds_load_toolcall_like_loadables() {
        use std::collections::HashMap;
        let seed = AgentState::from_config(
            &crate::config::GaitConfig::default(),
            "m".into(), "sys".into(), vec![], vec![]);
        let evs = mint_events(vec![
            events::toggle("mcp", "slack", true),
            events::toggle("mcp", "slack", false),
            events::toggle("skill", "tdd", true),
        ]);
        let st = run_projection(&ActiveAgent { seed }, &evs, &HashMap::new());
        let names = st.enabled_loadables();
        assert!(!names.contains("slack"), "LWW: slack toggled off");
        assert!(names.contains("tdd"), "tdd toggled on");
    }

    #[test]
    fn active_agent_checkpoint_resets_baseline() {
        use std::collections::HashMap;
        let seed = AgentState::from_config(
            &crate::config::GaitConfig::default(), "m".into(), "sys".into(), vec![], vec![]);
        let evs = mint_events(vec![
            events::toggle("skill", "old", true),
            wire::body(payload::Kind::Agent(pb::Agent {
                model: "m2".into(), instructions: "sys2".into(),
                tools: vec![], mcp_servers: vec!["slack".into()], skills: vec![], plugins: vec![],
                permissions: Some(pb::PermissionPolicy { allow: vec![], deny: vec![] }),
                through_seq: 0,
            })),
            events::toggle("skill", "tdd", true),
        ]);
        let st = run_projection(&ActiveAgent { seed }, &evs, &HashMap::new());
        assert_eq!(st.model, "m2", "checkpoint reset the model");
        let names = st.enabled_loadables();
        assert!(!names.contains("old"), "pre-checkpoint load wiped by Agent reset");
        assert!(names.contains("slack"), "checkpoint's mcp present");
        assert!(names.contains("tdd"), "post-checkpoint load folded");
    }

    #[test]
    fn active_agent_honors_mutations_redacted_load_dropped() {
        use std::collections::HashMap;
        let seed = AgentState::from_config(
            &crate::config::GaitConfig::default(), "m".into(), "sys".into(), vec![], vec![]);
        let one = mint_events(vec![
            events::toggle("mcp", "slack", true),
        ]);
        let mut muts = HashMap::new();
        muts.insert(one[0].id.clone(), MutationState::Redacted { reason: None });
        let st = run_projection(&ActiveAgent { seed }, &one, &muts);
        assert!(!st.enabled_loadables().contains("slack"), "redacted load disabled");
    }

    #[test]
    fn active_agent_projection_checkpoint_is_last_write_wins() {
        use std::collections::HashMap;
        let seed = AgentState::default();
        let evs = mint_events(vec![
            wire::body(payload::Kind::Agent(pb::Agent { model: "m1".into(), ..Default::default() })),
            wire::body(payload::Kind::Agent(pb::Agent { model: "m2".into(), ..Default::default() })),
        ]);
        let got = run_projection(&ActiveAgent { seed }, &evs, &HashMap::new());
        assert_eq!(got.model, "m2", "latest Agent checkpoint wins");
    }

    #[test]
    fn fold_returns_all_three_projections() {
        let dir = TempDir::new().unwrap();
        let path = write_events(dir.path(), vec![
            wire::body(pb::payload::Kind::Agent(pb::Agent {
                model: "gpt-fold".into(),
                tools: vec![], mcp_servers: vec![], skills: vec![], plugins: vec![],
                permissions: Some(pb::PermissionPolicy { allow: vec![], deny: vec![] }),
                instructions: String::new(), through_seq: 0,
            })),
            events::user_turn("hi".into(), None, None),
            events::toggle("skill", "tdd", true),
        ]);
        let all_events = read_events(&path).unwrap();
        let seed = AgentState::from_config(&crate::config::GaitConfig::default(),
            "m".into(), "".into(), vec![], vec![]);
        let result = fold(&all_events, seed).unwrap();

        // messages: user turn (no summary-only events, user_turn becomes a Message)
        assert!(!result.messages.is_empty());
        // agent.model: the checkpoint we wrote
        assert_eq!(result.agent.model, "gpt-fold");
        // enabled_loadables: tdd was toggled on
        assert!(result.agent.enabled_loadables().contains("tdd"));
    }

    #[test]
    fn fold_runs_all_projections_single_pass() {
        let evs = mint_events(vec![
            events::toggle("mcp", "slack", true),
        ]);
        let seed = AgentState::from_config(&crate::config::GaitConfig::default(),
            "m".into(), "".into(), vec![], vec![]);
        let res = fold(&evs, seed).unwrap();
        assert!(res.agent.enabled_loadables().contains("slack"));
        assert!(res.effective_allow.is_empty(), "permission stub yields no grants yet");
    }

    #[test]
    fn active_messages_coalesces_consecutive_tool_calls_and_results() {
        use std::collections::HashMap;
        // Two consecutive tool_calls then two consecutive tool_results must render
        // as ONE assistant(Parts[2]) message followed by ONE tool message[2].
        let evs = mint_events(vec![
            events::tool_call("c1".into(), "shell".into(), &serde_json::json!({"cmd":"ls"})),
            events::tool_call("c2".into(), "shell".into(), &serde_json::json!({"cmd":"pwd"})),
            events::tool_result("c1".into(), "a".into(), None, None),
            events::tool_result("c2".into(), "b".into(), None, None),
        ]);
        let msgs = run_projection(&ActiveMessages, &evs, &HashMap::new()).msgs;
        assert_eq!(msgs.len(), 2, "one assistant parts msg + one tool msg");
        match &msgs[0] {
            Message::Assistant { content: AssistantContent::Parts(parts) } =>
                assert_eq!(parts.len(), 2, "two tool calls coalesced into one assistant message"),
            other => panic!("expected Assistant Parts, got {other:?}"),
        }
        match &msgs[1] {
            Message::Tool { content } =>
                assert_eq!(content.len(), 2, "two tool results coalesced into one tool message"),
            other => panic!("expected Tool message, got {other:?}"),
        }
    }

    #[test]
    fn active_messages_resets_on_compaction() {
        use std::collections::HashMap;
        // A user turn before a compaction must be cleared; only the summary +
        // anything after survive.
        let cu = CompactionUsage { input: 1, output: 1 };
        let evs = mint_events(vec![
            events::user_turn("before".into(), None, None),
            events::summary("the summary".into(), 1, &cu),
            events::user_turn("after".into(), None, None),
        ]);
        let msgs = run_projection(&ActiveMessages, &evs, &HashMap::new()).msgs;
        assert_eq!(msgs.len(), 2, "summary + post-compaction turn (pre-compaction cleared)");
        assert!(matches!(&msgs[0],
            Message::User { content: MessageContent::Text(t) } if t.contains("the summary")));
        assert!(matches!(&msgs[1],
            Message::User { content: MessageContent::Text(t) } if t == "after"));
    }

    #[test]
    fn active_messages_two_compactions_second_wins() {
        use std::collections::HashMap;
        // Two Compaction events in one log. Each resets the message context to its
        // own summary; the SECOND summary replaces the first (positional reset, C1).
        let cu = CompactionUsage { input: 1, output: 1 };
        let evs = mint_events(vec![
            events::user_turn("before-first".into(), None, None),
            events::summary("first summary".into(), 1, &cu),
            events::user_turn("between".into(), None, None),
            events::summary("second summary".into(), 1, &cu),
            events::user_turn("after-second".into(), None, None),
        ]);
        let msgs = run_projection(&ActiveMessages, &evs, &HashMap::new()).msgs;
        // Only the second summary + the turn after it survive; the first summary and
        // everything before the second compaction are gone.
        assert_eq!(msgs.len(), 2, "second summary + post-second turn, got: {msgs:?}");
        assert!(matches!(&msgs[0],
            Message::User { content: MessageContent::Text(t) }
            if t.contains("second summary") && !t.contains("first summary")),
            "second compaction's summary replaces the first");
        assert!(matches!(&msgs[1],
            Message::User { content: MessageContent::Text(t) } if t == "after-second"));
    }

    #[test]
    fn permissions_session_persists_once_expires_on_turn_complete() {
        use std::collections::{HashMap, HashSet};
        let evs = mint_events(vec![
            events::permissions_reply("user".into(), &[PermissionsReplyItem {
                id: "1".into(), decision: "allow".into(),
                text: Some("shell:cargo test".into()), scope: Some("session".into()) }]),
            events::permissions_reply("user".into(), &[PermissionsReplyItem {
                id: "2".into(), decision: "allow".into(),
                text: Some("shell:ls".into()), scope: Some("once".into()) }]),
            wire::body(payload::Kind::TurnComplete(pb::TurnComplete::default())),
        ]);
        let st = run_projection(&ActivePermissions { seed: HashSet::new() }, &evs, &HashMap::new());
        assert!(st.session.contains("shell:cargo test"), "session grant persists past TurnComplete");
        assert!(!st.once.contains("shell:ls"), "once grant expires at TurnComplete");
    }

    #[test]
    fn permissions_session_deny_revokes_but_ledger_keeps_both() {
        use std::collections::{HashMap, HashSet};
        let evs = mint_events(vec![
            events::permissions_reply("user".into(), &[PermissionsReplyItem {
                id: "1".into(), decision: "allow".into(),
                text: Some("shell:rm".into()), scope: Some("session".into()) }]),
            events::permissions_reply("user".into(), &[PermissionsReplyItem {
                id: "2".into(), decision: "deny".into(),
                text: Some("shell:rm".into()), scope: Some("session".into()) }]),
        ]);
        // Append-only ledger: both the approve and the deny remain on disk.
        assert_eq!(evs.len(), 2, "approve and deny both stay in the log");
        let st = run_projection(&ActivePermissions { seed: HashSet::new() }, &evs, &HashMap::new());
        assert!(!st.session.contains("shell:rm"), "forward deny revokes from the derived set");
    }

    #[test]
    fn fold_effective_allow_includes_session_grant() {
        let evs = mint_events(vec![
            events::permissions_reply("user".into(), &[PermissionsReplyItem {
                id: "1".into(), decision: "allow".into(),
                text: Some("shell:git status".into()), scope: Some("session".into()) }]),
        ]);
        let res = fold(&evs, AgentState::default()).unwrap();
        assert!(res.effective_allow.contains("shell:git status"),
            "fold flattens session grants into effective_allow");
    }

    #[test]
    fn effective_kinds_skips_redacted_and_keeps_others() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("tx.jsonl");

        // An enable tool_call (will be redacted) + an assistant_text (kept).
        let tc = wire::append(&path, events::tool_call(
            "tc1".into(), "enable".into(),
            &serde_json::json!({"name":"slack","enabled":true}))).unwrap();
        wire::append(&path, redact_body(&tc.id)).unwrap();
        wire::append(&path, events::assistant_text("hi".into())).unwrap();

        let all = read_events(&path).unwrap();
        let mut muts = active_mutations(&all);
        apply_cascade(&mut muts, &all);

        let kinds: Vec<_> = effective_kinds(&all, &muts).collect();
        // redacted enable must NOT appear:
        assert!(kinds.iter().all(|(_, k)|
            !matches!(k, payload::Kind::ToolCall(tc) if tc.name == "enable")),
            "redacted enable must not appear in effective kinds");
        // the non-redacted assistant_text MUST appear:
        assert!(kinds.iter().any(|(_, k)| matches!(k, payload::Kind::AssistantText(_))),
            "non-redacted event must appear in effective kinds");
    }

    // --- Phase 4 mutation tests ---

    use crate::proto::gait::log::v1::{Redact, Replace};

    fn redact_body(target: &str) -> event::Body {
        event::Body::Redact(Redact { target: target.into(), reason: None })
    }

    fn replace_body(target: &str, new_payload: pb::Payload) -> event::Body {
        event::Body::Replace(Replace {
            target: target.into(),
            replacement: Some(new_payload),
            reason: None,
        })
    }

    #[test]
    fn redact_removes_event_from_messages() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("tx.jsonl");

        // Write a UserTurn, then a Redact targeting it.
        let user_event = wire::append(&path, events::user_turn("hello".into(), None, None)).unwrap();
        wire::append(&path, redact_body(&user_event.id)).unwrap();

        let all = read_events(&path).unwrap();
        let mut muts = active_mutations(&all);
        apply_cascade(&mut muts, &all);
        let msgs = run_projection(&ActiveMessages, &all, &muts).msgs;

        assert!(msgs.is_empty(), "redacted UserTurn should not appear in messages");
    }

    #[test]
    fn replace_substitutes_payload_in_messages() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("tx.jsonl");

        let user_event = wire::append(&path, events::user_turn("old text".into(), None, None)).unwrap();
        let new_payload = pb::Payload {
            kind: Some(payload::Kind::UserTurn(pb::UserTurn {
                content: "new text".into(),
                ..Default::default()
            })),
        };
        wire::append(&path, replace_body(&user_event.id, new_payload)).unwrap();

        let all = read_events(&path).unwrap();
        let mut muts = active_mutations(&all);
        apply_cascade(&mut muts, &all);
        let msgs = run_projection(&ActiveMessages, &all, &muts).msgs;

        assert_eq!(msgs.len(), 1);
        assert!(matches!(&msgs[0], Message::User { content: MessageContent::Text(t) } if t == "new text"),
            "replacement payload should be used");
    }

    #[test]
    fn later_redact_overrides_replace() {
        // Replace first, then Redact on same target — Redact has higher seq so it wins.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("tx.jsonl");

        let user_event = wire::append(&path, events::user_turn("original".into(), None, None)).unwrap();
        let new_payload = pb::Payload {
            kind: Some(payload::Kind::UserTurn(pb::UserTurn {
                content: "replaced".into(),
                ..Default::default()
            })),
        };
        wire::append(&path, replace_body(&user_event.id, new_payload)).unwrap();
        wire::append(&path, redact_body(&user_event.id)).unwrap(); // later → wins

        let all = read_events(&path).unwrap();
        let muts = active_mutations(&all);
        // LWW winner for user_event.id should be Redacted.
        assert!(matches!(muts.get(&user_event.id), Some(MutationState::Redacted { .. })),
            "Redact (higher seq) must win over earlier Replace");
    }

    #[test]
    fn cascade_redact_tool_call_removes_paired_result() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("tx.jsonl");

        let tc_event = wire::append(&path, events::tool_call("cid1".into(), "read".into(), &serde_json::json!({}))).unwrap();
        wire::append(&path, events::tool_result("cid1".into(), "output".into(), None, None)).unwrap();
        wire::append(&path, redact_body(&tc_event.id)).unwrap(); // Redact the ToolCall

        let all = read_events(&path).unwrap();
        let mut muts = active_mutations(&all);
        apply_cascade(&mut muts, &all);

        // Both ToolCall and ToolResult should be Redacted.
        assert_eq!(muts.len(), 2, "cascade should produce 2 Redacts");
        assert!(muts.values().all(|m| matches!(m, MutationState::Redacted { .. })));

        let msgs = run_projection(&ActiveMessages, &all, &muts).msgs;
        assert!(msgs.is_empty(), "both paired events redacted → no messages");
    }

    #[test]
    fn cascade_redact_overrides_orphan_replace() {
        // Redact ToolCall, Replace ToolResult — cascade Redact should win over the Replace.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("tx.jsonl");

        let tc_event = wire::append(&path, events::tool_call("cid2".into(), "shell".into(), &serde_json::json!({}))).unwrap();
        let tr_event = wire::append(&path, events::tool_result("cid2".into(), "output".into(), None, None)).unwrap();
        wire::append(&path, redact_body(&tc_event.id)).unwrap();     // Redact ToolCall
        // Replace ToolResult (same seq-space, but cascade must override).
        let new_tr = pb::Payload {
            kind: Some(payload::Kind::ToolResult(pb::ToolResult {
                call_id: "cid2".into(),
                output: "new output".into(),
                ..Default::default()
            })),
        };
        wire::append(&path, replace_body(&tr_event.id, new_tr)).unwrap();

        let all = read_events(&path).unwrap();
        let mut muts = active_mutations(&all);
        // Before cascade: ToolCall=Redacted, ToolResult=Replaced.
        apply_cascade(&mut muts, &all);
        // After cascade: ToolResult must become Redacted (cascade overrides Replace).
        assert!(matches!(muts.get(&tr_event.id), Some(MutationState::Redacted { .. })),
            "cascade Redact must override orphan Replace on ToolResult");
    }

    #[test]
    fn fold_mutations_included_in_result() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("tx.jsonl");

        let e = wire::append(&path, events::user_turn("hi".into(), None, None)).unwrap();
        wire::append(&path, redact_body(&e.id)).unwrap();

        let all = read_events(&path).unwrap();
        let result = fold(&all, AgentState::default()).unwrap();

        assert!(!result.mutations.is_empty(), "fold must populate mutations");
        assert!(result.messages.is_empty(), "redacted event must not appear in fold messages");
    }
    #[test]
    fn loadables_projection_full_session_and_mutation_aware() {
        use std::collections::HashMap;
        // A load, then a compaction AFTER it — the projection must NOT reset (full session).
        let cu = CompactionUsage { input: 1, output: 1 };
        let with_comp = mint_events(vec![
            events::toggle("mcp", "slack", true),
            events::summary("summary".into(), 1, &cu),
        ]);
        let p = ActiveAgent { seed: AgentState::default() };
        let got: AgentState = run_projection(&p, &with_comp, &HashMap::new());
        assert!(got.enabled_loadables().contains("slack"), "load before compaction must stay enabled (full session)");

        // Now redact that load → projection must drop it (honors mutations).
        let one = mint_events(vec![
            events::toggle("mcp", "slack", true),
        ]);
        let mut muts = HashMap::new();
        muts.insert(one[0].id.clone(), MutationState::Redacted { reason: None });
        let got2: AgentState = run_projection(&p, &one, &muts);
        assert!(!got2.enabled_loadables().contains("slack"), "redacted/undone load must be disabled");
    }
    #[test]
    fn last_turn_blocks_returns_last_complete() {
        // Two TurnComplete events with different blocks; the helper must return
        // the second (last) one's blocks, in order.
        let usage = TurnUsage { input: 1, output: 1, cache_read: 0, cache_write: 0 };
        let output = TurnOutput { message: "ok".into(), result: None };
        let first = vec!["alpha".to_string(), "beta".to_string()];
        let second = vec!["gamma".to_string(), "delta".to_string(), "epsilon".to_string()];
        let evs = mint_events(vec![
            events::turn_complete(&usage, "h1".into(), false, None, first.clone(), &output),
            events::turn_complete(&usage, "h2".into(), false, None, second.clone(), &output),
        ]);
        let got = last_turn_blocks(&evs);
        assert_eq!(got, second, "last TurnComplete wins");
    }

    #[test]
    fn last_turn_blocks_empty_when_none() {
        // No TurnComplete events in the log → empty Vec.
        let evs = mint_events(vec![
            events::user_turn("hi".into(), None, None),
            events::assistant_text("hello".into()),
        ]);
        assert!(last_turn_blocks(&evs).is_empty());
    }

    #[test]
    fn active_agent_folds_toggle_per_kind_and_model_change() {
        use std::collections::HashMap;
        use crate::proto::gait::log::v1::ToggleKind;
        let seed = AgentState::from_config(
            &crate::config::GaitConfig::default(), "m".into(), "sys".into(), vec![], vec![]);
        let evs = mint_events(vec![
            wire::body(payload::Kind::Toggle(pb::Toggle {
                kind: ToggleKind::Skill as i32, name: "tdd".into(), enabled: true })),
            wire::body(payload::Kind::Toggle(pb::Toggle {
                kind: ToggleKind::Mcp as i32, name: "slack".into(), enabled: true })),
            wire::body(payload::Kind::Toggle(pb::Toggle {
                kind: ToggleKind::Skill as i32, name: "tdd".into(), enabled: false })),
            wire::body(payload::Kind::ModelChange(pb::ModelChange { model: "m2".into() })),
        ]);
        let st = run_projection(&ActiveAgent { seed }, &evs, &HashMap::new());
        assert!(!st.skills.contains("tdd"), "skill toggled off");
        assert!(st.mcp.contains("slack"), "mcp toggled on in mcp set");
        assert!(!st.skills.contains("slack"), "mcp toggle did NOT leak into skills");
        assert_eq!(st.model, "m2", "ModelChange LWW");
    }

    #[test]
    fn rebaseline_agent_overlays_skills_and_mcp_from_state() {
        use std::collections::HashSet;
        let live = pb::Agent {
            model: "m1".into(),
            instructions: "sys".into(),
            tools: vec!["read".into()],
            mcp_servers: vec!["old-mcp".into()],
            skills: vec!["old-skill".into()],
            plugins: vec![],
            permissions: Some(pb::PermissionPolicy { allow: vec![], deny: vec![] }),
            through_seq: 0,
        };
        let st = AgentState {
            model: "m2".into(), // model from state is NOT used — static field from live
            instructions: String::new(),
            tools: HashSet::new(),
            skills: ["new-skill".to_string()].into(),
            mcp: ["new-mcp".to_string()].into(),
            plugins: HashSet::new(),
            allow: HashSet::new(),
            deny: HashSet::new(),
        };
        let got = rebaseline_agent(live.clone(), &st);
        assert_eq!(got.model, "m1", "model is static — from live, not state");
        assert_eq!(got.instructions, "sys", "instructions are static");
        assert_eq!(got.tools, vec!["read".to_string()], "tools are static");
        assert_eq!(got.skills, vec!["new-skill".to_string()], "skills overlaid from state");
        assert_eq!(got.mcp_servers, vec!["new-mcp".to_string()], "mcp overlaid from state");
        let perms = got.permissions.as_ref().expect("permissions preserved");
        assert!(perms.allow.is_empty());
    }

    #[test]
    fn plugin_toggle_folds_into_agent_state() {
        let events = vec![
            events::toggle("plugin", "myfmt", true),
            events::toggle("plugin", "other", true),
            events::toggle("plugin", "myfmt", false),
        ];
        let evs = mint_events(events);
        let state = active_agent(&evs, AgentState::default());
        assert!(!state.plugins.contains("myfmt"), "LWW: myfmt toggled off");
        assert!(state.plugins.contains("other"), "other toggled on");
    }

    #[test]
    fn plugin_enabled_appears_in_enabled_loadables() {
        let events = vec![
            events::toggle("plugin", "myfmt", true),
        ];
        let evs = mint_events(events);
        let state = active_agent(&evs, AgentState::default());
        let enabled = state.enabled_loadables();
        assert!(enabled.contains("myfmt"), "plugin in enabled_loadables");
    }

}


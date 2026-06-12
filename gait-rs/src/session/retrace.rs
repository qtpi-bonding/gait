//! # Retrace — the `Log → working-state` fold
//!
//! Folds the immutable event `Log` into working state via [`fold`] (all projections)
//! or individual projection functions:
//!
//! - [`messages`] / [`messages_from_path`] — the **Transcript** (`Vec<Message>`).
//!   Honours `Compaction` breakpoints and `Redact`/`Replace` mutations.
//! - [`enabled_loadables`] — active loadable set (LWW-Map over `load` tool calls).
//! - [`active_manifest`] — last `Manifest` payload (LWW-Register).
//! - [`active_mutations`] + [`apply_cascade`] — `Redact`/`Replace` LWW fold with
//!   ToolCall↔ToolResult cascade.

use std::collections::HashMap;
use std::path::Path;
use crate::types::{Message, MessageContent, AssistantContent, AssistantPart, ToolCall, ToolResult, ToolResultKind};
use crate::session::transcript::{read_events, last_compaction_index};
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
    messages_applying(all_events, &HashMap::new())
}

pub fn messages_from_path(path: &Path) -> anyhow::Result<Vec<Message>> {
    let all_events = read_events(path)?;
    messages_applying(&all_events, &HashMap::new())
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
        p.apply(&mut state, ev, kind);
    }
    p.finish(state)
}

/// Active loadable names. Seeds from config defaults; replays `load(name, enabled)`
/// tool calls as last-write-wins toggles. Folds the FULL session (compaction does
/// not reset it) and honors mutations (an undone load disables it).
pub(crate) struct ActiveLoadables {
    pub seed: std::collections::HashSet<String>,
}
impl Projection for ActiveLoadables {
    type State = std::collections::HashSet<String>;
    fn seed(&self) -> Self::State {
        self.seed.clone()
    }
    fn apply(&self, state: &mut Self::State, _ev: &Event, kind: &payload::Kind) {
        if let payload::Kind::ToolCall(tc) = kind {
            if tc.name == "load" {
                let input = struct_to_value(&tc.input);
                let name = input.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let on = input.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false);
                if on { state.insert(name); } else { state.remove(&name); }
            }
        }
    }
}

/// Last-write-wins register over `Manifest` events.
pub(crate) struct ActiveManifest;
impl Projection for ActiveManifest {
    type State = Option<pb::Manifest>;
    fn seed(&self) -> Self::State {
        None
    }
    fn apply(&self, state: &mut Self::State, _ev: &Event, kind: &payload::Kind) {
        if let payload::Kind::Manifest(r) = kind {
            *state = Some(r.clone());
        }
    }
}

/// Build messages, applying the given mutations map.
/// This is the internal implementation that both `messages` and `fold` use.
fn messages_applying(
    all_events: &[Event],
    mutations: &HashMap<String, MutationState>,
) -> anyhow::Result<Vec<Message>> {
    if all_events.is_empty() { return Ok(vec![]); }

    let start = last_compaction_index(all_events).unwrap_or(0);
    let events = &all_events[start..];

    let mut messages: Vec<Message> = vec![];
    let mut i = 0;

    while i < events.len() {
        // Skip Redacted events (check before calling effective_kind).
        if matches!(mutations.get(&events[i].id), Some(MutationState::Redacted { .. })) {
            i += 1;
            continue;
        }

        match effective_kind(&events[i], mutations) {
            Some(payload::Kind::Compaction(c)) => {
                messages.push(Message::User {
                    content: MessageContent::Text(
                        format!("[Session history summary: {}]", c.summary)
                    ),
                });
                i += 1;
            }
            Some(payload::Kind::SessionStart(_)) => { i += 1; }

            Some(payload::Kind::AgentUpdate(au)) => {
                let status = crate::proto::gait::log::v1::AgentStatus::try_from(au.status)
                    .map(|s| match s {
                        crate::proto::gait::log::v1::AgentStatus::Done => "done",
                        crate::proto::gait::log::v1::AgentStatus::Clarify => "clarify",
                        crate::proto::gait::log::v1::AgentStatus::Inform => "inform",
                        crate::proto::gait::log::v1::AgentStatus::Unspecified => "unspecified",
                    })
                    .unwrap_or("unspecified");
                let mut content = format!("[agent:{}] status: {}\nsummary: {}", au.session_id, status, au.summary);
                if let Some(items) = &au.items {
                    let v = serde_json::to_value(items).unwrap_or(serde_json::Value::Null);
                    content.push_str(&format!("\nitems: {}", v));
                }
                messages.push(Message::User { content: MessageContent::Text(content) });
                i += 1;
            }

            Some(payload::Kind::UserTurn(u)) => {
                messages.push(Message::User { content: MessageContent::Text(u.content.clone()) });
                i += 1;
            }

            Some(payload::Kind::AssistantText(a)) => {
                messages.push(Message::Assistant { content: AssistantContent::Text(a.content.clone()) });
                i += 1;
            }

            Some(payload::Kind::ToolCall(_)) => {
                // Gather all consecutive tool_call events
                let mut calls: Vec<(String, String, serde_json::Value)> = vec![];
                while i < events.len() {
                    if matches!(mutations.get(&events[i].id), Some(MutationState::Redacted { .. })) {
                        i += 1;
                        continue;
                    }
                    if let Some(payload::Kind::ToolCall(tc)) = effective_kind(&events[i], mutations) {
                        calls.push((tc.call_id.clone(), tc.name.clone(), struct_to_value(&tc.input)));
                        i += 1;
                    } else { break; }
                }
                // Gather consecutive tool_result events
                let mut results: Vec<(String, String)> = vec![];
                while i < events.len() {
                    if matches!(mutations.get(&events[i].id), Some(MutationState::Redacted { .. })) {
                        i += 1;
                        continue;
                    }
                    if let Some(payload::Kind::ToolResult(tr)) = effective_kind(&events[i], mutations) {
                        results.push((tr.call_id.clone(), tr.output.clone()));
                        i += 1;
                    } else { break; }
                }

                let parts: Vec<AssistantPart> = calls.iter().map(|(id, name, args)| {
                    AssistantPart::ToolCall(ToolCall {
                        tool_call_id: id.clone(),
                        tool_name: name.clone(),
                        args: args.clone(),
                    })
                }).collect();
                messages.push(Message::Assistant { content: AssistantContent::Parts(parts) });

                if !results.is_empty() {
                    let tool_content: Vec<ToolResult> = results.iter().map(|(id, output)| {
                        ToolResult {
                            kind: ToolResultKind::ToolResult,
                            tool_call_id: id.clone(),
                            result: serde_json::Value::String(output.clone()),
                        }
                    }).collect();
                    messages.push(Message::Tool { content: tool_content });
                }
            }

            Some(payload::Kind::ToolResult(_)) => { i += 1; } // orphan — skip

            Some(payload::Kind::Clarify(c)) => {
                let text = c.items.iter().map(|q| {
                    format!("clarify ({}): {}\nwhy: {}", clarify_kind_str(q.kind), q.content, q.why)
                }).collect::<Vec<_>>().join("\n\n");
                messages.push(Message::Assistant { content: AssistantContent::Text(text) });
                i += 1;
            }

            Some(payload::Kind::ClarifyReply(cr)) => {
                let text = cr.items.iter().map(|r| {
                    let base = format!("{}: {}", r.id, r.content);
                    if let Some(reason) = &r.reasoning { format!("{}\nreason: {}", base, reason) }
                    else { base }
                }).collect::<Vec<_>>().join("\n");
                messages.push(Message::User { content: MessageContent::Text(text) });
                i += 1;
            }

            Some(payload::Kind::ClarifyConvergence(cc)) => {
                for item in &cc.items {
                    messages.push(Message::Assistant { content: AssistantContent::Text(format!("[proposal] {}", item.proposal)) });
                    if let Some(resp) = &item.response {
                        messages.push(Message::User { content: MessageContent::Text(format!("[response] {}", resp)) });
                    }
                }
                i += 1;
            }

            Some(payload::Kind::Inform(inf)) => {
                let text = inf.items.iter().map(|it| {
                    let mut s = it.content.clone();
                    if let Some(ctx) = &it.context { s.push_str(&format!("\ncontext: {}", ctx)); }
                    s
                }).collect::<Vec<_>>().join("\n\n");
                messages.push(Message::Assistant { content: AssistantContent::Text(text) });
                i += 1;
            }

            Some(payload::Kind::InformReply(ir)) => {
                let text = ir.items.iter().map(|r| format!("{}: {}", r.id, r.content)).collect::<Vec<_>>().join("\n");
                messages.push(Message::User { content: MessageContent::Text(text) });
                i += 1;
            }

            Some(payload::Kind::Permissions(_)) => { i += 1; }    // audit event — excluded from model context
            Some(payload::Kind::PermissionsReply(_)) => { i += 1; } // audit event — excluded from model context

            Some(payload::Kind::Error(e)) => {
                let phase = crate::proto::gait::log::v1::ErrorPhase::try_from(e.phase)
                    .map(|p| match p {
                        crate::proto::gait::log::v1::ErrorPhase::Llm => "llm",
                        crate::proto::gait::log::v1::ErrorPhase::Tool => "tool",
                        crate::proto::gait::log::v1::ErrorPhase::Git => "git",
                        crate::proto::gait::log::v1::ErrorPhase::Init => "init",
                        crate::proto::gait::log::v1::ErrorPhase::Unspecified => "unspecified",
                    })
                    .unwrap_or("unspecified");
                messages.push(Message::User { content: MessageContent::Text(format!("error during {}: {}", phase, e.message)) });
                i += 1;
            }

            // Skip: TurnComplete, Raw (workflow/etc.), InformConvergence, Manifest,
            // mutations (Redact/Replace) — not needed for LLM context.
            _ => { i += 1; }
        }
    }

    Ok(messages)
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

/// Derive which loadable names are currently enabled, from the Log.
/// Seeds from config defaults (`config_enabled`), then replays the transcript's
/// `load(name, enabled=…)` tool calls as last-write-wins toggles per entity.
/// Deprecated shim — prefer the `ActiveLoadables` projection via `fold`. Kept so
/// existing callers compile until Task 5 switches them to RetraceResult. Behaviour
/// is identical (empty mutation map → effective_kinds == raw kinds).
pub fn enabled_loadables(
    events: &[Event],
    config_enabled: &std::collections::HashSet<String>,
) -> std::collections::HashSet<String> {
    run_projection(
        &ActiveLoadables { seed: config_enabled.clone() },
        events,
        &HashMap::new(),
    )
}

/// Result of folding the full event log through all Retrace projections.
pub struct RetraceResult {
    /// The Transcript: Vec<Message> for the model context window.
    pub messages: Vec<Message>,
    /// Active loadable names (seeds from config, LWW-toggled by `load` tool calls).
    pub enabled_loadables: std::collections::HashSet<String>,
    /// The most recent `Manifest` payload, if any (LWW-Register).
    pub active_manifest: Option<pb::Manifest>,
    /// LWW-converged mutations (Redact/Replace) keyed by target event id,
    /// with ToolCall↔ToolResult cascade applied.
    pub mutations: HashMap<String, MutationState>,
}

/// Fold the full event log into all three projections in a single pass.
///
/// `config_enabled` is the baseline set of loadable names from config (used by
/// `enabled_loadables`). Pass `&HashSet::new()` if not relevant.
pub fn fold(
    all_events: &[Event],
    config_enabled: &std::collections::HashSet<String>,
) -> anyhow::Result<RetraceResult> {
    // Compute mutations (LWW + cascade) before building messages.
    let mut mutations = active_mutations(all_events);
    apply_cascade(&mut mutations, all_events);

    let messages = messages_applying(all_events, &mutations)?;

    let compaction_start = last_compaction_index(all_events).unwrap_or(0);
    let post_compaction = &all_events[compaction_start..];
    let enabled_loadables = enabled_loadables(post_compaction, config_enabled);

    let active_manifest = active_manifest(all_events);

    Ok(RetraceResult { messages, enabled_loadables, active_manifest, mutations })
}

/// Most recent `Manifest` payload (LWW-Register). Shim over the `ActiveManifest`
/// projection so existing callers (drift-guard in run_turn) are untouched.
pub fn active_manifest(events: &[Event]) -> Option<pb::Manifest> {
    run_projection(&ActiveManifest, events, &HashMap::new())
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
                id: "tc1".into(), decision: "approve".into(), text: None, scope: None,
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
            events::compaction("summary of old stuff".into(), 1, &cu),
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
            events::tool_call("1".into(), "load".into(), &serde_json::json!({"name": "slack_send", "enabled": true})),
            events::tool_call("2".into(), "load".into(), &serde_json::json!({"name": "tdd", "enabled": false})),
        ];
        let enabled = enabled_loadables(&mint_events(bodies), &config_enabled);
        assert!(enabled.contains("slack_send"), "slack should be enabled after load");
        assert!(!enabled.contains("tdd"), "tdd should be disabled after unload (LWW per entity)");
    }

    #[test]
    fn both_projections_derive_from_one_event_log() {
        use std::collections::HashSet;
        let bodies = vec![
            events::user_turn("hi".into(), None, None),
            events::tool_call("1".into(), "load".into(), &serde_json::json!({"name": "slack_send", "enabled": true})),
        ];
        let proto_events = mint_events(bodies);
        let msgs = messages(&proto_events).unwrap();
        assert!(matches!(&msgs[0], Message::User { content: MessageContent::Text(t) } if t == "hi"));
        let enabled = enabled_loadables(&proto_events, &HashSet::new());

        assert!(enabled.contains("slack_send"));
    }

    #[test]
    fn active_manifest_returns_last_manifest() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("tx.jsonl");

        // Write two Manifest events — active_manifest should return the second.
        wire::append(&path, wire::body(pb::payload::Kind::Manifest(pb::Manifest {
            model: "model-a".into(),
            tools: vec![], mcp_servers: vec![], skills: vec![],
            permissions: Some(pb::PermissionPolicy { allow: vec![], deny: vec![] }),
            instructions: String::new(), output_schema_retries: None,
        }))).unwrap();
        wire::append(&path, wire::body(pb::payload::Kind::Manifest(pb::Manifest {
            model: "model-b".into(),
            tools: vec![], mcp_servers: vec![], skills: vec![],
            permissions: Some(pb::PermissionPolicy { allow: vec![], deny: vec![] }),
            instructions: String::new(), output_schema_retries: None,
        }))).unwrap();

        let all_events = read_events(&path).unwrap();
        let r = active_manifest(&all_events).expect("a manifest");
        assert_eq!(r.model, "model-b");
    }

    #[test]
    fn active_manifest_returns_none_for_no_manifest() {
        let bodies = vec![
            events::user_turn("hi".into(), None, None),
            events::assistant_text("hello".into()),
        ];
        let proto_events = mint_events(bodies);
        assert!(active_manifest(&proto_events).is_none());
    }

    #[test]
    fn active_manifest_projection_is_last_write_wins() {
        use std::collections::HashMap;
        let evs = mint_events(vec![
            wire::body(payload::Kind::Manifest(pb::Manifest { model: "m1".into(), ..Default::default() })),
            wire::body(payload::Kind::Manifest(pb::Manifest { model: "m2".into(), ..Default::default() })),
        ]);
        let got = run_projection(&ActiveManifest, &evs, &HashMap::new());
        assert_eq!(got.expect("a manifest").model, "m2", "latest Manifest wins");
    }

    #[test]
    fn fold_returns_all_three_projections() {
        use std::collections::HashSet;
        let dir = TempDir::new().unwrap();
        let path = write_events(dir.path(), vec![
            wire::body(pb::payload::Kind::Manifest(pb::Manifest {
                model: "gpt-fold".into(),
                tools: vec![], mcp_servers: vec![], skills: vec![],
                permissions: Some(pb::PermissionPolicy { allow: vec![], deny: vec![] }),
                instructions: String::new(), output_schema_retries: None,
            })),
            events::user_turn("hi".into(), None, None),
            events::tool_call("1".into(), "load".into(), &serde_json::json!({"name": "tdd", "enabled": true})),
        ]);
        let all_events = read_events(&path).unwrap();
        let config: HashSet<String> = HashSet::new();
        let result = fold(&all_events, &config).unwrap();

        // messages: user turn (compaction_start == 0, no manifest-only, user_turn becomes a Message)
        assert!(!result.messages.is_empty());
        // active_manifest: the one we wrote
        assert_eq!(result.active_manifest.as_ref().map(|r| r.model.as_str()), Some("gpt-fold"));
        // enabled_loadables: tdd was toggled on
        assert!(result.enabled_loadables.contains("tdd"));
    }

    #[test]
    fn effective_kinds_skips_redacted_and_keeps_others() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("tx.jsonl");

        // A load tool_call (will be redacted) + an assistant_text (kept).
        let tc = wire::append(&path, events::tool_call(
            "tc1".into(), "load".into(),
            &serde_json::json!({"name":"slack","enabled":true}))).unwrap();
        wire::append(&path, redact_body(&tc.id)).unwrap();
        wire::append(&path, events::assistant_text("hi".into())).unwrap();

        let all = read_events(&path).unwrap();
        let mut muts = active_mutations(&all);
        apply_cascade(&mut muts, &all);

        let kinds: Vec<_> = effective_kinds(&all, &muts).collect();
        // redacted load must NOT appear:
        assert!(kinds.iter().all(|(_, k)|
            !matches!(k, payload::Kind::ToolCall(tc) if tc.name == "load")),
            "redacted load must not appear in effective kinds");
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
        let msgs = messages_applying(&all, &muts).unwrap();

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
        let msgs = messages_applying(&all, &muts).unwrap();

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

        let msgs = messages_applying(&all, &muts).unwrap();
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
        use std::collections::HashSet;
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("tx.jsonl");

        let e = wire::append(&path, events::user_turn("hi".into(), None, None)).unwrap();
        wire::append(&path, redact_body(&e.id)).unwrap();

        let all = read_events(&path).unwrap();
        let result = fold(&all, &HashSet::new()).unwrap();

        assert!(!result.mutations.is_empty(), "fold must populate mutations");
        assert!(result.messages.is_empty(), "redacted event must not appear in fold messages");
    }
    #[test]
    fn loadables_projection_full_session_and_mutation_aware() {
        use std::collections::{HashMap, HashSet};
        // A load, then a compaction AFTER it — the projection must NOT reset (full session).
        let cu = CompactionUsage { input: 1, output: 1 };
        let with_comp = mint_events(vec![
            events::tool_call("1".into(), "load".into(), &serde_json::json!({"name":"slack","enabled":true})),
            events::compaction("summary".into(), 1, &cu),
        ]);
        let p = ActiveLoadables { seed: HashSet::new() };
        let got = run_projection(&p, &with_comp, &HashMap::new());
        assert!(got.contains("slack"), "load before compaction must stay enabled (full session)");

        // Now redact that load → projection must drop it (honors mutations).
        let one = mint_events(vec![
            events::tool_call("1".into(), "load".into(), &serde_json::json!({"name":"slack","enabled":true})),
        ]);
        let mut muts = HashMap::new();
        muts.insert(one[0].id.clone(), MutationState::Redacted { reason: None });
        let got2 = run_projection(&p, &one, &muts);
        assert!(!got2.contains("slack"), "redacted/undone load must be disabled");
    }
}

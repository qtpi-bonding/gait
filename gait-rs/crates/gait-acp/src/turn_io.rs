//! Pure translation between gait proto events and ACP schema types, plus the
//! TurnSink/TurnSource port impls that drive one ACP-initiated turn.
//! See docs/plans/2026-07-10-acp-adapter-phase2-design.md.
use std::collections::HashMap;
use std::path::PathBuf;

use agent_client_protocol::schema::v1::{
    ContentBlock, ContentChunk, CurrentModeUpdate, MessageId, PermissionOption, PermissionOptionId,
    PermissionOptionKind, RequestPermissionOutcome, RequestPermissionRequest,
    SelectedPermissionOutcome, SessionId, SessionNotification, SessionUpdate, StopReason,
    TextContent, ToolCall as AcpToolCall, ToolCallId, ToolCallStatus, ToolCallUpdate,
    ToolCallUpdateFields,
};
use agent_client_protocol::{Client, ConnectionTo};
use gait_core::embed::{Diag, TurnSink, TurnSource, TurnStart, user_turn_events};
use gait_core::git::Repos;
use gait_core::proto::gait::log::v1::{
    Decision, Event, PermissionItem, RunResult, StreamItem, event, payload, stream_item,
};
use gait_core::session::log::{SessionState, session_state};
use gait_ops::permission::{GrantScope, ItemChoice};

/// Wrap ACP text into an `AgentMessageChunk` / `AgentThoughtChunk` body.
///
/// Each call is its own complete message (gait only streams settled events
/// over ACP today, never live deltas — see `AcpTurnSink::item`), so a fresh
/// UUID per call is correct, not a shortcut: `ContentChunk::message_id` must
/// be set for the v1->v2 conversion `acp-agui-adapter` depends on to succeed
/// (`IntoV2 for v1::ContentChunk` errors on `message_id: None`).
fn text_chunk(text: impl Into<String>) -> ContentChunk {
    ContentChunk::new(ContentBlock::Text(TextContent::new(text)))
        .message_id(MessageId::from(uuid::Uuid::new_v4().to_string()))
}

/// Translate a client-renderable payload to an ACP `session/update`.
///
/// Shared by live settled-event delivery and durable-history replay. Returns
/// `None` for control/session metadata and errors surfaced through requests.
pub fn kind_to_session_update(kind: &payload::Kind) -> Option<SessionUpdate> {
    match kind {
        payload::Kind::UserText(t) => Some(SessionUpdate::UserMessageChunk(text_chunk(
            t.content.clone(),
        ))),
        payload::Kind::IncomingMessage(m) => Some(SessionUpdate::UserMessageChunk(text_chunk(
            m.content.clone(),
        ))),
        payload::Kind::AssistantText(t) => Some(SessionUpdate::AgentMessageChunk(text_chunk(
            t.content.clone(),
        ))),
        payload::Kind::ThinkingBlock(t) => Some(SessionUpdate::AgentThoughtChunk(text_chunk(
            t.content.clone(),
        ))),
        payload::Kind::ToolCall(tc) => {
            // proto field is `call_id` (not `id`); `input` is an Option<Struct>,
            // so convert with to_value, not from_str.
            let mut call = AcpToolCall::new(ToolCallId::new(tc.call_id.clone()), tc.name.clone());
            call.raw_input = tc.input.as_ref().and_then(|s| serde_json::to_value(s).ok());
            Some(SessionUpdate::ToolCall(call))
        }
        payload::Kind::ToolResult(tr) => {
            let ok = tr.process.as_ref().is_none_or(|pr| pr.exit_code == 0);
            // status lives on ToolCallUpdateFields, and ToolCallUpdate::new takes
            // (id, fields) — two args.
            let fields = ToolCallUpdateFields::new().status(if ok {
                ToolCallStatus::Completed
            } else {
                ToolCallStatus::Failed
            });
            let upd = ToolCallUpdate::new(ToolCallId::new(tr.call_id.clone()), fields);
            Some(SessionUpdate::ToolCallUpdate(upd))
        }
        payload::Kind::Mode(m) => Some(SessionUpdate::CurrentModeUpdate(CurrentModeUpdate::new(
            m.name.clone(),
        ))),
        _ => None,
    }
}

/// Translate one *settled* gait event to an ACP `session/update`. Live text
/// deltas (`StreamItem::Delta`) are handled in the sink, not here.
pub fn event_to_session_update(ev: &Event) -> Option<SessionUpdate> {
    let Some(event::Body::Payload(p)) = &ev.body else {
        return None;
    };
    kind_to_session_update(p.kind.as_ref()?)
}

/// Render the mutation-applied durable history in source order.
///
/// `apply_cascade` is load-bearing: a redaction of either side of a tool
/// call/result pair hides its partner as well, matching the core transcript
/// fold. Replay intentionally does not run a brain or mutate the log.
pub fn replay_updates(events: &[Event]) -> Vec<SessionUpdate> {
    let mut mutations = gait_state::session::retrace::active_mutations(events);
    gait_state::session::retrace::apply_cascade(&mut mutations, events);
    gait_state::session::retrace::effective_kinds(events, &mutations)
        .filter_map(|(_, kind)| kind_to_session_update(kind))
        .collect()
}

/// Extract the prompt text from ACP content blocks (Phase 2 handles `Text`
/// blocks; image/audio/resource blocks are a marked seam for a later phase).
pub fn content_blocks_to_text(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect()
}

/// Map a finished turn to an ACP `StopReason`. `Err(msg)` means the prompt must
/// be resolved as a JSON-RPC error (an internal failure is not a `Refusal`, and
/// an interaction-required result is unsupported until Phase 3).
/// Emit one `PromptAccepted` event for a prompt that is now in the log.
pub(crate) fn emit_prompt_accepted(
    source_record_id: String,
    session_id: String,
    message_ids: Vec<String>,
    source: gait_events::gait::events::v1::PromptSource,
    author: Option<gait_events::Principal>,
) {
    concord_telemetry::event!(gait_events::gait::events::v1::PromptAccepted {
        source_record_id,
        session_id,
        message_count: message_ids.len() as u32,
        message_ids,
        source: source as i32,
        author,
    });
}

/// The `on_durable` hook of a direct (unattested) prompt. Each prompt gets its
/// own record id: a session id would repeat on every prompt of the session and
/// defeat per-record dedup.
pub(crate) fn direct_on_durable(session_id: String) -> gait_core::port::input::OnDurable {
    let record_id = uuid::Uuid::new_v4().to_string();
    Box::new(move || {
        emit_prompt_accepted(
            record_id.clone(),
            session_id,
            vec![record_id],
            gait_events::gait::events::v1::PromptSource::Direct,
            None,
        );
        Ok(())
    })
}

pub fn turn_result_to_stop_reason(r: &RunResult) -> Result<StopReason, String> {
    use gait_core::proto::gait::log::v1::turn_result::Outcome;
    match r.turns.last().and_then(|turn| turn.outcome.as_ref()) {
        Some(Outcome::Done(_)) => Ok(StopReason::EndTurn),
        Some(Outcome::Cancelled(_)) => Ok(StopReason::Cancelled),
        Some(Outcome::Failed(failed)) if failed.reason == "max_steps" => {
            Ok(StopReason::MaxTurnRequests)
        }
        Some(Outcome::Failed(failed)) => Err(failed.message.clone()),
        Some(Outcome::Pause(_)) => {
            // Permissions/Clarify/Inform/SandboxUnlock all resume via
            // AcpTurnSource::next() before a run ever finalizes into this arm for
            // the single-pending-item case. This arm is now reached only for:
            // multi-item Clarify/Inform batches (unsupported — see
            // pending_interaction's caller in next()), or any pause next()
            // couldn't resolve (transport error, protocol violation, cancellation
            // after the fact).
            Err("turn ended awaiting interaction, which is unsupported here".into())
        }
        None => Err("run produced no final turn outcome".into()),
    }
}

/// Build the singular `tool_call` field an ACP `session/request_permission`
/// carries. Title is the item's display command; id is the gait tool_call_id.
pub fn tool_call_update(item: &PermissionItem) -> ToolCallUpdate {
    ToolCallUpdate::new(
        ToolCallId::new(item.id.clone()),
        ToolCallUpdateFields::new().title(item.cmd.clone()),
    )
}

/// Bridge one tool call's segments to a flat ACP option list plus an in-scope
/// decode map (design §4.2/§4.3). `segs` all share one `tool_call_id`.
///
/// Options (per design):
///   - Allow once      -> each seg { Allow, Once, patterns[0] }        (kind AllowOnce)
///   - Allow session   -> each seg { Allow, Session, patterns.last() } (kind AllowAlways)
///   - Reject          -> each seg { Deny }                            (kind RejectOnce)
///   - Reject session  -> each seg { Deny, Session, patterns.last() }  (kind RejectAlways)
///   - (single-segment only) one Allow-session per *middle* rung        (kind AllowAlways)
///
/// "Reject session" mirrors "Allow session" on the deny side: the client
/// picks it to mean "no, and stop asking me about this exact command" —
/// see AllowState::deny_session's doc comment for the mechanism. Plain
/// "Reject" keeps today's behavior (asked again on the next identical
/// attempt), matching "Allow once" on the allow side.
///
/// The `option_id` is an opaque ordinal; the map records exactly which
/// `ItemChoice`s selecting it yields. No semantics are encoded in the string.
pub fn build_permission_options(
    segs: &[PermissionItem],
) -> (
    Vec<PermissionOption>,
    HashMap<PermissionOptionId, Vec<ItemChoice>>,
) {
    let mut options = Vec::new();
    let mut map: HashMap<PermissionOptionId, Vec<ItemChoice>> = HashMap::new();
    let mut next = 0u32;

    let mut push = |options: &mut Vec<PermissionOption>,
                    map: &mut HashMap<PermissionOptionId, Vec<ItemChoice>>,
                    name: String,
                    kind: PermissionOptionKind,
                    choices: Vec<ItemChoice>| {
        let id = PermissionOptionId::new(next.to_string());
        next += 1;
        options.push(PermissionOption::new(id.clone(), name, kind));
        map.insert(id, choices);
    };

    let allow = |id: &str, scope: GrantScope, pat: &str| ItemChoice {
        id: id.to_string(),
        decision: Decision::Allow,
        scope,
        pattern: Some(pat.to_string()),
    };
    let deny = |id: &str| ItemChoice {
        id: id.to_string(),
        decision: Decision::Deny,
        scope: GrantScope::Once, // ignored for Deny — plain "reject once"
        pattern: None,
    };
    let deny_session = |id: &str, pat: &str| ItemChoice {
        id: id.to_string(),
        decision: Decision::Deny,
        scope: GrantScope::Session,
        pattern: Some(pat.to_string()),
    };

    // A segment always has at least one rung; guard defensively.
    let narrow = |s: &PermissionItem| s.patterns.first().cloned().unwrap_or_else(|| s.cmd.clone());
    let broad = |s: &PermissionItem| s.patterns.last().cloned().unwrap_or_else(|| s.cmd.clone());

    // Canonical: allow once (narrowest per seg).
    push(
        &mut options,
        &mut map,
        "Allow once".to_string(),
        PermissionOptionKind::AllowOnce,
        segs.iter()
            .map(|s| allow(&s.id, GrantScope::Once, &narrow(s)))
            .collect(),
    );
    // Canonical: allow this session (broadest per seg).
    push(
        &mut options,
        &mut map,
        "Allow this session".to_string(),
        PermissionOptionKind::AllowAlways,
        segs.iter()
            .map(|s| allow(&s.id, GrantScope::Session, &broad(s)))
            .collect(),
    );
    // Canonical: reject.
    push(
        &mut options,
        &mut map,
        "Reject".to_string(),
        PermissionOptionKind::RejectOnce,
        segs.iter().map(|s| deny(&s.id)).collect(),
    );
    // Canonical: reject this session (broadest per seg) — the deny-side
    // mirror of "Allow this session" below.
    push(
        &mut options,
        &mut map,
        "Reject this session".to_string(),
        PermissionOptionKind::RejectAlways,
        segs.iter()
            .map(|s| deny_session(&s.id, &broad(s)))
            .collect(),
    );

    // Session-granular: single-segment only (per-rung is ambiguous for compounds).
    if let [seg] = segs {
        let n = seg.patterns.len();
        // Middle rungs: indices 1..n-1 (narrowest is the Once option, broadest the
        // Session option already offered above).
        for rung in seg.patterns.iter().take(n.saturating_sub(1)).skip(1) {
            push(
                &mut options,
                &mut map,
                format!("Allow `{rung}` this session"),
                PermissionOptionKind::AllowAlways,
                vec![allow(&seg.id, GrantScope::Session, rung)],
            );
        }
    }

    (options, map)
}

/// Drives one ACP-initiated turn, including the Phase-3 permission loop.
pub struct AcpTurnSource {
    first: Option<TurnStart>,
    cx: ConnectionTo<Client>,
    session_id: SessionId,
    sid: String,
    gait_dir: PathBuf,
    /// Shared per-turn cancel signal (a clone of the SessionEntry token). Tripped
    /// by the cancel handler and, on the compliant Cancelled outcome, by us.
    cancel: tokio_util::sync::CancellationToken,
}

impl AcpTurnSource {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        text: String,
        repos: Repos,
        codebase: PathBuf,
        cx: ConnectionTo<Client>,
        session_id: SessionId,
        gait_dir: PathBuf,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Self {
        Self::new_with_claim(
            text,
            repos,
            codebase,
            cx,
            session_id,
            gait_dir,
            cancel,
            gait_core::port::input::ProvenanceClaim::Absent,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_with_claim(
        text: String,
        repos: Repos,
        codebase: PathBuf,
        cx: ConnectionTo<Client>,
        session_id: SessionId,
        gait_dir: PathBuf,
        cancel: tokio_util::sync::CancellationToken,
        claim: gait_core::port::input::ProvenanceClaim,
    ) -> Self {
        let sid = session_id.0.to_string();
        // `provenance` (when present) belongs to the message body only --
        // user_turn_events()'s second body (context_status) is spine-derived
        // metadata, not attributable content, so it never carries provenance.
        let incoming = !matches!(&claim, gait_core::port::input::ProvenanceClaim::Absent);
        let message_body = if incoming {
            gait_core::proto::events::incoming_message(text.clone())
        } else {
            gait_core::proto::events::user_text(text.clone())
        };
        let mut bodies: Vec<gait_core::port::input::InputEnvelope> =
            user_turn_events(&repos, &codebase, text)
                .into_iter()
                .map(|b| (b, None).into())
                .collect();
        let direct_prompt = matches!(&claim, gait_core::port::input::ProvenanceClaim::Absent);
        if let Some(first) = bodies.first_mut() {
            *first = gait_core::port::input::InputEnvelope::with_claim(message_body, claim);
        }
        let on_durable = if direct_prompt {
            Some(direct_on_durable(sid.clone()))
        } else {
            None
        };
        Self {
            first: Some(TurnStart::Input {
                bodies,
                on_durable,
            }),
            cx,
            session_id,
            sid,
            gait_dir,
            cancel,
        }
    }

    /// Construct a turn source from an already-verified list of `(body,
    /// provenance)` pairs -- one per attested room message -- instead of a
    /// single free-text `task` plus one optional provenance. Used by the
    /// `PromptRequest` handler's attested-delivery branch, where the caller
    /// (`gait_acp::attestation::verify_prompt`) has already produced the
    /// authoritative bodies and must not re-derive them from `req.prompt`.
    /// `on_durable` confirms the batch's replay sequence once the core has
    /// appended the batch to the log.
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_bodies(
        bodies: Vec<(
            gait_core::proto::gait::log::v1::event::Body,
            Option<gait_core::proto::gait::log::v1::Provenance>,
        )>,
        on_durable: gait_core::port::input::OnDurable,
        cx: ConnectionTo<Client>,
        session_id: SessionId,
        gait_dir: PathBuf,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Self {
        let sid = session_id.0.to_string();
        Self {
            first: Some(TurnStart::Input {
                bodies: bodies.into_iter().map(Into::into).collect(),
                on_durable: Some(on_durable),
            }),
            cx,
            session_id,
            sid,
            gait_dir,
            cancel,
        }
    }
    /// Sends one `elicitation/create` request (a single free-text field) and
    /// decodes the reply. `why_or_context` folds `ClarifyItem.why` /
    /// `InformItem.context` into the message body — elicitation has no
    /// separate rationale field. Returns `None` on decline/cancel/any
    /// non-Accept outcome; the caller is responsible for treating `None`
    /// the same way the existing `Permissions` path treats
    /// `RequestPermissionOutcome::Cancelled` (trip `self.cancel`, abandon the
    /// turn). This method itself does not touch `self.cancel`.
    async fn resume_clarify_or_inform(
        &self,
        content: &str,
        why_or_context: Option<&str>,
    ) -> Option<String> {
        use agent_client_protocol::schema::v1::{
            CreateElicitationRequest, ElicitationAction, ElicitationFormMode, ElicitationMode,
            ElicitationSchema, ElicitationScope, ElicitationSessionScope, StringPropertySchema,
        };
        let message = match why_or_context {
            Some(extra) if !extra.is_empty() => format!("{content}\n\n{extra}"),
            _ => content.to_string(),
        };
        let schema = ElicitationSchema::new().property("answer", StringPropertySchema::new(), true);
        let mode = ElicitationMode::Form(ElicitationFormMode::new(
            ElicitationScope::Session(ElicitationSessionScope::new(self.session_id.clone())),
            schema,
        ));
        let req = CreateElicitationRequest::new(mode, message);
        let resp = self.cx.send_request(req).block_task().await.ok()?;
        match resp.action {
            ElicitationAction::Accept(accept) => {
                let content = accept.content?;
                match content.get("answer")? {
                    agent_client_protocol::schema::v1::ElicitationContentValue::String(s) => {
                        Some(s.clone())
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// Sends one `session/request_permission` offering exactly Once and
    /// Session, then decodes the selected grant scope.
    async fn resume_sandbox_unlock(&self) -> Option<gait_ops::permission::GrantScope> {
        use agent_client_protocol::schema::v1::{
            PermissionOption, PermissionOptionId, PermissionOptionKind, RequestPermissionOutcome,
            RequestPermissionRequest, SelectedPermissionOutcome,
        };
        let once = PermissionOption::new(
            PermissionOptionId::new("once"),
            "Allow once",
            PermissionOptionKind::AllowOnce,
        );
        let session = PermissionOption::new(
            PermissionOptionId::new("session"),
            "Allow this session",
            PermissionOptionKind::AllowAlways,
        );
        let tool_call = ToolCallUpdate::new(
            ToolCallId::new("sandbox-unlock"),
            ToolCallUpdateFields::new().title("Sandbox unlock requested".to_string()),
        );
        let req =
            RequestPermissionRequest::new(self.session_id.clone(), tool_call, vec![once, session]);
        let resp = self.cx.send_request(req).block_task().await.ok()?;
        match resp.outcome {
            RequestPermissionOutcome::Selected(SelectedPermissionOutcome { option_id, .. }) => {
                match option_id.0.as_ref() {
                    "once" => Some(gait_ops::permission::GrantScope::Once),
                    "session" => Some(gait_ops::permission::GrantScope::Session),
                    _ => None,
                }
            }
            _ => None,
        }
    }
}

/// A durable pause a session is currently awaiting a reply for, read
/// straight off the log — generalizes the old `pending_permissions` to
/// cover all four of gait's `PauseReason` peers, not just `Permissions`.
#[derive(Debug)]
enum PendingInteraction {
    Permission(Vec<PermissionItem>),
    Clarify(Vec<gait_core::proto::gait::log::v1::ClarifyItem>),
    Inform(Vec<gait_core::proto::gait::log::v1::InformItem>),
    SandboxUnlock(Vec<gait_core::proto::gait::log::v1::SandboxUnlockItem>),
}

/// A paused turn can leave terminal bookkeeping after its interaction event.
/// Inspect the request/reply sequence directly so bookkeeping does not hide
/// an unanswered request from the next ACP source round. Mirrors the
/// original `pending_permissions`' event-replay approach, generalized to
/// all four pause reasons.
fn pending_interaction(path: &std::path::Path) -> Option<PendingInteraction> {
    let events = gait_core::session::log::read_events(path).ok()?;
    let mut pending = None;
    for event in events {
        match gait_core::proto::event_kind(&event) {
            Some(payload::Kind::Permissions(r)) => {
                pending = Some(PendingInteraction::Permission(r.items.clone()))
            }
            Some(payload::Kind::PermissionsReply(_)) => pending = None,
            Some(payload::Kind::Clarify(r)) => {
                pending = Some(PendingInteraction::Clarify(r.items.clone()))
            }
            Some(payload::Kind::ClarifyReply(_)) => pending = None,
            Some(payload::Kind::Inform(r)) => {
                pending = Some(PendingInteraction::Inform(r.items.clone()))
            }
            Some(payload::Kind::InformReply(_)) => pending = None,
            Some(payload::Kind::SandboxUnlock(r)) => {
                pending = Some(PendingInteraction::SandboxUnlock(r.items.clone()))
            }
            Some(payload::Kind::SandboxUnlockReply(_)) => pending = None,
            _ => {}
        }
    }
    match &pending {
        Some(PendingInteraction::Permission(items)) if items.is_empty() => None,
        Some(PendingInteraction::Clarify(items)) if items.is_empty() => None,
        Some(PendingInteraction::Inform(items)) if items.is_empty() => None,
        Some(PendingInteraction::SandboxUnlock(items)) if items.is_empty() => None,
        _ => pending,
    }
}

#[async_trait::async_trait(?Send)]
impl TurnSource for AcpTurnSource {
    async fn next(&mut self) -> Option<TurnStart> {
        // First call: feed the user input.
        if let Some(start) = self.first.take() {
            return Some(start);
        }

        // Post-turn: is the turn waiting for an interaction?
        let log_path = gait_core::session::live::log_path(&self.gait_dir, &self.sid);
        // The relay returns after the brain's terminal frame, while the
        // append-only log writer can flush the preceding permission event a
        // few scheduler ticks later. Do not interpret that tiny publication
        // window as an exhausted source: doing so finalizes the pause before
        // ACP has had a chance to answer it. Once the log is quiescent this is
        // a short, bounded wait and the normal no-permission path is unchanged.
        let mut interaction: Option<PendingInteraction> = None;
        for _ in 0..20 {
            interaction =
                pending_interaction(&log_path).or_else(|| match session_state(&log_path) {
                    Ok(SessionState::AwaitingPermission { items }) => {
                        Some(PendingInteraction::Permission(items))
                    }
                    Ok(SessionState::AwaitingClarify { items }) => {
                        Some(PendingInteraction::Clarify(items))
                    }
                    Ok(SessionState::AwaitingInform { items }) => {
                        Some(PendingInteraction::Inform(items))
                    }
                    Ok(SessionState::AwaitingSandboxUnlock { items }) => {
                        Some(PendingInteraction::SandboxUnlock(items))
                    }
                    _ => None,
                });
            if interaction.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        match interaction? {
            PendingInteraction::Permission(items) => {
                // Ask round: one request per distinct tool_call_id; accumulate choices.
                let mut choices: Vec<ItemChoice> = Vec::new();
                // Preserve first-seen order of ids (grouping is by shared tool_call_id).
                let mut seen: Vec<String> = Vec::new();
                for it in &items {
                    if !seen.iter().any(|s| s == &it.id) {
                        seen.push(it.id.clone());
                    }
                }
                for id in &seen {
                    let segs: Vec<_> = items.iter().filter(|it| &it.id == id).cloned().collect();
                    let (options, map) = build_permission_options(&segs);
                    let tool_call = tool_call_update(&segs[0]);
                    let req =
                        RequestPermissionRequest::new(self.session_id.clone(), tool_call, options);
                    let resp = tokio::select! {
                        biased;
                        // Our own session/cancel tripped the token: abandon. The completion
                        // path sees was_cancelled() and resolves StopReason::Cancelled.
                        _ = self.cancel.cancelled() => return None,
                        r = self.cx.send_request(req).block_task() => match r {
                            Ok(r) => r,
                            // Transport/connection error: abandon the turn (drain reaps it).
                            Err(_) => return None,
                        },
                    };
                    match resp.outcome {
                        RequestPermissionOutcome::Selected(SelectedPermissionOutcome {
                            option_id,
                            ..
                        }) => match map.get(&option_id) {
                            Some(cs) => choices.extend(cs.iter().cloned()),
                            // Client echoed an option we never offered -> protocol violation.
                            // Bail; the run loop finalizes and the prompt resolves as an error
                            // (no reply written). NOT a cancellation — leave the token alone.
                            None => return None,
                        },
                        // The client cancelled while we awaited: trip the shared token so the
                        // completion path reports StopReason::Cancelled, then finalize (no reply).
                        RequestPermissionOutcome::Cancelled => {
                            self.cancel.cancel();
                            return None;
                        }
                        // Future-proof: any new outcome is treated like cancelled.
                        _ => {
                            self.cancel.cancel();
                            return None;
                        }
                    }
                }

                // Write the reply (a log append — never a turn) and resume. Completeness is
                // guaranteed by construction (choices cover every pending id). A reply error
                // is an internal bug; abandon rather than half-answer.
                // The brain may append terminal bookkeeping immediately after the
                // permission event. Use the request snapshot we just presented rather
                // than re-deriving state from that now-advanced tail.
                if self.cancel.is_cancelled() {
                    return None;
                }
                match gait_ops::permission::reply_for_items(
                    &self.gait_dir,
                    &self.sid,
                    items,
                    choices,
                ) {
                    Ok(_) => Some(TurnStart::Resume),
                    Err(_) => None,
                }
            }
            PendingInteraction::Clarify(items) => {
                if items.len() != 1 {
                    return None;
                }
                let item = &items[0];
                let answer = tokio::select! {
                    biased;
                    _ = self.cancel.cancelled() => return None,
                    a = self.resume_clarify_or_inform(&item.content, Some(&item.why)) => a,
                };
                let Some(answer) = answer else {
                    self.cancel.cancel();
                    return None;
                };
                match gait_ops::interaction::reply(&self.gait_dir, &self.sid, answer) {
                    Ok(_) => Some(TurnStart::Resume),
                    Err(_) => None,
                }
            }
            PendingInteraction::Inform(items) => {
                if items.len() != 1 {
                    return None;
                }
                let item = &items[0];
                let answer = tokio::select! {
                    biased;
                    _ = self.cancel.cancelled() => return None,
                    a = self.resume_clarify_or_inform(&item.content, item.context.as_deref()) => a,
                };
                let Some(answer) = answer else {
                    self.cancel.cancel();
                    return None;
                };
                match gait_ops::interaction::reply(&self.gait_dir, &self.sid, answer) {
                    Ok(_) => Some(TurnStart::Resume),
                    Err(_) => None,
                }
            }
            PendingInteraction::SandboxUnlock(_items) => {
                let scope = tokio::select! {
                    biased;
                    _ = self.cancel.cancelled() => return None,
                    s = self.resume_sandbox_unlock() => s,
                };
                let Some(scope) = scope else {
                    self.cancel.cancel();
                    return None;
                };
                match gait_ops::permission::unlock(gait_ops::permission::UnlockParams {
                    gait_dir: self.gait_dir.clone(),
                    session_id: self.sid.clone(),
                    scope,
                    agent: None,
                }) {
                    Ok(_) => Some(TurnStart::Resume),
                    Err(_) => None,
                }
            }
        }
    }
}

/// Streams settled turn events out as ACP `session/update` notifications and
/// captures the final `TurnResult`. `cx.send_notification` is synchronous, so
/// `item()` sends inline — no bridge task, and FIFO ordering is preserved.
pub struct AcpTurnSink {
    cx: ConnectionTo<Client>,
    session_id: SessionId,
    result: Option<RunResult>,
}

impl AcpTurnSink {
    pub fn new(cx: ConnectionTo<Client>, session_id: SessionId) -> Self {
        Self {
            cx,
            session_id,
            result: None,
        }
    }
    pub fn take_result(&mut self) -> Option<RunResult> {
        self.result.take()
    }
}

impl TurnSink for AcpTurnSink {
    fn item(&mut self, item: &StreamItem) {
        // Live text deltas are a no-op stub this phase (the settled AssistantText
        // event carries the full text — see design §6).
        let Some(stream_item::Item::Event(ev)) = &item.item else {
            return;
        };
        if let Some(update) = event_to_session_update(ev) {
            let _ = self
                .cx
                .send_notification(SessionNotification::new(self.session_id.clone(), update));
        }
    }
    fn result(&mut self, r: &RunResult) {
        self.result = Some(r.clone());
    }
    fn session_id(&mut self, _id: &str) {}
    fn diagnostic(&mut self, _level: Diag, _msg: &str) {}
}

#[cfg(test)]
mod tests {
    #[test]
    fn direct_prompts_in_one_session_get_distinct_record_ids() {
        let cap = concord_telemetry::testing::capture_exclusive();
        let session = format!("direct-session-{}", std::process::id());
        direct_on_durable(session.clone())().unwrap();
        direct_on_durable(session.clone())().unwrap();
        let events = cap.events_where::<gait_events::gait::events::v1::PromptAccepted>(
            |v| v.session_id == session,
        );
        assert_eq!(events.len(), 2);
        assert_ne!(events[0].source_record_id, events[1].source_record_id);
        for event in &events {
            assert_ne!(event.source_record_id, session);
            assert_eq!(event.message_ids, vec![event.source_record_id.clone()]);
            assert_eq!(event.source, gait_events::gait::events::v1::PromptSource::Direct as i32);
        }
    }

    use super::*;
    use agent_client_protocol::schema::v1::{
        ContentBlock, PermissionOptionKind, SessionUpdate, StopReason, TextContent, ToolCallId,
    };
    use gait_core::proto::events;
    use gait_core::proto::gait::log::v1::{Event, PauseReason};
    use gait_ops::permission::GrantScope;

    fn run_with(outcome: gait_core::proto::gait::log::v1::turn_result::Outcome) -> RunResult {
        RunResult {
            run_id: "r1".into(),
            turns: vec![gait_core::proto::gait::log::v1::TurnResult {
                turn_id: "t1".into(),
                outcome: Some(outcome),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    fn event(body: gait_core::proto::gait::log::v1::event::Body) -> Event {
        Event {
            body: Some(body),
            ..Default::default()
        }
    }

    #[test]
    fn assistant_text_maps_to_agent_message_chunk() {
        let ev = event(events::assistant_text("hello".into()));
        match event_to_session_update(&ev) {
            Some(SessionUpdate::AgentMessageChunk(chunk)) => {
                assert!(matches!(&chunk.content, ContentBlock::Text(t) if t.text == "hello"));
                assert!(
                    chunk.message_id.is_some(),
                    "message_id must be set for v1->v2 conversion"
                );
            }
            other => panic!("expected AgentMessageChunk, got {other:?}"),
        }
    }

    #[test]
    fn user_text_maps_to_user_message_chunk() {
        let ev = event(events::user_text("hello".into()));
        match event_to_session_update(&ev) {
            Some(SessionUpdate::UserMessageChunk(chunk)) => {
                assert!(matches!(&chunk.content, ContentBlock::Text(t) if t.text == "hello"));
                assert!(
                    chunk.message_id.is_some(),
                    "message_id must be set for v1->v2 conversion"
                );
            }
            other => panic!("expected UserMessageChunk, got {other:?}"),
        }
    }

    #[test]
    fn each_text_chunk_call_gets_a_distinct_message_id() {
        // Each settled event is its own complete message (no live-delta
        // streaming today), so two separate chunks must never collide.
        let a = text_chunk("first");
        let b = text_chunk("second");
        assert_ne!(a.message_id, b.message_id);
    }

    #[test]
    fn replay_updates_preserve_renderable_event_order() {
        let events = vec![
            event(events::user_text("question".into())),
            event(events::assistant_text("answer".into())),
        ];
        let updates = replay_updates(&events);
        assert!(matches!(
            &updates[..],
            [
                SessionUpdate::UserMessageChunk(_),
                SessionUpdate::AgentMessageChunk(_)
            ]
        ));
    }

    #[test]
    fn mode_maps_to_current_mode_update() {
        let ev = event(events::mode(
            "plan",
            vec!["shell".to_string()],
            vec![],
            "steer text",
            "Plan mode",
            None,
        ));
        match event_to_session_update(&ev) {
            Some(SessionUpdate::CurrentModeUpdate(upd)) => {
                assert_eq!(
                    upd.current_mode_id,
                    agent_client_protocol::schema::v1::SessionModeId::new("plan")
                );
            }
            other => panic!("expected CurrentModeUpdate, got {other:?}"),
        }
    }

    #[test]
    fn thinking_maps_to_agent_thought_chunk() {
        let ev = event(events::thinking_block("pondering".into(), String::new()));
        assert!(matches!(
            event_to_session_update(&ev),
            Some(SessionUpdate::AgentThoughtChunk(_))
        ));
    }

    #[test]
    fn tool_call_maps_to_tool_call_update_kind() {
        let ev = event(events::tool_call(
            "c1".into(),
            "read".into(),
            &serde_json::json!({"file_path": "a.rs"}),
        ));
        assert!(matches!(
            event_to_session_update(&ev),
            Some(SessionUpdate::ToolCall(_))
        ));
    }

    #[test]
    fn tool_result_maps_to_tool_call_update() {
        let ev = event(events::tool_result("c1".into(), "ok".into(), None, None));
        assert!(matches!(
            event_to_session_update(&ev),
            Some(SessionUpdate::ToolCallUpdate(_))
        ));
    }

    #[test]
    fn non_client_facing_events_map_to_none() {
        // A session_start / context event has no client-facing rendering.
        let ev = event(events::cwd("/tmp"));
        assert!(event_to_session_update(&ev).is_none());
    }

    #[test]
    fn content_blocks_to_text_concatenates_text_blocks() {
        let blocks = vec![
            ContentBlock::Text(TextContent::new("say ")),
            ContentBlock::Text(TextContent::new("hi")),
        ];
        assert_eq!(content_blocks_to_text(&blocks), "say hi");
    }

    #[test]
    fn done_maps_to_end_turn() {
        let r = run_with(gait_core::proto::gait::log::v1::turn_result::Outcome::Done(
            Default::default(),
        ));
        assert_eq!(turn_result_to_stop_reason(&r), Ok(StopReason::EndTurn));
    }

    #[test]
    fn cancelled_maps_to_cancelled() {
        assert_eq!(
            turn_result_to_stop_reason(&run_with(
                gait_core::proto::gait::log::v1::turn_result::Outcome::Cancelled(Default::default())
            )),
            Ok(StopReason::Cancelled)
        );
    }

    #[test]
    fn max_steps_maps_to_max_turn_requests() {
        let r = run_with(
            gait_core::proto::gait::log::v1::turn_result::Outcome::Failed(
                gait_core::proto::gait::log::v1::TurnFailed {
                    reason: "max_steps".into(),
                    message: "hit cap".into(),
                },
            ),
        );
        assert_eq!(
            turn_result_to_stop_reason(&r),
            Ok(StopReason::MaxTurnRequests)
        );
    }

    #[test]
    fn error_maps_to_jsonrpc_error() {
        let r = run_with(
            gait_core::proto::gait::log::v1::turn_result::Outcome::Failed(
                gait_core::proto::gait::log::v1::TurnFailed {
                    reason: "error".into(),
                    message: "boom".into(),
                },
            ),
        );
        assert_eq!(turn_result_to_stop_reason(&r), Err("boom".into()));
    }

    #[test]
    fn interaction_required_maps_to_jsonrpc_error() {
        // Permissions/Clarify/Inform/Unlock are the Phase-3 seam: no interactive
        // support yet, so surface a loud error rather than a fake completion.
        for reason in [
            PauseReason::Permissions,
            PauseReason::Clarify,
            PauseReason::Inform,
            PauseReason::SandboxUnlock,
        ] {
            let run = run_with(
                gait_core::proto::gait::log::v1::turn_result::Outcome::Pause(
                    gait_core::proto::gait::log::v1::TurnPause {
                        reason: reason as i32,
                        ..Default::default()
                    },
                ),
            );
            assert!(turn_result_to_stop_reason(&run).is_err());
        }
    }

    fn item(id: &str, cmd: &str, patterns: &[&str]) -> PermissionItem {
        PermissionItem {
            id: id.to_string(),
            cmd: cmd.to_string(),
            reason: String::new(),
            patterns: patterns.iter().map(|s| s.to_string()).collect(),
            tool_name: "shell".to_string(),
            tool_args: None,
            preview: None,
        }
    }

    #[test]
    fn single_segment_request_offers_canonical_plus_ladder() {
        // A 3-rung ladder: narrowest "cargo build", mid "cargo *", broadest "*".
        let segs = vec![item("tc1", "cargo build", &["cargo build", "cargo *", "*"])];
        let (options, map) = build_permission_options(&segs);

        // Canonical four (allow-once, allow-session, reject, reject-session)
        // + one middle-rung session option = 5.
        assert_eq!(options.len(), 5, "4 canonical + 1 middle rung");

        // Every option decodes.
        for opt in &options {
            assert!(
                map.contains_key(&opt.option_id),
                "option {:?} must decode",
                opt.option_id
            );
        }

        // Allow-once (kind AllowOnce) -> Once scope, narrowest pattern.
        let once = options
            .iter()
            .find(|o| o.kind == PermissionOptionKind::AllowOnce)
            .unwrap();
        let once_choices = &map[&once.option_id];
        assert_eq!(once_choices.len(), 1);
        assert_eq!(once_choices[0].decision, Decision::Allow);
        assert_eq!(once_choices[0].scope, GrantScope::Once);
        assert_eq!(once_choices[0].pattern.as_deref(), Some("cargo build"));

        // Reject (kind RejectOnce) -> Deny.
        let reject = options
            .iter()
            .find(|o| o.kind == PermissionOptionKind::RejectOnce)
            .unwrap();
        assert_eq!(map[&reject.option_id][0].decision, Decision::Deny);

        // The Session options (kind AllowAlways) carry Session scope; the broadest
        // one grants "*", and the ladder as a whole covers the mid rung "cargo *".
        let session_patterns: std::collections::HashSet<String> = options
            .iter()
            .filter(|o| o.kind == PermissionOptionKind::AllowAlways)
            .flat_map(|o| map[&o.option_id].iter())
            .inspect(|c| assert_eq!(c.scope, GrantScope::Session))
            .filter_map(|c| c.pattern.clone())
            .collect();
        assert!(
            session_patterns.contains("*"),
            "broadest session grant present"
        );
        assert!(
            session_patterns.contains("cargo *"),
            "mid-rung session grant present"
        );
    }

    #[test]
    fn compound_segments_offer_canonical_only() {
        // Two segments sharing one tool_call_id -> compound command, all-or-nothing.
        let segs = vec![
            item("tc9", "cargo build", &["cargo build", "cargo *"]),
            item("tc9", "git push", &["git push", "git *"]),
        ];
        let (options, map) = build_permission_options(&segs);

        // Exactly the canonical four — no per-rung explosion for multi-segment.
        assert_eq!(options.len(), 4, "compound requests get canonical-only");

        // "Allow this session" expands to ONE choice per segment (both broadest),
        // proving all-or-nothing coverage of the shared id.
        let session = options
            .iter()
            .find(|o| o.kind == PermissionOptionKind::AllowAlways)
            .unwrap();
        let choices = &map[&session.option_id];
        assert_eq!(choices.len(), 2, "one choice per segment");
        assert!(
            choices
                .iter()
                .all(|c| c.id == "tc9" && c.scope == GrantScope::Session)
        );
        let pats: std::collections::HashSet<&str> = choices
            .iter()
            .filter_map(|c| c.pattern.as_deref())
            .collect();
        assert!(
            pats.contains("cargo *") && pats.contains("git *"),
            "each seg broadest"
        );
    }

    #[test]
    fn reject_decodes_to_deny_for_all_segments() {
        let segs = vec![
            item("tc9", "cargo build", &["cargo build"]),
            item("tc9", "git push", &["git push"]),
        ];
        let (options, map) = build_permission_options(&segs);
        let reject = options
            .iter()
            .find(|o| o.kind == PermissionOptionKind::RejectOnce)
            .unwrap();
        let choices = &map[&reject.option_id];
        assert_eq!(choices.len(), 2);
        assert!(choices.iter().all(|c| c.decision == Decision::Deny));
    }

    #[test]
    fn reject_session_decodes_to_session_scoped_deny_broadest_pattern() {
        // The deny-side mirror of "Allow this session": kind RejectAlways,
        // Decision::Deny, GrantScope::Session, broadest pattern per segment.
        let segs = vec![
            item("tc9", "cargo build", &["cargo build", "cargo *"]),
            item("tc9", "git push", &["git push", "git *"]),
        ];
        let (options, map) = build_permission_options(&segs);
        let reject_session = options
            .iter()
            .find(|o| o.kind == PermissionOptionKind::RejectAlways)
            .unwrap();
        let choices = &map[&reject_session.option_id];
        assert_eq!(choices.len(), 2, "one choice per segment");
        assert!(choices.iter().all(|c| c.id == "tc9"
            && c.decision == Decision::Deny
            && c.scope == GrantScope::Session));
        let pats: std::collections::HashSet<&str> = choices
            .iter()
            .filter_map(|c| c.pattern.as_deref())
            .collect();
        assert!(
            pats.contains("cargo *") && pats.contains("git *"),
            "each seg's broadest pattern"
        );
    }

    #[test]
    fn tool_call_update_carries_id_and_title() {
        let it = item("tc1", "cargo build", &["cargo build"]);
        let upd = tool_call_update(&it);
        assert_eq!(upd.tool_call_id, ToolCallId::new("tc1"));
        assert_eq!(upd.fields.title.as_deref(), Some("cargo build"));
    }

    #[test]
    fn pending_interaction_detects_clarify() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        gait_state::proto::wire::append(
            &path,
            gait_state::proto::events::clarify(&[gait_core::proto::gait::log::v1::ClarifyItem {
                id: "c1".into(),
                content: "which db?".into(),
                why: "ambiguous".into(),
                kind: gait_core::proto::gait::log::v1::ClarifyKind::Is as i32,
                options: vec![],
            }]),
        )
        .unwrap();
        match pending_interaction(&path) {
            Some(PendingInteraction::Clarify(items)) => assert_eq!(items.len(), 1),
            other => panic!("expected Clarify, got {other:?}"),
        }
    }

    #[test]
    fn pending_interaction_detects_inform() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        gait_state::proto::wire::append(
            &path,
            gait_state::proto::events::inform(&[gait_core::proto::gait::log::v1::InformItem {
                id: "i1".into(),
                content: "deleted 3 stale branches".into(),
                context: None,
            }]),
        )
        .unwrap();
        match pending_interaction(&path) {
            Some(PendingInteraction::Inform(items)) => assert_eq!(items.len(), 1),
            other => panic!("expected Inform, got {other:?}"),
        }
    }

    #[test]
    fn pending_interaction_detects_sandbox_unlock() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        gait_state::proto::wire::append(
            &path,
            gait_state::proto::events::sandbox_unlock(&[
                gait_core::proto::gait::log::v1::SandboxUnlockItem {
                    id: "u1".into(),
                    tool_name: "shell".into(),
                    resource: "/etc".into(),
                    dimension: "write".into(),
                    preview: None,
                },
            ]),
        )
        .unwrap();
        match pending_interaction(&path) {
            Some(PendingInteraction::SandboxUnlock(items)) => assert_eq!(items.len(), 1),
            other => panic!("expected SandboxUnlock, got {other:?}"),
        }
    }

    #[test]
    fn pending_interaction_none_when_idle() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        assert!(pending_interaction(&path).is_none());
    }

    #[tokio::test]
    async fn resume_clarify_sends_elicitation_and_returns_accepted_text() {
        use agent_client_protocol::schema::ProtocolVersion;
        use agent_client_protocol::schema::v1::{
            CreateElicitationRequest, CreateElicitationResponse, ElicitationAcceptAction,
            ElicitationAction, ElicitationContentValue, InitializeRequest, InitializeResponse,
        };
        use agent_client_protocol::{Agent, Client, ConnectTo, Responder};
        let (cx_tx, cx_rx) = tokio::sync::oneshot::channel();
        let cx_tx = std::sync::Arc::new(std::sync::Mutex::new(Some(cx_tx)));
        let server = Agent.builder().on_receive_request(
            move |_req: InitializeRequest,
                  responder: Responder<InitializeResponse>,
                  cx: agent_client_protocol::ConnectionTo<Client>| {
                let cx_tx = cx_tx.clone();
                async move {
                    cx_tx.lock().unwrap().take().unwrap().send(cx).unwrap();
                    responder.respond(InitializeResponse::new(ProtocolVersion::V1))
                }
            },
            agent_client_protocol::on_receive_request!(),
        );
        let (channel, server_future) = server.into_channel_and_future();
        tokio::spawn(server_future);
        tokio::spawn(
            Client
                .builder()
                .on_receive_request(
                    |_req: CreateElicitationRequest,
                     responder: Responder<CreateElicitationResponse>,
                     _cx| async move {
                        let mut content = std::collections::BTreeMap::new();
                        content.insert(
                            "answer".to_string(),
                            ElicitationContentValue::String("postgres".into()),
                        );
                        responder.respond(CreateElicitationResponse::new(
                            ElicitationAction::Accept(
                                ElicitationAcceptAction::new().content(Some(content)),
                            ),
                        ))
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .connect_with(channel, async |cx| {
                    cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                        .block_task()
                        .await?;
                    std::future::pending::<Result<(), agent_client_protocol::Error>>().await
                }),
        );
        let cx = cx_rx.await.unwrap();
        let source = AcpTurnSource::new(
            "hello".into(),
            Repos::production(),
            PathBuf::from("."),
            cx,
            SessionId::new("s1"),
            PathBuf::from("."),
            tokio_util::sync::CancellationToken::new(),
        );
        let answer = source
            .resume_clarify_or_inform("which db?", Some("ambiguous config"))
            .await;
        assert_eq!(answer.as_deref(), Some("postgres"));
    }

    #[tokio::test]
    async fn resume_clarify_decline_returns_none() {
        use agent_client_protocol::schema::ProtocolVersion;
        use agent_client_protocol::schema::v1::{
            CreateElicitationRequest, CreateElicitationResponse, ElicitationAction,
            InitializeRequest, InitializeResponse,
        };
        use agent_client_protocol::{Agent, Client, ConnectTo, Responder};
        let (cx_tx, cx_rx) = tokio::sync::oneshot::channel();
        let cx_tx = std::sync::Arc::new(std::sync::Mutex::new(Some(cx_tx)));
        let server = Agent.builder().on_receive_request(
            move |_req: InitializeRequest,
                  responder: Responder<InitializeResponse>,
                  cx: agent_client_protocol::ConnectionTo<Client>| {
                let cx_tx = cx_tx.clone();
                async move {
                    cx_tx.lock().unwrap().take().unwrap().send(cx).unwrap();
                    responder.respond(InitializeResponse::new(ProtocolVersion::V1))
                }
            },
            agent_client_protocol::on_receive_request!(),
        );
        let (channel, server_future) = server.into_channel_and_future();
        tokio::spawn(server_future);
        tokio::spawn(
            Client
                .builder()
                .on_receive_request(
                    |_req: CreateElicitationRequest,
                     responder: Responder<CreateElicitationResponse>,
                     _cx| async move {
                        responder
                            .respond(CreateElicitationResponse::new(ElicitationAction::Decline))
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .connect_with(channel, async |cx| {
                    cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                        .block_task()
                        .await?;
                    std::future::pending::<Result<(), agent_client_protocol::Error>>().await
                }),
        );
        let cx = cx_rx.await.unwrap();
        let source = AcpTurnSource::new(
            "hello".into(),
            Repos::production(),
            PathBuf::from("."),
            cx,
            SessionId::new("s1"),
            PathBuf::from("."),
            tokio_util::sync::CancellationToken::new(),
        );
        assert!(
            source
                .resume_clarify_or_inform("which db?", None)
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn resume_sandbox_unlock_sends_once_session_options_and_decodes_choice() {
        use agent_client_protocol::schema::ProtocolVersion;
        use agent_client_protocol::schema::v1::{
            InitializeRequest, InitializeResponse, PermissionOptionKind, RequestPermissionOutcome,
            RequestPermissionRequest, RequestPermissionResponse, SelectedPermissionOutcome,
        };
        use agent_client_protocol::{Agent, Client, ConnectTo, Responder};
        let (cx_tx, cx_rx) = tokio::sync::oneshot::channel();
        let cx_tx = std::sync::Arc::new(std::sync::Mutex::new(Some(cx_tx)));
        let server = Agent.builder().on_receive_request(
            move |_req: InitializeRequest,
                  responder: Responder<InitializeResponse>,
                  cx: agent_client_protocol::ConnectionTo<Client>| {
                let cx_tx = cx_tx.clone();
                async move {
                    cx_tx.lock().unwrap().take().unwrap().send(cx).unwrap();
                    responder.respond(InitializeResponse::new(ProtocolVersion::V1))
                }
            },
            agent_client_protocol::on_receive_request!(),
        );
        let (channel, server_future) = server.into_channel_and_future();
        tokio::spawn(server_future);
        tokio::spawn(
            Client
                .builder()
                .on_receive_request(
                    |req: RequestPermissionRequest,
                     responder: Responder<RequestPermissionResponse>,
                     _cx| async move {
                        assert_eq!(req.options.len(), 2);
                        let session_opt = req
                            .options
                            .iter()
                            .find(|o| o.kind == PermissionOptionKind::AllowAlways)
                            .unwrap();
                        responder.respond(RequestPermissionResponse::new(
                            RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
                                session_opt.option_id.clone(),
                            )),
                        ))
                    },
                    agent_client_protocol::on_receive_request!(),
                )
                .connect_with(channel, async |cx| {
                    cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                        .block_task()
                        .await?;
                    std::future::pending::<Result<(), agent_client_protocol::Error>>().await
                }),
        );
        let cx = cx_rx.await.unwrap();
        let source = AcpTurnSource::new(
            "hello".into(),
            Repos::production(),
            PathBuf::from("."),
            cx,
            SessionId::new("s1"),
            PathBuf::from("."),
            tokio_util::sync::CancellationToken::new(),
        );
        let scope = source.resume_sandbox_unlock().await;
        assert_eq!(scope, Some(gait_ops::permission::GrantScope::Session));
    }
}

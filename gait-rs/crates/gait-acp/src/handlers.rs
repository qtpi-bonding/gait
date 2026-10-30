//! ACP request handlers for the Phase 1 connection skeleton: `initialize` and
//! `session/new`. See docs/plans/2026-07-09-acp-adapter-design.md.
//!
//! Concurrency finding (Task 5): `on_receive_request` handlers run inside the
//! dispatch loop and block further message processing until they return,
//! **unless** the handler explicitly hands work off via `ConnectionTo::spawn`
//! and returns immediately — see `build_agent_with_slow_handler` below, which
//! is the pattern Phase 2's `session/prompt` must follow so long-running brain
//! turns don't stall `initialize`/`session/new` on other connections.
use agent_client_protocol::schema::v1::{
    AgentCapabilities, CancelNotification, CloseSessionRequest, CloseSessionResponse,
    CurrentModeUpdate, DeleteSessionRequest, DeleteSessionResponse, InitializeRequest,
    InitializeResponse, ListSessionsRequest, ListSessionsResponse, LoadSessionRequest,
    LoadSessionResponse, NewSessionRequest, NewSessionResponse, PromptRequest, PromptResponse,
    ResumeSessionRequest, ResumeSessionResponse, SessionCapabilities, SessionCloseCapabilities,
    SessionDeleteCapabilities, SessionListCapabilities, SessionMode, SessionModeState,
    SessionNotification, SessionResumeCapabilities, SessionUpdate, SetSessionModeRequest,
    SetSessionModeResponse, StopReason,
};
use agent_client_protocol::{Agent, Builder, Client, HandleDispatchFrom, NullRun, Responder};
use std::path::Path;
use std::sync::Arc;
use tracing::Instrument;

use crate::attestation::Attestor;
use crate::error::{self, PromptError};
use crate::lifecycle;
use crate::session_map::{SessionMap, discover_gait_dir_for_cwd};
use crate::turn_io::{
    AcpTurnSink, AcpTurnSource, content_blocks_to_text, emit_prompt_accepted,
    turn_result_to_stop_reason,
};

/// The `on_durable` hook of an attested batch: once the batch is in the log
/// its idempotency key must stay recorded, `PromptAccepted` is emitted once,
/// and the replay sequence is confirmed.
fn attested_on_durable(
    source_record_id: String,
    session_id: String,
    message_ids: Vec<String>,
    author_id: Option<String>,
    durable_flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
    confirm: impl FnOnce() -> anyhow::Result<()> + Send + 'static,
) -> gait_core::port::input::OnDurable {
    Box::new(move || {
        durable_flag.store(true, std::sync::atomic::Ordering::SeqCst);
        let author = author_id.map(|id| {
            gait_events::Principal::new(gait_events::PrincipalKind::Agent, id).with_verified_by([
                gait_events::Verification::DeliveryAttestation,
                gait_events::Verification::DelegationCert,
            ])
        });
        emit_prompt_accepted(
            source_record_id,
            session_id,
            message_ids,
            gait_events::gait::events::v1::PromptSource::Attested,
            author,
        );
        confirm()
    })
}

/// A supplied-but-invalid (or required-but-missing) attestation does not fail
/// the prompt: it runs as rejected user text. It is still an error, so it goes
/// through `acp_error` once, which emits the error record and builds the wire
/// error. Returns that wire error so callers and tests can see its code.
fn reject_attestation(err: &anyhow::Error) -> agent_client_protocol::Error {
    error::acp_error(err, error::PROMPT)
}

fn meta_getter(
    meta: &Option<serde_json::Map<String, serde_json::Value>>,
) -> impl Fn(&str) -> Option<String> + '_ {
    move |key| {
        meta.as_ref()
            .and_then(|values| values.get(key))
            .and_then(|value| value.as_str())
            .map(str::to_owned)
    }
}

/// Build the gait-acp agent: registers `initialize` and `session/new` against
/// the given `SessionMap`. Returns the concrete `Builder` (not just an opaque
/// `ConnectTo<Client>`) so `build_agent_with_slow_handler` (Task 5) can chain
/// one more `.on_receive_request` onto it; the builder itself still
/// implements `ConnectTo<Client>` for `.connect_to(transport)` /
/// `.into_channel_and_future()`.
/// Advertise the reserved `default` baseline plus every authored mode.
/// `current_mode_id` is always "default" for a freshly minted session
/// (gait-acp mints a new session per `session/new`; no mode is restored).
/// A mode whose `MODE.md` fails to parse is still listed (by name) with no
/// description — advertisement must never fail `session/new`.
pub fn advertise_modes(gait_dir: &Path) -> SessionModeState {
    let mut modes = vec![
        SessionMode::new("default", "default")
            .description(Some("gait's baseline — no overlay".to_string())),
    ];
    for name in gait_core::modes::list_modes(gait_dir) {
        let desc: Option<String> = gait_core::modes::load_mode(gait_dir, &name)
            .ok()
            .map(|m| m.description)
            .filter(|d| !d.is_empty());
        modes.push(SessionMode::new(name.clone(), name).description(desc));
    }
    SessionModeState::new("default", modes)
}

pub fn build_agent(map: SessionMap) -> Builder<Agent, impl HandleDispatchFrom<Client>, NullRun> {
    build_agent_with_attestation(map, false, None)
}

/// Like [`build_agent`], but with the attestation-verification policy that
/// governs `session/prompt`: `require_attestation` is a single process-wide
/// `bool` (not per-session state -- the trust decision has to be rooted in
/// how the process itself was launched, e.g. by `multimatrix-bridge`, not a
/// runtime `_meta` claim), and `attestor` holds the agent's trusted delivery
/// keys and own devices. `None` means this process has no agent identity, so
/// no attestation can be verified.
pub fn build_agent_with_attestation(
    map: SessionMap,
    require_attestation: bool,
    attestor: Option<Arc<Attestor>>,
) -> Builder<Agent, impl HandleDispatchFrom<Client>, NullRun> {
    // Connection-level: written by the `initialize` handler below, read by each
    // `session/prompt` before it installs the fs delegate (design §1).
    let caps = std::sync::Arc::new(crate::fs_delegate::ClientFsCaps::default());
    Agent
        .builder()
        .name("gait-acp")
        .on_receive_request(
            {
                let caps = caps.clone();
                move |req: InitializeRequest, responder: Responder<InitializeResponse>, _cx| {
                    let caps = caps.clone();
                    async move {
                        // Static/minimal capabilities; zero auth methods so `authenticate`
                        // never arrives (design §5).
                        // Record the client's fs capabilities for the whole
                        // connection. All-or-nothing gating happens at prompt time.
                        let fs = req.client_capabilities.fs;
                        caps.set(fs.read_text_file, fs.write_text_file);
                        // Phase 7: advertise session/list, session/close, session/delete
                        // so a spec-conformant client actually sends them.
                        let session_caps = SessionCapabilities::default()
                            .list(SessionListCapabilities::default())
                            .close(SessionCloseCapabilities::default())
                            .delete(SessionDeleteCapabilities::default())
                            .resume(SessionResumeCapabilities::default());
                        let agent_caps = AgentCapabilities::default()
                            .load_session(true)
                            .session_capabilities(session_caps);
                        responder.respond(
                            InitializeResponse::new(req.protocol_version)
                                .agent_capabilities(agent_caps),
                        )
                    }
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let map = map.clone();
                move |req: LoadSessionRequest,
                      responder: Responder<LoadSessionResponse>,
                      cx: agent_client_protocol::ConnectionTo<Client>| {
                    let map = map.clone();
                    async move { lifecycle::load_session(&map, req, responder, cx) }
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let map = map.clone();
                move |req: ResumeSessionRequest,
                      responder: Responder<ResumeSessionResponse>,
                      _cx| {
                    let map = map.clone();
                    async move { lifecycle::resume_session(&map, req, responder) }
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let map = map.clone();
                move |req: NewSessionRequest, responder: Responder<NewSessionResponse>, _cx| {
                    let map = map.clone();
                    async move {
                        let gait_dir = match discover_gait_dir_for_cwd(&req.cwd) {
                            Ok(dir) => dir,
                            Err(e) => return responder.respond_with_error(error::acp_error(&anyhow::anyhow!(e), error::NEW_SESSION)),
                        };
                        let session_id = gait_core::session::new_session_id();
                        let modes = advertise_modes(&gait_dir);
                        map.insert(gait_dir, session_id.clone(), req.cwd.clone());
                        responder.respond(NewSessionResponse::new(session_id).modes(Some(modes)))
                    }
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let map = map.clone();
                let caps = caps.clone();
                let attestor = attestor.clone();
                move |req: PromptRequest,
                      responder: Responder<PromptResponse>,
                      cx: agent_client_protocol::ConnectionTo<Client>| {
                    let map = map.clone();
                    let caps = caps.clone();
                    let attestor = attestor.clone();
                    async move {
                        let session_id = req.session_id.clone();
                        let sid = session_id.0.to_string();
                        let Some((gait_dir, codebase)) = map.resolve(&sid) else {
                            return responder.respond_with_error(error::acp_error(&coded_error::into_anyhow(PromptError::SessionUnknown(sid.clone())), error::PROMPT));
                        };
                        let span = tracing::info_span!("acp.prompt", session_id = %sid);
                        concord_telemetry::propagation::set_parent(&span, meta_getter(&req.meta));
                        // Reject a resend of an already-admitted idempotency
                        // key immediately, before every other admission check
                        // below -- this must run FIRST so the key is never
                        // falsely blacklisted by some OTHER rejection reason.
                        // The key is only *recorded* (further down) once the
                        // request has passed every check and this turn is
                        // actually going to run -- see
                        // `SessionMap::record_idempotency_key`'s doc comment
                        // for why recording earlier would be unsound.
                        let idempotency_key =
                            crate::idempotency::idempotency_key_from_meta(&req.meta);
                        if let Some(key) = &idempotency_key {
                            if map.has_seen_idempotency_key(&sid, key) {
                                return responder.respond_with_error(error::acp_error(&coded_error::into_anyhow(PromptError::AlreadySeen { key: key.clone() }), error::PROMPT));
                            }
                        }
                        // Reject BEFORE touching any session_map bookkeeping. A
                        // second concurrent PromptRequest is going to fail once
                        // its thread reaches SessionLock anyway (see the
                        // 2026-08-21 doc) -- but overwriting cascade/cancel/join
                        // here first would clobber the still-running first
                        // turn's live tracking in the window before that
                        // rejection lands (see the 2026-08-30 follow-up doc).
                        if map.has_live_turn(&sid) {
                            return responder.respond_with_error(error::acp_error(&coded_error::into_anyhow(PromptError::SessionBusy(sid.clone())), error::PROMPT));
                        }
                        // Verification records nothing; the replay-sequence
                        // confirmation travels with the batch as its
                        // `on_durable` closure.
                        let verified = crate::attestation::verify_prompt(
                            attestor.as_deref(),
                            &req.meta,
                        );
                        // Only now -- past every admission check, committed to
                        // actually running this turn -- record the idempotency
                        // key. Recording earlier would permanently blacklist a
                        // key whose prompt was rejected above and never ran. If
                        // the turn then fails before its trigger is durable the
                        // key is released again (see the turn thread below), so
                        // a retry with the same key is accepted.
                        let batch_durable =
                            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                        if let Some(key) = &idempotency_key {
                            map.record_idempotency_key(&sid, key);
                        }
                        // Two ways to build this turn's trigger: the
                        // no-attestation path (plain prompts remain visible;
                        // malformed provenance claims are marked for
                        // quarantine), via user_turn_events() so the
                        // context_status body is still included, or the
                        // fix -- one (body, provenance) pair per verified
                        // room message, built exclusively from `body_text`,
                        // never from `req.prompt`.
                        enum TurnInput {
                            Prompt {
                                text: String,
                                provenance: gait_core::port::input::ProvenanceClaim,
                            },
                            AttestedMessages {
                                bodies: Vec<(
                                    gait_core::proto::gait::log::v1::event::Body,
                                    Option<gait_core::proto::gait::log::v1::Provenance>,
                                )>,
                                on_durable: gait_core::port::input::OnDurable,
                            },
                        }
                        let turn_input = match verified {
                            // A supplied but invalid attestation is retained
                            // as rejected user text in the durable log. The
                            // core fold excludes it from model context.
                            Err(e) => {
                                let _ = reject_attestation(&e);
                                TurnInput::Prompt {
                                    text: content_blocks_to_text(&req.prompt),
                                    provenance: gait_core::port::input::ProvenanceClaim::Rejected,
                                }
                            }
                            // No attestation present at all.
                            Ok(None) => {
                                if require_attestation {
                                    let _ = reject_attestation(&coded_error::into_anyhow(
                                        PromptError::AttestationRequired,
                                    ));
                                    TurnInput::Prompt {
                                        text: content_blocks_to_text(&req.prompt),
                                        provenance: gait_core::port::input::ProvenanceClaim::Rejected,
                                    }
                                } else {
                                    TurnInput::Prompt {
                                        text: content_blocks_to_text(&req.prompt),
                                        provenance: gait_core::port::input::ProvenanceClaim::Absent,
                                    }
                                }
                            }
                            // The fix: build every logged event body -- and
                            // every piece of model-facing rendered text --
                            // exclusively from the verified body_text, never
                            // from req.prompt. A compromised/buggy bridge
                            // that signs one message but sends different
                            // prompt content must never have that different
                            // text executed.
                            Ok(Some(batch)) => {
                                let (messages, confirm) = batch.into_parts();
                                let message_ids: Vec<String> = messages
                                    .iter()
                                    .map(|message| message.message_id.clone())
                                    .collect();
                                let source_record_id = message_ids
                                    .first()
                                    .cloned()
                                    .unwrap_or_default();
                                let author = messages.first().and_then(|message| {
                                    message.provenance.verified_author_id.clone()
                                });
                                let on_durable = attested_on_durable(
                                    source_record_id,
                                    sid.clone(),
                                    message_ids,
                                    author,
                                    batch_durable.clone(),
                                    confirm,
                                );
                                TurnInput::AttestedMessages {
                                    bodies: messages
                                        .into_iter()
                                        .map(|m| {
                                            (
                                                gait_core::proto::wire::body(
                                                    gait_core::proto::gait::log::v1::payload::Kind::IncomingMessage(
                                                        gait_core::proto::gait::log::v1::IncomingMessage {
                                                            content: m.body_text,
                                                            ..Default::default()
                                                        },
                                                    ),
                                                ),
                                                Some(m.provenance),
                                            )
                                        })
                                        .collect(),
                                    on_durable,
                                }
                            }
                        };

                        let handle = gait_core::embed::CascadeKillHandle::new();
                        map.set_cascade(&sid, handle.clone());
                        let cancel = tokio_util::sync::CancellationToken::new();
                        map.set_cancel(&sid, cancel.clone());
                        let map_thread = map.clone();
                        let sid_thread = sid.clone();
                        let key_thread = idempotency_key.clone();
                        let durable_thread = batch_durable.clone();
                        let caps_thread = caps.clone();
                        let join = std::thread::spawn(move || {
                            // The TurnSource is created AND driven entirely on this
                            // thread's current-thread runtime — nothing !Send ever
                            // crosses a thread boundary, so the ?Send "drive inline"
                            // invariant (src/port/input.rs:13-21) holds. The thread
                            // exists only because the signature is !Send.
                            let rt = tokio::runtime::Builder::new_current_thread()
                                .enable_all()
                                .build()
                                .expect("current-thread runtime");
                            let local = tokio::task::LocalSet::new();
                            let mut sink = AcpTurnSink::new(cx.clone(), session_id.clone());
                            // Install the fs delegate iff the client advertised BOTH
                            // fs methods; otherwise None -> disk I/O under the sandbox
                            // (today's behavior). The spine still authorizes every
                            // path before the delegate is called (tool.rs:74-97).
                            let fs_delegate: Option<
                                std::sync::Arc<dyn gait_core::embed::FsDelegate>,
                            > = if caps_thread.delegable() {
                                Some(std::sync::Arc::new(crate::fs_delegate::AcpFsDelegate {
                                    cx: cx.clone(),
                                    session_id: session_id.clone(),
                                }))
                            } else {
                                None
                            };
                            let source: AcpTurnSource = match turn_input {
                                TurnInput::Prompt { text, provenance } => AcpTurnSource::new_with_claim(
                                    text,
                                    gait_core::git::Repos::production(),
                                    codebase.clone(),
                                    cx,
                                    session_id,
                                    gait_dir.clone(),
                                    cancel,
                                    provenance,
                                ),
                                TurnInput::AttestedMessages { bodies, on_durable } => AcpTurnSource::new_with_bodies(
                                    bodies,
                                    on_durable,
                                    cx,
                                    session_id,
                                    gait_dir.clone(),
                                    cancel,
                                ),
                            };
                            let opts = gait_turn_runner::TurnRunOpts {
                                run_id: format!("acp-{sid_thread}"),
                                session_id: sid_thread.clone(),
                                continue_session: sid_thread.clone(),
                                gait_dir,
                                codebase,
                                args: Vec::new(),
                                sandbox_on: None,
                                cascade_group_arg: None,
                                cascade: Some(handle),
                                // The real trigger content lives in `source`
                                // below (a live TurnSource), not here.
                                profile: None,
                                fs_delegate,
                                allow_intree: false,
                            };
                            let outcome = local.block_on(
                                &rt,
                                gait_turn_runner::run_turn_source(
                                    opts,
                                    Box::new(source),
                                    &mut sink,
                                )
                                .instrument(span),
                            );
                            map_thread.take_cascade(&sid_thread);
                            // A failed turn whose trigger never became durable did not
                            // run, so release its idempotency key for the retry.
                            if outcome.is_err()
                                && !durable_thread.load(std::sync::atomic::Ordering::SeqCst)
                            {
                                if let Some(key) = &key_thread {
                                    map_thread.forget_idempotency_key(&sid_thread, key);
                                }
                            }
                            let stop = if map_thread.was_cancelled(&sid_thread) {
                                // Cancel wins over whatever the sink captured (or didn't):
                                // spec requires Cancelled "even if cancellation causes
                                // exceptions in underlying operations".
                                Ok(StopReason::Cancelled)
                            } else {
                                match outcome {
                                    Err(_e) if map_thread.was_cancelled(&sid_thread) => {
                                        Ok(StopReason::Cancelled)
                                    }
                                    Err(e) => Err(e),
                                    Ok(_) => match sink.take_result() {
                                        Some(tr) => turn_result_to_stop_reason(&tr),
                                        None => Err(coded_error::into_anyhow(PromptError::Internal(
                                            "turn produced no result".into(),
                                        ))),
                                    },
                                }
                            };
                            // Resolve the deferred prompt; a failed send just means the
                            // connection already closed (drain will reap this thread).
                            let _ = match stop {
                                Ok(reason) => responder.respond(PromptResponse::new(reason)),
                                Err(e) => responder.respond_with_error(error::acp_error(&e, error::PROMPT)),
                            };
                        });
                        // Track the thread so a connection-teardown drain can reap it
                        // (design §4a). Set after spawn; if the turn already finished,
                        // join() at drain returns instantly.
                        map.set_turn_thread(&sid, join);
                        Ok(())
                    }
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let map = map.clone();
                move |req: SetSessionModeRequest,
                      responder: Responder<SetSessionModeResponse>,
                      cx: agent_client_protocol::ConnectionTo<Client>| {
                    let map = map.clone();
                    async move {
                        let session_id = req.session_id.clone();
                        let sid = session_id.0.to_string();
                        let Some((gait_dir, _codebase)) = map.resolve(&sid) else {
                            return responder.respond_with_error(error::acp_error(&coded_error::into_anyhow(PromptError::SessionUnknown(sid.clone())), error::SET_SESSION_MODE));
                        };
                        let mode_id = req.mode_id.clone();
                        let log_path = gait_core::session::live::log_path(&gait_dir, &sid);
                        match gait_core::modes::activate_mode(
                            &log_path,
                            &gait_dir,
                            mode_id.0.as_ref(),
                        ) {
                            Ok(()) => {
                                // Confirm the active mode to the client (ACP contract).
                                // send_notification returns a #[must_use] Result; bind it
                                // like turn_io.rs:345 — a failed send just means the
                                // connection closed, which the caller will observe anyway.
                                let _ = cx.send_notification(SessionNotification::new(
                                    session_id,
                                    SessionUpdate::CurrentModeUpdate(CurrentModeUpdate::new(
                                        mode_id,
                                    )),
                                ));
                                responder.respond(SetSessionModeResponse::new())
                            }
                            Err(e) => responder.respond_with_error(error::acp_error(&e, error::SET_SESSION_MODE)),
                        }
                    }
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let map = map.clone();
                move |req: ListSessionsRequest, responder: Responder<ListSessionsResponse>, _cx| {
                    let map = map.clone();
                    async move { lifecycle::list_sessions(&map, req, responder) }
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let map = map.clone();
                move |req: CloseSessionRequest, responder: Responder<CloseSessionResponse>, _cx| {
                    let map = map.clone();
                    async move { lifecycle::close_session(&map, req, responder) }
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let map = map.clone();
                move |req: DeleteSessionRequest,
                      responder: Responder<DeleteSessionResponse>,
                      _cx| {
                    let map = map.clone();
                    async move { lifecycle::delete_session(&map, req, responder) }
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            {
                let map = map.clone();
                move |n: CancelNotification, _cx| {
                    let map = map.clone();
                    async move {
                        // Fire both signals; each no-ops if its state isn't the live
                        // one. Return immediately — this runs inside the dispatch loop.
                        map.cancel(n.session_id.0.as_ref());
                        Ok(())
                    }
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meta_getter_reads_string_values_only() {
        let mut meta = serde_json::Map::new();
        meta.insert("traceparent".into(), serde_json::json!("00-abc-def-01"));
        meta.insert("number".into(), serde_json::json!(7));
        let meta = Some(meta);
        let getter = meta_getter(&meta);
        assert_eq!(getter("traceparent"), Some("00-abc-def-01".into()));
        assert_eq!(getter("missing"), None);
        assert_eq!(getter("number"), None);
    }

    #[test]
    fn prompt_accepted_emits_once_for_a_direct_prompt() {
        let cap = concord_telemetry::testing::capture_exclusive();
        let id = format!("direct-{}", std::process::id());
        emit_prompt_accepted(
            id.clone(),
            "session-direct".into(),
            vec![id.clone()],
            gait_events::gait::events::v1::PromptSource::Direct,
            None,
        );
        let event = cap.single_event_where::<gait_events::gait::events::v1::PromptAccepted>(
            |value| value.source_record_id == id,
        );
        assert_eq!(event.source, gait_events::gait::events::v1::PromptSource::Direct as i32);
        assert_eq!(event.message_count, 1);
        assert!(event.author.is_none());
    }

    #[test]
    fn prompt_accepted_emits_once_for_an_attested_batch_with_author() {
        let cap = concord_telemetry::testing::capture_exclusive();
        let id = format!("attested-{}", std::process::id());
        let author = gait_events::Principal::new(gait_events::PrincipalKind::Agent, "agent-1")
            .with_verified_by([
                gait_events::Verification::DeliveryAttestation,
                gait_events::Verification::DelegationCert,
            ]);
        emit_prompt_accepted(
            id.clone(),
            "session-attested".into(),
            vec![id.clone(), "second-message".into()],
            gait_events::gait::events::v1::PromptSource::Attested,
            Some(author),
        );
        let event = cap.single_event_where::<gait_events::gait::events::v1::PromptAccepted>(
            |value| value.source_record_id == id,
        );
        assert_eq!(event.source, gait_events::gait::events::v1::PromptSource::Attested as i32);
        assert_eq!(event.message_count, 2);
        assert_eq!(event.message_ids.len(), 2);
        assert!(event.author.is_some());
    }

    #[test]
    fn attested_on_durable_marks_durable_emits_once_and_confirms() {
        let cap = concord_telemetry::testing::capture_exclusive();
        let id = format!("wired-{}", std::process::id());
        let durable = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let confirmed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let confirmed_hook = confirmed.clone();
        let hook = attested_on_durable(
            id.clone(),
            "session-wired".into(),
            vec![id.clone(), "second".into()],
            Some("agent-1".into()),
            durable.clone(),
            move || {
                confirmed_hook.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            },
        );
        assert!(!durable.load(std::sync::atomic::Ordering::SeqCst));
        assert!(cap
            .events_where::<gait_events::gait::events::v1::PromptAccepted>(|v| v.source_record_id == id)
            .is_empty());
        hook().unwrap();
        assert!(durable.load(std::sync::atomic::Ordering::SeqCst));
        assert!(confirmed.load(std::sync::atomic::Ordering::SeqCst));
        let event = cap.single_event_where::<gait_events::gait::events::v1::PromptAccepted>(
            |v| v.source_record_id == id,
        );
        assert_eq!(event.message_count, 2);
        assert!(event.author.is_some());
    }

    #[test]
    fn dropped_attested_on_durable_emits_nothing() {
        let cap = concord_telemetry::testing::capture_exclusive();
        let id = format!("dropped-{}", std::process::id());
        let hook = attested_on_durable(
            id.clone(),
            "session-dropped".into(),
            vec![id.clone()],
            None,
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            || Ok(()),
        );
        drop(hook);
        assert!(cap
            .events_where::<gait_events::gait::events::v1::PromptAccepted>(|v| v.source_record_id == id)
            .is_empty());
    }

    #[test]
    fn rejected_attestation_emits_one_error_record_and_wire_carries_its_code() {
        let cap = concord_telemetry::testing::capture_exclusive();
        let mut meta = serde_json::Map::new();
        meta.insert(
            message_attestation::DEFAULT_META_KEY.to_string(),
            serde_json::json!({}),
        );
        // No attestor: the real verify_prompt refuses the attestation.
        let err = crate::attestation::verify_prompt(None, &Some(meta))
            .err()
            .expect("an attestation without an attestor is rejected");
        let wire = reject_attestation(&err);
        let data = wire.data.as_ref().unwrap();
        assert_eq!(data["code"], "gait.acp.prompt.attestation_missing_own_devices");
        assert_eq!(data["class"], "logical");
        let fingerprint = data["fingerprint"].clone();
        let records = cap.errors_where(|record| record.fingerprint == fingerprint);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].code, "gait.acp.prompt.attestation_missing_own_devices");
        assert_eq!(cap.errors_where(|_| true).len(), 1);
    }

    #[test]
    fn missing_required_attestation_is_an_attestation_required_error() {
        let cap = concord_telemetry::testing::capture_exclusive();
        let err = coded_error::into_anyhow(PromptError::AttestationRequired);
        let wire = reject_attestation(&err);
        assert_eq!(wire.data.as_ref().unwrap()["code"], "gait.acp.prompt.attestation_required");
        assert_eq!(cap.errors_where(|_| true).len(), 1);
    }

    #[test]
    fn advertise_lists_default_first_then_authored_with_descriptions() {
        let t = tempfile::TempDir::new().unwrap();
        let gd = t.path();
        let write = |name: &str, body: &str| {
            let d = gd.join("modes").join(name);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("MODE.md"), body).unwrap();
        };
        write("plan", "---\ndeny: [write]\ndescription: Plan only\n---\n");
        write("code", "---\n---\ngo\n"); // no description
        write("broken", "---\ndeny: [unclosed\n---\n"); // malformed YAML

        let state = advertise_modes(gd);
        assert_eq!(state.current_mode_id.0.as_ref(), "default");
        let ids: Vec<String> = state
            .available_modes
            .iter()
            .map(|m| m.id.0.to_string())
            .collect();
        assert_eq!(ids[0], "default", "baseline advertised first");
        assert!(ids.contains(&"plan".to_string()));
        assert!(ids.contains(&"code".to_string()));
        assert!(
            ids.contains(&"broken".to_string()),
            "malformed still listed"
        );

        let plan = state
            .available_modes
            .iter()
            .find(|m| m.id.0.as_ref() == "plan")
            .unwrap();
        assert_eq!(plan.description.as_deref(), Some("Plan only"));
        let code = state
            .available_modes
            .iter()
            .find(|m| m.id.0.as_ref() == "code")
            .unwrap();
        assert_eq!(code.description, None, "empty description -> None");
        let broken = state
            .available_modes
            .iter()
            .find(|m| m.id.0.as_ref() == "broken")
            .unwrap();
        assert_eq!(broken.description, None, "malformed -> None, never a panic");
    }
}

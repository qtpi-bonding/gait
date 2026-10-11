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

use crate::attestation::TrustedMultimatrixKey;
use crate::lifecycle;
use crate::session_map::{SessionMap, discover_gait_dir_for_cwd};
use crate::turn_io::{
    AcpTurnSink, AcpTurnSource, content_blocks_to_text, turn_result_to_stop_reason,
};

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
    build_agent_with_attestation(map, false, Vec::new())
}

/// Like [`build_agent`], but with the attestation-verification policy that
/// governs `session/prompt`: `require_attestation` is a single process-wide
/// `bool` (not per-session state -- the trust decision has to be rooted in
/// how the process itself was launched, e.g. by `multimatrix-bridge`, not a
/// runtime `_meta` claim), and `trusted_keys` pins which multimatrix service
/// keys a `DeliveryAttestation` may be signed by.
pub fn build_agent_with_attestation(
    map: SessionMap,
    require_attestation: bool,
    trusted_keys: Vec<TrustedMultimatrixKey>,
) -> Builder<Agent, impl HandleDispatchFrom<Client>, NullRun> {
    // Connection-level: written by the `initialize` handler below, read by each
    // `session/prompt` before it installs the fs delegate (design §1).
    let caps = std::sync::Arc::new(crate::fs_delegate::ClientFsCaps::default());
    let trusted_keys = std::sync::Arc::new(trusted_keys);
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
                            Err(e) => return responder.respond_with_internal_error(e.to_string()),
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
                let trusted_keys = trusted_keys.clone();
                move |req: PromptRequest,
                      responder: Responder<PromptResponse>,
                      cx: agent_client_protocol::ConnectionTo<Client>| {
                    let map = map.clone();
                    let caps = caps.clone();
                    let trusted_keys = trusted_keys.clone();
                    async move {
                        let session_id = req.session_id.clone();
                        let sid = session_id.0.to_string();
                        let Some((gait_dir, codebase)) = map.resolve(&sid) else {
                            return responder
                                .respond_with_internal_error(format!("unknown session: {sid}"));
                        };
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
                                return responder.respond_with_error(
                                    crate::idempotency::already_seen_error(key),
                                );
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
                            return responder.respond_with_internal_error(format!(
                                "session already has an in-flight turn: {sid}"
                            ));
                        }
                        // Only now -- past every admission check, committed to
                        // actually running this turn -- record the idempotency
                        // key. Recording earlier would permanently blacklist a
                        // key whose prompt was rejected above and never ran.
                        if let Some(key) = &idempotency_key {
                            map.record_idempotency_key(&sid, key);
                        }
                        // Mandatory attestation check. `own_device_public_key`
                        // is loaded unconditionally (cheap, and
                        // verify_and_build_messages returns Ok(None) before
                        // ever touching it when `_meta` carries no
                        // attestation at all) -- an empty fallback on a
                        // keyring-load failure is safe: it simply can never
                        // match a real recipient list, so a genuine
                        // attestation still gets correctly rejected rather
                        // than silently bypassing the check.
                        let own_device_public_key: Vec<u8> =
                            crate::attestation::load_own_device_public_key(&gait_dir);
                        let replay_store =
                            match crate::attestation_replay::AttestationReplayStore::new(&gait_dir) {
                                Ok(store) => store,
                                Err(e) => {
                                    return responder.respond_with_internal_error(format!(
                                        "opening attestation replay store: {e}"
                                    ));
                                }
                            };
                        let verified = crate::attestation::verify_and_build_messages(
                            &req.meta,
                            &own_device_public_key,
                            &trusted_keys,
                            &replay_store,
                        );
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
                            },
                        }
                        let turn_input = match verified {
                            // A supplied but invalid attestation is retained
                            // as rejected user text in the durable log. The
                            // core fold excludes it from model context.
                            Err(_e) => TurnInput::Prompt {
                                text: content_blocks_to_text(&req.prompt),
                                provenance: gait_core::port::input::ProvenanceClaim::Rejected,
                            },
                            // No attestation present at all.
                            Ok(None) => {
                                if require_attestation {
                                    TurnInput::Prompt {
                                        text: content_blocks_to_text(&req.prompt),
                                        provenance: gait_core::port::input::ProvenanceClaim::Rejected,
                                    }
                                } else {
                                    TurnInput::Prompt {
                                        text: content_blocks_to_text(&req.prompt),
                                        provenance: crate::meta_provenance::provenance_from_meta(
                                            &req.meta,
                                        ),
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
                            Ok(Some(messages)) => TurnInput::AttestedMessages {
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
                            },
                        };

                        let handle = gait_core::embed::CascadeKillHandle::new();
                        map.set_cascade(&sid, handle.clone());
                        let cancel = tokio_util::sync::CancellationToken::new();
                        map.set_cancel(&sid, cancel.clone());
                        let map_thread = map.clone();
                        let sid_thread = sid.clone();
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
                                TurnInput::AttestedMessages { bodies } => AcpTurnSource::new_with_bodies(
                                    bodies,
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
                                ),
                            );
                            map_thread.take_cascade(&sid_thread);
                            let stop = if map_thread.was_cancelled(&sid_thread) {
                                // Cancel wins over whatever the sink captured (or didn't):
                                // spec requires Cancelled "even if cancellation causes
                                // exceptions in underlying operations".
                                Ok(StopReason::Cancelled)
                            } else {
                                match outcome {
                                    Err(e) if map_thread.was_cancelled(&sid_thread) => {
                                        Ok(StopReason::Cancelled)
                                    }
                                    Err(e) => Err(e.to_string()),
                                    Ok(_) => match sink.take_result() {
                                        Some(tr) => turn_result_to_stop_reason(&tr),
                                        None => Err("turn produced no result".into()),
                                    },
                                }
                            };
                            // Resolve the deferred prompt; a failed send just means the
                            // connection already closed (drain will reap this thread).
                            let _ = match stop {
                                Ok(reason) => responder.respond(PromptResponse::new(reason)),
                                Err(msg) => responder.respond_with_internal_error(msg),
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
                            return responder
                                .respond_with_internal_error(format!("unknown session: {sid}"));
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
                            Err(e) => responder.respond_with_internal_error(e.to_string()),
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

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
    CancelNotification, InitializeRequest, InitializeResponse, NewSessionRequest,
    NewSessionResponse, PromptRequest, PromptResponse, StopReason,
};
use agent_client_protocol::{Agent, Builder, Client, HandleDispatchFrom, NullRun, Responder};

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
pub fn build_agent(map: SessionMap) -> Builder<Agent, impl HandleDispatchFrom<Client>, NullRun> {
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
                        // never arrives (design §5). load_session stays false (default).
                        // Record the client's fs capabilities for the whole
                        // connection. All-or-nothing gating happens at prompt time.
                        let fs = req.client_capabilities.fs;
                        caps.set(fs.read_text_file, fs.write_text_file);
                        responder.respond(InitializeResponse::new(req.protocol_version))
                    }
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
                        map.insert(gait_dir, session_id.clone(), req.cwd.clone());
                        responder.respond(NewSessionResponse::new(session_id))
                    }
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let map = map.clone();
                let caps = caps.clone();
                move |req: PromptRequest,
                      responder: Responder<PromptResponse>,
                      cx: agent_client_protocol::ConnectionTo<Client>| {
                    let map = map.clone();
                    let caps = caps.clone();
                    async move {
                        let session_id = req.session_id.clone();
                        let sid = session_id.0.to_string();
                        let Some((gait_dir, codebase)) = map.resolve(&sid) else {
                            return responder
                                .respond_with_internal_error(format!("unknown session: {sid}"));
                        };
                        let text = content_blocks_to_text(&req.prompt);

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
                            let mut obs = gait_workflow::observer::NullObserver;
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
                            let source = AcpTurnSource::new(
                                text,
                                gait_core::git::Repos::production(),
                                codebase.clone(),
                                cx,
                                session_id,
                                gait_dir.clone(),
                                cancel,
                            );
                            let opts = gait_workflow::SinglePromptOpts {
                                run_id: format!("acp-{sid_thread}"),
                                continue_session: sid_thread.clone(),
                                gait_dir,
                                codebase,
                                sandbox_on: None,
                                cascade: Some(handle),
                                fs_delegate,
                                allow_intree: false,
                                ..Default::default()
                            };
                            let outcome = local.block_on(
                                &rt,
                                gait_workflow::single_prompt_run(
                                    opts,
                                    &mut sink,
                                    &mut obs,
                                    Some(Box::new(source)),
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

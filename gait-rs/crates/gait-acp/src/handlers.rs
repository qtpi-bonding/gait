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
    InitializeRequest, InitializeResponse, NewSessionRequest, NewSessionResponse,
};
use agent_client_protocol::{Agent, Client, ConnectTo, Responder};

use crate::session_map::{SessionMap, discover_gait_dir_for_cwd};

/// Build the gait-acp agent: registers `initialize` and `session/new` against
/// the given `SessionMap`. Returns an opaque `ConnectTo<Client>` so callers can
/// either `.connect_to(transport)` (real stdio) or `.into_channel_and_future()`
/// (in-process, for tests).
pub fn build_agent(map: SessionMap) -> impl ConnectTo<Client> {
    Agent
        .builder()
        .name("gait-acp")
        .on_receive_request(
            async |req: InitializeRequest, responder: Responder<InitializeResponse>, _cx| {
                // Static/minimal capabilities; zero auth methods so `authenticate`
                // never arrives (design §5). load_session stays false (default).
                responder.respond(InitializeResponse::new(req.protocol_version))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |req: NewSessionRequest, responder: Responder<NewSessionResponse>, _cx| {
                let gait_dir = match discover_gait_dir_for_cwd(&req.cwd) {
                    Ok(dir) => dir,
                    Err(e) => return responder.respond_with_internal_error(e.to_string()),
                };
                let session_id = gait_core::session::new_session_id();
                map.insert(gait_dir, session_id.clone());
                responder.respond(NewSessionResponse::new(session_id))
            },
            agent_client_protocol::on_receive_request!(),
        )
}

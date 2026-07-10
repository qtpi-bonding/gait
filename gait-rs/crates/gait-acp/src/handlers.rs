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
use agent_client_protocol::{Agent, Builder, Client, HandleDispatchFrom, NullRun, Responder};

use crate::session_map::{SessionMap, discover_gait_dir_for_cwd};

/// Build the gait-acp agent: registers `initialize` and `session/new` against
/// the given `SessionMap`. Returns the concrete `Builder` (not just an opaque
/// `ConnectTo<Client>`) so `build_agent_with_slow_handler` (Task 5) can chain
/// one more `.on_receive_request` onto it; the builder itself still
/// implements `ConnectTo<Client>` for `.connect_to(transport)` /
/// `.into_channel_and_future()`.
pub fn build_agent(map: SessionMap) -> Builder<Agent, impl HandleDispatchFrom<Client>, NullRun> {
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
                map.insert(gait_dir, session_id.clone(), req.cwd.clone());
                responder.respond(NewSessionResponse::new(session_id))
            },
            agent_client_protocol::on_receive_request!(),
        )
}

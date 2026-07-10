//! Integration tests for the Phase 1 connection skeleton. Drives the agent
//! over the SDK's in-process `Channel` transport instead of real stdio.
use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::InitializeRequest;
use agent_client_protocol::{Client, ConnectTo};

use gait_acp::session_map::SessionMap;

/// Build a fresh agent + client-side connection over an in-process channel,
/// and run `main_fn` with the client's `ConnectionTo<Agent>`.
async fn drive<R>(
    map: SessionMap,
    main_fn: impl AsyncFnOnce(
        agent_client_protocol::ConnectionTo<agent_client_protocol::Agent>,
    ) -> agent_client_protocol::Result<R>,
) -> agent_client_protocol::Result<R> {
    let (channel, server_future) = gait_acp::build_agent(map).into_channel_and_future();
    tokio::spawn(server_future);
    Client.connect_with(channel, main_fn).await
}

async fn drive_initialize() -> agent_client_protocol::schema::v1::InitializeResponse {
    drive(SessionMap::new(), async |cx| {
        cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
            .block_task()
            .await
    })
    .await
    .expect("initialize should succeed")
}

#[tokio::test]
async fn initialize_returns_capabilities_with_zero_auth_methods() {
    let resp = drive_initialize().await;
    assert!(
        resp.auth_methods.is_empty(),
        "must advertise zero auth methods"
    );
    assert_eq!(resp.protocol_version, ProtocolVersion::V1);
}

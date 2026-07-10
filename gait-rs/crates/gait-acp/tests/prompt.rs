//! Phase 2 integration tests for session/prompt, driven over the SDK's
//! in-process channel (same harness as tests/skeleton.rs).
use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{InitializeRequest, PromptRequest, SessionId};
use agent_client_protocol::{Client, ConnectTo};
use gait_acp::session_map::SessionMap;

#[tokio::test]
async fn prompt_unknown_session_errors_not_panics() {
    let map = SessionMap::new();
    let (channel, server) = gait_acp::build_agent(map).into_channel_and_future();
    tokio::spawn(server);

    let (prompt_result, init_ok) = Client
        .connect_with(channel, async |cx| {
            let prompt_result = cx
                .send_request(PromptRequest::new(
                    SessionId::new("never-minted"),
                    vec!["hi".to_string().into()],
                ))
                .block_task()
                .await;
            // Connection must survive a bad prompt.
            let init = cx
                .send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            Ok((prompt_result, init))
        })
        .await
        .expect("connection stays alive");

    assert!(
        prompt_result.is_err(),
        "unknown session id -> JSON-RPC error"
    );
    assert!(init_ok.auth_methods.is_empty());
}

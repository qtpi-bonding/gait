//! Integration tests for the Phase 1 connection skeleton. Drives the agent
//! over the SDK's in-process `Channel` transport instead of real stdio.
use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{InitializeRequest, NewSessionRequest};
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

/// Drive `session/new` for `cwd` over a fresh agent, returning the minted
/// session id and the `SessionMap` the agent was built with (so the caller
/// can assert bookkeeping).
async fn drive_session_new(
    cwd: &std::path::Path,
) -> agent_client_protocol::Result<(String, SessionMap)> {
    let map = SessionMap::new();
    let (channel, server_future) = gait_acp::build_agent(map.clone()).into_channel_and_future();
    tokio::spawn(server_future);
    let cwd = cwd.to_path_buf();
    let session_id = Client
        .connect_with(channel, async |cx| {
            let resp = cx
                .send_request(NewSessionRequest::new(cwd))
                .block_task()
                .await?;
            Ok(resp.session_id.to_string())
        })
        .await?;
    Ok((session_id, map))
}

#[tokio::test]
async fn session_new_in_gait_workspace_returns_session_id_and_records_it() {
    // A repo dir with a `.gaitref` pointer file naming an out-of-tree gait-dir.
    let t = tempfile::TempDir::new().unwrap();
    let gait_dir = t.path().join("home");
    std::fs::create_dir_all(&gait_dir).unwrap();
    let repo = t.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(
        repo.join(".gaitref"),
        format!("gaitdir: {}\n", gait_dir.display()),
    )
    .unwrap();

    let (session_id, map) = drive_session_new(&repo).await.expect("ok");
    assert!(!session_id.is_empty());
    assert!(map.contains(&gait_dir, &session_id));
}

#[tokio::test]
async fn session_new_without_gait_dir_errors_not_panics() {
    let t = tempfile::TempDir::new().unwrap(); // no `.gaitref`/`.gait/` anywhere above

    // Drive both requests over the SAME connection: the point of this test is
    // that a bad session/new must not drop the stdio connection.
    let map = SessionMap::new();
    let (channel, server_future) = gait_acp::build_agent(map).into_channel_and_future();
    tokio::spawn(server_future);
    let cwd = t.path().to_path_buf();
    let (session_result, init_resp) = Client
        .connect_with(channel, async |cx| {
            let session_result = cx
                .send_request(NewSessionRequest::new(cwd))
                .block_task()
                .await;
            let init_resp = cx
                .send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            Ok((session_result, init_resp))
        })
        .await
        .expect("connection must stay alive");

    assert!(
        session_result.is_err(),
        "a cwd with no gait-dir must return a JSON-RPC error"
    );
    assert!(init_resp.auth_methods.is_empty());
}

#[tokio::test]
async fn new_session_outside_workspace_is_coded_not_found() {
    let t = tempfile::TempDir::new().unwrap();
    let Err(err) = drive_session_new(t.path()).await else {
        panic!("a cwd with no gait-dir must fail");
    };
    let data = err.data.expect("error data");
    assert_eq!(data["code"], "gait.state.gait_dir.not_found");
    assert_eq!(data["class"], "logical");
}

/// Task 5: a handler that spawns work and returns immediately must not block
/// the read loop from serving other requests. Fires a slow request (does NOT
/// await its response yet), then fires `initialize` and asserts THAT resolves
/// promptly — proving the slow handler didn't block the read loop. Only then
/// releases the oneshot the slow handler is waiting on and awaits its
/// response.
#[tokio::test]
async fn read_loop_stays_free_while_a_handler_is_in_flight() {
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let map = SessionMap::new();
    let (channel, server_future) =
        gait_acp::build_agent_with_slow_handler(map, release_rx).into_channel_and_future();
    tokio::spawn(server_future);

    let (init_resp, _slow_resp) = Client
        .connect_with(channel, async |cx| {
            // Fire the slow request; do NOT await its response yet.
            let slow = cx.send_request(gait_acp::SlowRequest {});

            // This must resolve promptly even though the slow handler above
            // is still parked on the (unreleased) oneshot.
            let init_resp = cx
                .send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;

            release_tx.send(()).ok();
            let slow_resp = slow.block_task().await?;
            Ok((init_resp, slow_resp))
        })
        .await
        .expect("connection should stay alive");

    assert!(init_resp.auth_methods.is_empty());
}

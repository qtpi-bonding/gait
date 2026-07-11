//! Phase 5: the ACP fs-delegation proxy. Drives `AcpFsDelegate` through a real
//! in-process agent<->client connection via the `_gait_acp_test/fs_probe` hook
//! (build_agent_with_fs_probe). The client serves fs/read_text_file &
//! fs/write_text_file from an in-memory buffer map; a path containing
//! "err-read"/"err-write" makes that handler return a JSON-RPC error.
use agent_client_protocol::schema::v1::{
    ReadTextFileRequest, ReadTextFileResponse, SessionId, WriteTextFileRequest,
    WriteTextFileResponse,
};
use agent_client_protocol::{Client, ConnectTo, Responder};
use gait_acp::session_map::SessionMap;
use gait_acp::{FsProbeRequest, FsProbeResponse};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

type Fs = Arc<Mutex<HashMap<String, String>>>;

/// Connect a client (serving fs from `fs`) to the fs-probe agent, send one
/// probe, and return its response.
async fn run_probe(req: FsProbeRequest, fs: Fs) -> FsProbeResponse {
    let map = SessionMap::new();
    let (channel, server) = gait_acp::build_agent_with_fs_probe(map).into_channel_and_future();
    tokio::spawn(server);
    Client
        .builder()
        .on_receive_request(
            {
                let fs = fs.clone();
                move |r: ReadTextFileRequest, resp: Responder<ReadTextFileResponse>, _cx| {
                    let fs = fs.clone();
                    async move {
                        let p = r.path.to_string_lossy().to_string();
                        if p.contains("err-read") {
                            return resp.respond_with_internal_error("no such file".to_string());
                        }
                        let content = fs.lock().unwrap().get(&p).cloned().unwrap_or_default();
                        resp.respond(ReadTextFileResponse::new(content))
                    }
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let fs = fs.clone();
                move |r: WriteTextFileRequest, resp: Responder<WriteTextFileResponse>, _cx| {
                    let fs = fs.clone();
                    async move {
                        let p = r.path.to_string_lossy().to_string();
                        if p.contains("err-write") {
                            return resp.respond_with_internal_error("disk full".to_string());
                        }
                        fs.lock().unwrap().insert(p, r.content.clone());
                        resp.respond(WriteTextFileResponse::new())
                    }
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(channel, async move |cx| {
            Ok(cx
                .send_request(req)
                .block_task()
                .await
                .expect("probe responds"))
        })
        .await
        .expect("connection stays alive")
}

#[tokio::test]
async fn delegated_read_returns_buffer() {
    let fs: Fs = Arc::new(Mutex::new(HashMap::new()));
    fs.lock()
        .unwrap()
        .insert("/w/a.txt".into(), "buffered!".into());
    let resp = run_probe(
        FsProbeRequest {
            session_id: SessionId::new("s"),
            path: "/w/a.txt".into(),
            write: None,
        },
        fs,
    )
    .await;
    assert_eq!(resp.content.as_deref(), Some("buffered!"));
    assert_eq!(resp.error, None);
}

#[tokio::test]
async fn delegated_write_persists_to_client() {
    let fs: Fs = Arc::new(Mutex::new(HashMap::new()));
    let resp = run_probe(
        FsProbeRequest {
            session_id: SessionId::new("s"),
            path: "/w/b.txt".into(),
            write: Some("new content".into()),
        },
        fs.clone(),
    )
    .await;
    assert!(resp.wrote);
    assert_eq!(resp.error, None);
    assert_eq!(
        fs.lock().unwrap().get("/w/b.txt").map(String::as_str),
        Some("new content")
    );
}

#[tokio::test]
async fn read_error_maps_to_none_not_err() {
    let fs: Fs = Arc::new(Mutex::new(HashMap::new()));
    let resp = run_probe(
        FsProbeRequest {
            session_id: SessionId::new("s"),
            path: "/err-read/x".into(),
            write: None,
        },
        fs,
    )
    .await;
    // Error -> empty/new-file: Ok(None), NOT Err. The swallow is the point.
    assert_eq!(resp.content, None);
    assert_eq!(
        resp.error, None,
        "a read error must be swallowed to None, not surfaced"
    );
}

#[tokio::test]
async fn write_error_surfaces_as_err() {
    let fs: Fs = Arc::new(Mutex::new(HashMap::new()));
    let resp = run_probe(
        FsProbeRequest {
            session_id: SessionId::new("s"),
            path: "/err-write/x".into(),
            write: Some("x".into()),
        },
        fs,
    )
    .await;
    assert!(!resp.wrote);
    assert!(resp.error.is_some(), "a write error must surface as Err");
}

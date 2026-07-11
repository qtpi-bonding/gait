//! Phase 5: the ACP fs-delegation proxy. Drives `AcpFsDelegate` through a real
//! in-process agent<->client connection via the `_gait_acp_test/fs_probe` hook
//! (build_agent_with_fs_probe). The client serves fs/read_text_file &
//! fs/write_text_file from an in-memory buffer map; a path containing
//! "err-read"/"err-write" makes that handler return a JSON-RPC error.
use agent_client_protocol::schema::v1::{
    ClientCapabilities, FileSystemCapabilities, InitializeRequest, NewSessionRequest,
    PromptRequest, ReadTextFileRequest, ReadTextFileResponse, SessionId, StopReason,
    WriteTextFileRequest, WriteTextFileResponse,
};
// ProtocolVersion lives at schema:: (sibling of v1), NOT inside schema::v1 —
// matching the existing tests/skeleton.rs:3 and tests/prompt.rs:3 convention.
use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::{Client, ConnectTo, Responder};
use gait_acp::session_map::SessionMap;
use gait_acp::{FsProbeRequest, FsProbeResponse};
use gait_proto::gait::sandbox::v1::{Append, Signal, StreamForward, Terminated, signal};
use prost::Message;
use std::collections::HashMap;
use std::path::Path;
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

fn frame(sig: &Signal) -> Vec<u8> {
    let b = sig.encode_to_vec();
    let mut f = vec![1u8];
    f.extend_from_slice(&(b.len() as u32).to_be_bytes());
    f.extend_from_slice(&b);
    f
}

fn init_git_repo(dir: &Path) {
    use std::process::Command;
    let run = |args: &[&str]| {
        Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t.com")
            .output()
            .unwrap();
    };
    run(&["init"]);
    run(&["commit", "--allow-empty", "-m", "init"]);
    let _ = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["branch", "-m", "master", "main"])
        .output();
}

/// A gait-init'd workspace with the mock brain wired in; returns (tmp, gait_dir, repo).
fn workspace() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    unsafe { std::env::set_var("GAIT_BRAIN_EXE", env!("CARGO_BIN_EXE_mock_brain_acp")) };
    let t = tempfile::TempDir::new().unwrap();
    let gait_dir = t.path().join("home");
    std::fs::create_dir_all(&gait_dir).unwrap();
    init_git_repo(&gait_dir);
    let repo = t.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_git_repo(&repo);
    std::fs::write(
        repo.join(".gaitref"),
        format!("gaitdir: {}\n", gait_dir.display()),
    )
    .unwrap();
    std::fs::write(gait_dir.join("gait.jsonc"), r#"{ "model": "mock/model" }"#).unwrap();
    (t, gait_dir, repo)
}

fn forward_event(body: gait_core::proto::gait::log::v1::event::Body) -> Signal {
    use gait_core::proto::gait::log::v1::{Event, StreamItem, stream_item};
    Signal {
        corr_id: 0,
        body: Some(signal::Body::StreamForward(StreamForward {
            item: Some(StreamItem {
                item: Some(stream_item::Item::Event(Event {
                    body: Some(body),
                    ..Default::default()
                })),
            }),
        })),
    }
}

fn terminated(run_result_json: &str) -> Signal {
    Signal {
        corr_id: 0,
        body: Some(signal::Body::Terminated(Terminated {
            run_result_json: run_result_json.to_string(),
            failed: false,
        })),
    }
}

#[allow(dead_code)]
fn append_event(body: gait_core::proto::gait::log::v1::event::Body) -> Signal {
    use gait_core::proto::gait::log::v1::Event;
    Signal {
        corr_id: 0,
        body: Some(signal::Body::Append(Append {
            event: Some(Event {
                body: Some(body),
                ..Default::default()
            }),
        })),
    }
}

/// Drive a full prompt against the real `build_agent`, sending `initialize` with
/// the given fs capabilities first. Returns the prompt's StopReason result.
async fn prompt_with_caps(read: bool, write: bool) -> Result<StopReason, String> {
    let (_t, gait_dir, repo) = workspace();
    // A single "done" turn: no tool calls, so the delegate (if installed) is
    // constructed but never invoked — this proves the wiring, not delegation.
    let mut blob = Vec::new();
    for s in [
        forward_event(gait_core::proto::events::assistant_text("done".into())),
        terminated(r#"{"done":{"message":"ok"}}"#),
    ] {
        blob.extend_from_slice(&frame(&s));
    }
    std::fs::write(gait_dir.join("mock_frames.bin"), blob).unwrap();

    let map = SessionMap::new();
    let (channel, server) = gait_acp::build_agent(map).into_channel_and_future();
    tokio::spawn(server);
    Client
        .builder()
        .connect_with(channel, async move |cx| {
            // connect_with does NOT auto-send initialize; send it explicitly so the
            // agent captures our advertised fs capabilities.
            cx.send_request(
                InitializeRequest::new(ProtocolVersion::V1).client_capabilities(
                    ClientCapabilities::new().fs(FileSystemCapabilities::new()
                        .read_text_file(read)
                        .write_text_file(write)),
                ),
            )
            .block_task()
            .await?;
            let sid = cx
                .send_request(NewSessionRequest::new(repo.clone()))
                .block_task()
                .await?
                .session_id;
            let result = match cx
                .send_request(PromptRequest::new(sid, vec!["go".to_string().into()]))
                .block_task()
                .await
            {
                Ok(resp) => Ok(resp.stop_reason),
                Err(e) => Err(e.to_string()),
            };
            Ok(result)
        })
        .await
        .expect("connection stays alive")
}

#[tokio::test]
async fn full_fs_caps_turn_completes_with_delegate_installed() {
    // read && write -> delegate installed; a no-tool turn still completes.
    let result = prompt_with_caps(true, true).await;
    assert_eq!(result, Ok(StopReason::EndTurn));
}

#[tokio::test]
async fn no_fs_caps_turn_completes_with_disk_fallback() {
    // partial/absent caps -> delegate NOT installed (None); same turn completes.
    let result = prompt_with_caps(false, false).await;
    assert_eq!(result, Ok(StopReason::EndTurn));
}

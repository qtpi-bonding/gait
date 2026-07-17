//! Phase 4 integration tests: session/cancel in both turn phases resolves the
//! in-flight prompt with StopReason::Cancelled.
use agent_client_protocol::schema::v1::{
    CancelNotification, NewSessionRequest, PermissionOptionKind, PromptRequest,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    SelectedPermissionOutcome, SessionId, StopReason,
};
use agent_client_protocol::{Client, ConnectTo, Responder};
use gait_acp::session_map::SessionMap;
use gait_core::proto::gait::log::v1::PermissionItem;
use gait_proto::gait::sandbox::v1::{Append, Signal, StreamForward, Terminated, signal};
use prost::Message;
use std::path::Path;
use std::time::Duration;

fn frame(sig: &Signal) -> Vec<u8> {
    let b = sig.encode_to_vec();
    let mut f = vec![1u8];
    f.extend_from_slice(&(b.len() as u32).to_be_bytes());
    f.extend_from_slice(&b);
    f
}

fn set_brain_exe() {
    unsafe { std::env::set_var("GAIT_BRAIN_EXE", env!("CARGO_BIN_EXE_mock_brain_acp")) };
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

fn write_frames(gait_dir: &Path, step: u32, sigs: &[Signal]) {
    let mut blob = Vec::new();
    for s in sigs {
        blob.extend_from_slice(&frame(s));
    }
    std::fs::write(gait_dir.join(format!("mock_frames.{step}.bin")), blob).unwrap();
}

fn perm_item(id: &str, cmd: &str, patterns: &[&str]) -> PermissionItem {
    PermissionItem {
        id: id.to_string(),
        cmd: cmd.to_string(),
        reason: "shell requires permission".to_string(),
        patterns: patterns.iter().map(|s| s.to_string()).collect(),
        tool_name: "shell".to_string(),
        tool_args: None,
        preview: None,
    }
}

fn workspace() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    set_brain_exe();
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
    std::fs::create_dir_all(gait_dir.join("inferences/mock")).unwrap();
    std::fs::write(
        gait_dir.join("inferences/mock/INFERENCE.yaml"),
        "model: mock/model\n",
    )
    .unwrap();
    std::fs::write(gait_dir.join("gait.jsonc"), r#"{ "inference": "mock" }"#).unwrap();
    (t, gait_dir, repo)
}

#[tokio::test]
async fn cancel_while_awaiting_permission_overrides_allow() {
    let (_t, gait_dir, repo) = workspace();
    // brain#0 -> AwaitingPermission (one item), then terminate Permissions.
    write_frames(
        &gait_dir,
        0,
        &[
            append_event(gait_core::proto::events::permissions(&[perm_item(
                "tc1",
                "ls",
                &["ls"],
            )])),
            terminated(r#"{"permissions":{}}"#),
        ],
    );
    // No step-1 frames: if the token FAILED and the allow were processed, brain#1
    // would run with no frames and the turn would end EndTurn (or hang) -> the
    // Cancelled assertion would fail. A working token abandons before the allow.

    let map = SessionMap::new();
    let (channel, server) = gait_acp::build_agent(map).into_channel_and_future();
    tokio::spawn(server);

    let (stop, sid) = Client
        .builder()
        .on_receive_request(
            move |req: RequestPermissionRequest,
                  responder: Responder<RequestPermissionResponse>,
                  cx: agent_client_protocol::ConnectionTo<agent_client_protocol::Agent>| {
                async move {
                    // Cancel first (fire-and-forget), then answer Allow-once. The
                    // brief sleep guarantees the adapter processes the notification
                    // (trips the token) before the allow reaches the source's select!.
                    // send_notification returns a #[must_use] Result; discard it.
                    let _ = cx.send_notification(CancelNotification::new(req.session_id.clone()));
                    tokio::time::sleep(Duration::from_millis(150)).await;
                    let once = req.options.iter()
                        .find(|o| o.kind == PermissionOptionKind::AllowOnce).unwrap();
                    responder.respond(RequestPermissionResponse::new(
                        RequestPermissionOutcome::Selected(
                            SelectedPermissionOutcome::new(once.option_id.clone()),
                        ),
                    ))
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(channel, async |cx| {
            let sid = cx.send_request(NewSessionRequest::new(repo.clone()))
                .block_task().await?.session_id;
            let sid_str = sid.0.to_string();
            let resp = cx.send_request(PromptRequest::new(sid, vec!["run it".to_string().into()]))
                .block_task().await?;
            Ok((resp.stop_reason, sid_str))
        })
        .await
        .expect("connection stays alive");

    // `stop` is a bare StopReason here: this test's own connect_with uses `?` and
    // returns Ok(resp.stop_reason). After Task 2 the cancel resolves as a normal
    // PromptResponse(Cancelled), so `?` succeeds (no panic at `.expect`).
    assert_eq!(
        stop,
        StopReason::Cancelled,
        "cancel overrides the subsequent allow"
    );
    // The on-disk log is pbjson-serialized: the PermissionsReply oneof field is the
    // lowerCamelCase proto-JSON name "permissionsReply" (matches permission.rs's
    // own assertion — NOT the snake_case/PascalCase spellings).
    let log = std::fs::read_to_string(gait_core::session::live::log_path(&gait_dir, &sid))
        .unwrap_or_default();
    assert!(
        !log.contains("permissionsReply"),
        "cancel must not write a PermissionsReply; got: {log:?}"
    );
}

#[tokio::test]
async fn cancel_running_brain_resolves_cancelled() {
    let (_t, gait_dir, repo) = workspace();
    // brain#0 sends one forwarded event, then hangs (no Terminated). The cascade
    // kill tears it down; completion sees was_cancelled -> Cancelled, regardless
    // of how the severed subprocess finalized.
    write_frames(
        &gait_dir,
        0,
        &[forward_event(gait_core::proto::events::assistant_text(
            "working".into(),
        ))],
    );
    std::fs::write(gait_dir.join("mock_hang_ms"), "10000").unwrap();

    let map = SessionMap::new();
    let (channel, server) = gait_acp::build_agent(map).into_channel_and_future();
    tokio::spawn(server);

    let stop = Client
        .builder()
        .connect_with(channel, async |cx| {
            let sid = cx
                .send_request(NewSessionRequest::new(repo.clone()))
                .block_task()
                .await?
                .session_id;
            // Cancel ~300ms in: the brain is spawned and enrolled in the cascade
            // group by then (kill() lands), but nowhere near the 10s hang deadline.
            // `tokio::spawn` (not spawn_local): connect_with does not establish a
            // LocalSet, and the send future is Send.
            let cx2 = cx.clone();
            let sid2 = sid.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(300)).await;
                let _ = cx2.send_notification(CancelNotification::new(sid2));
            });
            let resp = cx
                .send_request(PromptRequest::new(sid, vec!["run it".to_string().into()]))
                .block_task()
                .await?;
            Ok(resp.stop_reason)
        })
        .await
        .expect("connection stays alive");

    assert_eq!(stop, StopReason::Cancelled);
}

#[tokio::test]
async fn cancel_unknown_session_is_noop() {
    let (_t, _gait_dir, repo) = workspace();
    let map = SessionMap::new();
    let (channel, server) = gait_acp::build_agent(map).into_channel_and_future();
    tokio::spawn(server);

    // A real NewSession, a cancel for a GHOST id, then another NewSession: two
    // successful round-trips bracketing the ghost cancel prove it was a harmless
    // no-op that neither panicked nor tore down the connection.
    Client
        .builder()
        .connect_with(channel, async |cx| {
            let _ = cx
                .send_request(NewSessionRequest::new(repo.clone()))
                .block_task()
                .await?;
            let _ = cx.send_notification(CancelNotification::new(SessionId::new(
                "ghost-never-minted",
            )));
            let ok = cx
                .send_request(NewSessionRequest::new(repo.clone()))
                .block_task()
                .await;
            assert!(
                ok.is_ok(),
                "server survives a cancel for an unknown session"
            );
            Ok(())
        })
        .await
        .expect("connection stays alive");
}

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
    Signal {
        corr_id: 0,
        body: Some(signal::Body::Append(Append {
            event: Some(gait_core::proto::envelope::mint(
                body,
                None,
                gait_core::proto::envelope::EventScope::Step {
                    run: 0,
                    turn: 0,
                    step: 0,
                },
            )),
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
    let outcome = if run_result_json.contains("permissions") {
        gait_core::proto::gait::sandbox::v1::FinalizedTurnOutcome::Permissions
    } else {
        gait_core::proto::gait::sandbox::v1::FinalizedTurnOutcome::TextDone
    };
    Signal {
        corr_id: 0,
        body: Some(signal::Body::Terminated(Terminated {
            turn: Some(gait_core::proto::gait::sandbox::v1::FinalizedTurn {
                outcome: outcome as i32,
                message: "ok".into(),
                ..Default::default()
            }),
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
    unsafe { std::env::set_var("GAIT_HOME", &gait_dir) };
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
        gait_dir.join("inferences/mock/inference.textproto"),
        "model: \"mock/model\"\ncompaction_threshold_tokens: 128000\n",
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

fn meta(v: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
    v.as_object().unwrap().clone()
}

/// The session's CancellationSignals and the retrace text of its last message.
fn cancel_record(gait_dir: &Path, sid: &str) -> (Vec<gait_core::proto::gait::log::v1::CancellationSignal>, String) {
    use gait_core::proto::gait::log::v1::payload::Kind;
    let events = gait_core::session::log::read_events(&gait_core::session::live::log_path(gait_dir, sid))
        .expect("read log");
    let signals = events
        .iter()
        .filter_map(|e| match gait_core::proto::event_kind(e) {
            Some(Kind::CancellationSignal(s)) => Some(s.clone()),
            _ => None,
        })
        .collect();
    let text = match gait_state::session::retrace::transcript(&events).expect("retrace").last() {
        Some(gait_core::types::Message::User {
            content: gait_core::types::MessageContent::Text(t),
            ..
        }) => t.clone(),
        other => panic!("expected a trailing user message, got {other:?}"),
    };
    (signals, text)
}

/// Runs a prompt against a hanging brain and cancels it with `cancel_meta`.
async fn cancel_hanging_turn(cancel_meta: Option<serde_json::Map<String, serde_json::Value>>) -> (std::path::PathBuf, String, tempfile::TempDir) {
    let (t, gait_dir, repo) = workspace();
    write_frames(
        &gait_dir,
        0,
        &[forward_event(gait_core::proto::events::assistant_text("working".into()))],
    );
    std::fs::write(gait_dir.join("mock_hang_ms"), "10000").unwrap();
    let (channel, server) = gait_acp::build_agent(SessionMap::new()).into_channel_and_future();
    tokio::spawn(server);
    let (stop, sid) = Client
        .builder()
        .connect_with(channel, async |cx| {
            let sid = cx.send_request(NewSessionRequest::new(repo.clone())).block_task().await?.session_id;
            let (cx2, sid2) = (cx.clone(), sid.clone());
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(300)).await;
                let mut n = CancelNotification::new(sid2);
                n.meta = cancel_meta;
                let _ = cx2.send_notification(n);
            });
            let resp = cx
                .send_request(PromptRequest::new(sid.clone(), vec!["run it".to_string().into()]))
                .block_task()
                .await?;
            Ok((resp.stop_reason, sid.0.to_string()))
        })
        .await
        .expect("connection stays alive");
    assert_eq!(stop, StopReason::Cancelled);
    (gait_dir, sid, t)
}

#[tokio::test]
async fn initialize_advertises_session_notices() {
    use agent_client_protocol::schema::ProtocolVersion;
    use agent_client_protocol::schema::v1::InitializeRequest;
    let (channel, server) = gait_acp::build_agent(SessionMap::new()).into_channel_and_future();
    tokio::spawn(server);
    let resp = Client
        .builder()
        .connect_with(channel, async |cx| {
            Ok(cx
                .send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?)
        })
        .await
        .expect("initialize");
    assert_eq!(
        resp.meta.as_ref().and_then(|m| m.get("qtpi.app/sessionNotices")),
        Some(&serde_json::Value::Bool(true))
    );
}

#[tokio::test]
async fn a_cancel_with_actor_meta_records_who_and_why() {
    let m = meta(serde_json::json!({"qtpi.app/actor":
        {"kind": "orchestrator", "id": "svc", "reason": "Action 7: x"}}));
    let (gait_dir, sid, _t) = cancel_hanging_turn(Some(m)).await;
    let (signals, text) = cancel_record(&gait_dir, &sid);
    assert_eq!(signals.len(), 1, "exactly one CancellationSignal");
    let s = &signals[0];
    assert_eq!(s.actor_kind, gait_core::proto::gait::log::v1::CancelActorKind::Orchestrator as i32);
    assert_eq!(s.actor_id, "svc");
    assert_eq!(s.reason.as_deref(), Some("Action 7: x"));
    assert_eq!(text, "[turn cancelled by orchestrator: Action 7: x]");
}

#[tokio::test]
async fn a_cancel_without_meta_records_no_cause() {
    let (gait_dir, sid, _t) = cancel_hanging_turn(None).await;
    let (signals, text) = cancel_record(&gait_dir, &sid);
    assert_eq!(signals.len(), 1, "exactly one CancellationSignal");
    assert_eq!(signals[0].actor_kind, gait_core::proto::gait::log::v1::CancelActorKind::Unspecified as i32);
    assert_eq!(text, "[turn interrupted by user]");
}

#[tokio::test]
async fn a_permission_cancelled_by_policy_records_the_policy_cause() {
    let (_t, gait_dir, repo) = workspace();
    write_frames(
        &gait_dir,
        0,
        &[
            append_event(gait_core::proto::events::permissions(&[perm_item("tc1", "ls", &["ls"])])),
            terminated(r#"{"permissions":{}}"#),
        ],
    );
    let (channel, server) = gait_acp::build_agent(SessionMap::new()).into_channel_and_future();
    tokio::spawn(server);
    let (stop, sid) = Client
        .builder()
        .on_receive_request(
            move |_req: RequestPermissionRequest,
                  responder: Responder<RequestPermissionResponse>,
                  _cx: agent_client_protocol::ConnectionTo<agent_client_protocol::Agent>| async move {
                let mut resp = RequestPermissionResponse::new(RequestPermissionOutcome::Cancelled);
                resp.meta = Some(meta(serde_json::json!({
                    "qtpi.app/actor": {"kind": "policy", "id": "lib"},
                    "qtpi.app/notice": {"kind": "permission_denied_by_policy", "tool": "Bash"},
                })));
                responder.respond(resp)
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(channel, async |cx| {
            let sid = cx.send_request(NewSessionRequest::new(repo.clone())).block_task().await?.session_id;
            let resp = cx
                .send_request(PromptRequest::new(sid.clone(), vec!["run it".to_string().into()]))
                .block_task()
                .await?;
            Ok((resp.stop_reason, sid.0.to_string()))
        })
        .await
        .expect("connection stays alive");
    assert_eq!(stop, StopReason::Cancelled);
    let (signals, text) = cancel_record(&gait_dir, &sid);
    assert_eq!(signals.len(), 1);
    assert_eq!(signals[0].reason.as_deref(), Some("a rule refused permission for `Bash`"));
    assert_eq!(text, "[turn cancelled by policy: a rule refused permission for `Bash`]");
}

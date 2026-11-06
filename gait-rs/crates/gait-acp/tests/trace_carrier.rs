//! Traceability: the carrier in ACP `_meta` parents the cancel span, and links
//! (never parents) the answers to gait's own requests.
#![allow(dead_code)]
use agent_client_protocol::schema::v1::{
    CancelNotification, NewSessionRequest, PromptRequest, RequestPermissionOutcome,
    RequestPermissionRequest, RequestPermissionResponse, StopReason,
};
use agent_client_protocol::{Client, ConnectTo, Responder};
use concord_telemetry::{TraceCarrier, carrier, testing};
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


fn meta_of(c: &TraceCarrier) -> serde_json::Map<String, serde_json::Value> {
    c.to_pairs()
        .into_iter()
        .map(|(k, v)| (k.to_owned(), serde_json::Value::String(v)))
        .collect()
}

fn trace_id_of(c: &TraceCarrier) -> String {
    c.traceparent.as_ref().expect("traceparent")[3..35].to_owned()
}

fn span_id_of(c: &TraceCarrier) -> String {
    c.traceparent.as_ref().expect("traceparent")[36..52].to_owned()
}

macro_rules! finished {
    ($name:expr) => {
        testing::spans()
            .into_iter()
            .rev()
            .find(|span| span.name == $name)
            .unwrap_or_else(|| panic!("no finished span named {}", $name))
    };
}

#[tokio::test]
async fn cancel_meta_parents_acp_cancel_and_links_from_acp_prompt() {
    let _captured = testing::capture_exclusive();
    let origin = tracing::info_span!("test-origin");
    let sent = origin.in_scope(|| {
        concord_telemetry::context::add("action_id", "A9");
        carrier::capture()
    });
    let (_t, gait_dir, repo) = workspace();
    write_frames(
        &gait_dir,
        0,
        &[forward_event(gait_core::proto::events::assistant_text("working".into()))],
    );
    std::fs::write(gait_dir.join("mock_hang_ms"), "10000").unwrap();
    let (channel, server) = gait_acp::build_agent(SessionMap::new()).into_channel_and_future();
    tokio::spawn(server);
    let cancel_meta = meta_of(&sent);
    let stop = Client
        .builder()
        .connect_with(channel, async |cx| {
            let sid = cx.send_request(NewSessionRequest::new(repo.clone())).block_task().await?.session_id;
            let (cx2, sid2) = (cx.clone(), sid.clone());
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(300)).await;
                let mut n = CancelNotification::new(sid2);
                n.meta = Some(cancel_meta);
                let _ = cx2.send_notification(n);
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
    drop(origin);

    let cancel = finished!("acp.cancel");
    assert_eq!(cancel.span_context.trace_id().to_string(), trace_id_of(&sent));
    assert_eq!(cancel.parent_span_id.to_string(), span_id_of(&sent));

    let prompt = finished!("acp.prompt");
    assert!(
        prompt.events.iter().any(|e| e.name == "cancel_requested"),
        "the running prompt notes the cancel"
    );
    assert!(
        prompt.links.links.iter().any(|l| l.span_context.span_id() == cancel.span_context.span_id()),
        "the prompt links to the cancel that stopped it"
    );
}

#[tokio::test]
async fn permission_answer_links_not_parents() {
    let _captured = testing::capture_exclusive();
    let origin = tracing::info_span!("test-answerer");
    let sent = origin.in_scope(carrier::capture);
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
    let answer_meta = meta_of(&sent);
    Client
        .builder()
        .on_receive_request(
            move |_req: RequestPermissionRequest,
                  responder: Responder<RequestPermissionResponse>,
                  _cx: agent_client_protocol::ConnectionTo<agent_client_protocol::Agent>| {
                let answer_meta = answer_meta.clone();
                async move {
                    let mut resp = RequestPermissionResponse::new(RequestPermissionOutcome::Cancelled);
                    resp.meta = Some(answer_meta);
                    responder.respond(resp)
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(channel, async |cx| {
            let sid = cx.send_request(NewSessionRequest::new(repo.clone())).block_task().await?.session_id;
            cx.send_request(PromptRequest::new(sid, vec!["run it".to_string().into()]))
                .block_task()
                .await?;
            Ok(())
        })
        .await
        .expect("connection stays alive");
    drop(origin);

    let answer = finished!("acp.answer");
    assert_ne!(answer.span_context.trace_id().to_string(), trace_id_of(&sent), "an answer is never parented on the answerer");
    assert_eq!(answer.links.links.len(), 1);
    assert_eq!(answer.links.links[0].span_context.trace_id().to_string(), trace_id_of(&sent));
}

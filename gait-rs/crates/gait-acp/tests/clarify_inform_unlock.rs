//! Phase 3 integration tests: the permission loop (session/request_permission).
//! brain#1 -> AwaitingPermission; a mock client answers; the adapter writes a
//! PermissionsReply and resumes; brain#2 completes.
use agent_client_protocol::schema::v1::{
    NewSessionRequest, PermissionOption, PromptRequest, RequestPermissionOutcome,
    RequestPermissionRequest, RequestPermissionResponse, SelectedPermissionOutcome, StopReason,
};
use agent_client_protocol::{Client, ConnectTo, Responder};
use gait_acp::session_map::SessionMap;
use gait_proto::gait::sandbox::v1::{Append, Signal, StreamForward, Terminated, signal};
use prost::Message;
use std::path::Path;
use std::sync::{Arc, Mutex};

fn frame(sig: &Signal) -> Vec<u8> {
    let b = sig.encode_to_vec();
    let mut f = vec![1u8];
    f.extend_from_slice(&(b.len() as u32).to_be_bytes());
    f.extend_from_slice(&b);
    f
}

fn set_brain_exe() {
    // Safe to set repeatedly; the value is identical for the whole test binary.
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

/// A settled event, wrapped as a StreamForward frame (client-facing render).
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

/// An event appended to the on-disk session log (persisted; drives session_state).
fn append_event(body: gait_core::proto::gait::log::v1::event::Body) -> Signal {
    append_event_for_turn(body, 0)
}

fn append_event_for_turn(body: gait_core::proto::gait::log::v1::event::Body, turn: u32) -> Signal {
    Signal {
        corr_id: 0,
        body: Some(signal::Body::Append(Append {
            event: Some(gait_core::proto::envelope::mint(
                body,
                None,
                gait_core::proto::envelope::EventScope::Step {
                    run: 0,
                    turn,
                    step: 0,
                },
            )),
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

/// Set up a gait-init'd workspace with a mock brain; return (gait_dir, repo).
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
        gait_dir.join("inferences/mock/INFERENCE.yaml"),
        "model: mock/model\ncompaction_threshold_tokens: 128000\n",
    )
    .unwrap();
    std::fs::write(gait_dir.join("gait.jsonc"), r#"{ "inference": "mock" }"#).unwrap();
    (t, gait_dir, repo)
}

/// A client that records each request_permission and answers via `pick`
/// (given the offered options, return the chosen response).
///
/// Returns `(Result<StopReason, String>, String)` where the outer `String` is
/// the minted `sid` (so the caller can build the live log path for on-disk
/// assertions, per plan Step-4 note) and the `Result` is the prompt outcome.
///
/// The `Result` — not a bare `StopReason` — matters here: per the Task-2
/// design note, an unanswered `Permissions` turn (client cancels, or echoes
/// an unknown `option_id`) surfaces as a genuine JSON-RPC error on the
/// *prompt*, not a clean `StopReason`. That error must NOT be propagated with
/// `?` out of this closure — doing so would fail the whole `connect_with`
/// future (a connection-level failure) and panic every caller's
/// `.expect("connection stays alive")`, even though the connection itself is
/// fine. So the prompt error is caught here and handed back as `Err(String)`
/// instead, leaving `.expect("connection stays alive")` to guard only actual
/// connection failures (e.g. `new_session` itself failing).
async fn run_prompt_with_client(
    repo: std::path::PathBuf,
    seen: Arc<Mutex<Vec<Vec<PermissionOption>>>>,
    pick: impl Fn(&[PermissionOption]) -> RequestPermissionResponse + Send + Sync + 'static,
) -> (Result<StopReason, String>, String) {
    let map = SessionMap::new();
    let (channel, server) = gait_acp::build_agent(map).into_channel_and_future();
    tokio::spawn(server);
    let pick = Arc::new(pick);
    Client
        .builder()
        .on_receive_request(
            {
                let seen = seen.clone();
                let pick = pick.clone();
                move |req: RequestPermissionRequest,
                      responder: Responder<RequestPermissionResponse>,
                      _cx| {
                    let seen = seen.clone();
                    let pick = pick.clone();
                    async move {
                        seen.lock().unwrap().push(req.options.clone());
                        responder.respond(pick(&req.options))
                    }
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(channel, async |cx| {
            let sid = cx
                .send_request(NewSessionRequest::new(repo.clone()))
                .block_task()
                .await?
                .session_id;
            let sid_str = sid.0.to_string();
            let result = match cx
                .send_request(PromptRequest::new(sid, vec!["run it".to_string().into()]))
                .block_task()
                .await
            {
                Ok(resp) => Ok(resp.stop_reason),
                Err(e) => Err(e.to_string()),
            };
            Ok((result, sid_str))
        })
        .await
        .expect("connection stays alive")
}

fn terminated_for(kind: &str) -> Signal {
    let outcome = match kind {
        "clarify" => gait_proto::gait::sandbox::v1::FinalizedTurnOutcome::Clarify,
        "inform" => gait_proto::gait::sandbox::v1::FinalizedTurnOutcome::Inform,
        "unlock" => gait_proto::gait::sandbox::v1::FinalizedTurnOutcome::SandboxUnlock,
        _ => gait_proto::gait::sandbox::v1::FinalizedTurnOutcome::TextDone,
    };
    Signal {
        corr_id: 0,
        body: Some(signal::Body::Terminated(Terminated {
            turn: Some(gait_proto::gait::sandbox::v1::FinalizedTurn {
                outcome: outcome as i32,
                message: "ok".into(),
                ..Default::default()
            }),
            failed: false,
        })),
    }
}

fn clarify_item(id: &str, content: &str) -> gait_core::proto::gait::log::v1::ClarifyItem {
    gait_core::proto::gait::log::v1::ClarifyItem {
        id: id.into(),
        content: content.into(),
        why: "needed".into(),
        kind: gait_core::proto::gait::log::v1::ClarifyKind::Is as i32,
        options: vec![],
    }
}

fn inform_item(id: &str, content: &str) -> gait_core::proto::gait::log::v1::InformItem {
    gait_core::proto::gait::log::v1::InformItem {
        id: id.into(),
        content: content.into(),
        context: None,
    }
}

fn unlock_item(id: &str) -> gait_core::proto::gait::log::v1::SandboxUnlockItem {
    gait_core::proto::gait::log::v1::SandboxUnlockItem {
        id: id.into(),
        tool_name: "shell".into(),
        resource: "/tmp".into(),
        dimension: "write".into(),
        preview: None,
    }
}

async fn run_elicitation_prompt(
    repo: std::path::PathBuf,
    pick: impl Fn(
        &agent_client_protocol::schema::v1::CreateElicitationRequest,
    ) -> agent_client_protocol::schema::v1::CreateElicitationResponse
    + Send
    + Sync
    + 'static,
) -> (Result<StopReason, String>, String) {
    use agent_client_protocol::schema::v1::{
        CreateElicitationRequest, CreateElicitationResponse, NewSessionRequest, PromptRequest,
    };
    let map = SessionMap::new();
    let (channel, server) = gait_acp::build_agent(map).into_channel_and_future();
    tokio::spawn(server);
    let pick = Arc::new(pick);
    Client
        .builder()
        .on_receive_request(
            move |req: CreateElicitationRequest,
                  responder: Responder<CreateElicitationResponse>,
                  _cx| {
                let pick = pick.clone();
                async move { responder.respond(pick(&req)) }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(channel, async |cx| {
            let sid = cx
                .send_request(NewSessionRequest::new(repo.clone()))
                .block_task()
                .await?
                .session_id;
            let sid_str = sid.0.to_string();
            let result = match cx
                .send_request(PromptRequest::new(sid, vec!["run it".to_string().into()]))
                .block_task()
                .await
            {
                Ok(resp) => Ok(resp.stop_reason),
                Err(e) => Err(e.to_string()),
            };
            Ok((result, sid_str))
        })
        .await
        .expect("connection stays alive")
}

fn accepted_answer(answer: &str) -> agent_client_protocol::schema::v1::CreateElicitationResponse {
    use agent_client_protocol::schema::v1::{
        CreateElicitationResponse, ElicitationAcceptAction, ElicitationAction,
        ElicitationContentValue,
    };
    let mut content = std::collections::BTreeMap::new();
    content.insert(
        "answer".into(),
        ElicitationContentValue::String(answer.into()),
    );
    CreateElicitationResponse::new(ElicitationAction::Accept(
        ElicitationAcceptAction::new().content(Some(content)),
    ))
}

async fn run_permission_prompt(
    repo: std::path::PathBuf,
) -> (
    Result<StopReason, String>,
    String,
    Arc<Mutex<Vec<Vec<PermissionOption>>>>,
) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let result = run_prompt_with_client(repo, seen.clone(), |opts| {
        let option = opts
            .iter()
            .find(|o| o.option_id.0.as_ref() == "session")
            .unwrap();
        RequestPermissionResponse::new(RequestPermissionOutcome::Selected(
            SelectedPermissionOutcome::new(option.option_id.clone()),
        ))
    })
    .await;
    (result.0, result.1, seen)
}

#[tokio::test]
async fn clarify_resumes_and_completes() {
    let (_t, gait_dir, repo) = workspace();
    write_frames(
        &gait_dir,
        0,
        &[
            append_event(gait_core::proto::events::clarify(&[clarify_item(
                "c1",
                "which db?",
            )])),
            terminated_for("clarify"),
        ],
    );
    write_frames(
        &gait_dir,
        1,
        &[
            append_event_for_turn(gait_core::proto::events::assistant_text("done".into()), 1),
            forward_event(gait_core::proto::events::assistant_text("done".into())),
            terminated_for("done"),
        ],
    );
    let (result, sid) = run_elicitation_prompt(repo, |_| accepted_answer("postgres")).await;
    assert_eq!(result, Ok(StopReason::EndTurn));
    let log = std::fs::read_to_string(gait_core::session::live::log_path(&gait_dir, &sid)).unwrap();
    assert!(
        log.contains("clarifyReply") && log.contains("postgres"),
        "{log}"
    );
}

#[tokio::test]
async fn inform_resumes_and_completes() {
    let (_t, gait_dir, repo) = workspace();
    write_frames(
        &gait_dir,
        0,
        &[
            append_event(gait_core::proto::events::inform(&[inform_item(
                "i1",
                "deleted branches",
            )])),
            terminated_for("inform"),
        ],
    );
    write_frames(
        &gait_dir,
        1,
        &[
            append_event_for_turn(gait_core::proto::events::assistant_text("done".into()), 1),
            forward_event(gait_core::proto::events::assistant_text("done".into())),
            terminated_for("done"),
        ],
    );
    let (result, sid) = run_elicitation_prompt(repo, |_| accepted_answer("postgres")).await;
    assert_eq!(result, Ok(StopReason::EndTurn));
    let log = std::fs::read_to_string(gait_core::session::live::log_path(&gait_dir, &sid)).unwrap();
    assert!(
        log.contains("informReply") && log.contains("postgres"),
        "{log}"
    );
}

#[tokio::test]
async fn sandbox_unlock_resumes_and_completes() {
    let (_t, gait_dir, repo) = workspace();
    write_frames(
        &gait_dir,
        0,
        &[
            append_event(gait_core::proto::events::sandbox_unlock(&[unlock_item(
                "u1",
            )])),
            terminated_for("unlock"),
        ],
    );
    write_frames(
        &gait_dir,
        1,
        &[
            append_event_for_turn(gait_core::proto::events::assistant_text("done".into()), 1),
            forward_event(gait_core::proto::events::assistant_text("done".into())),
            terminated_for("done"),
        ],
    );
    let (result, sid, seen) = run_permission_prompt(repo).await;
    assert_eq!(result, Ok(StopReason::EndTurn));
    assert_eq!(seen.lock().unwrap()[0].len(), 2);
    let log = std::fs::read_to_string(gait_core::session::live::log_path(&gait_dir, &sid)).unwrap();
    assert!(
        log.contains("sandboxUnlockReply") && log.contains("session"),
        "{log}"
    );
}

#[tokio::test]
async fn clarify_multi_item_batch_surfaces_as_error() {
    let (_t, gait_dir, repo) = workspace();
    write_frames(
        &gait_dir,
        0,
        &[
            append_event(gait_core::proto::events::clarify(&[
                clarify_item("c1", "one"),
                clarify_item("c2", "two"),
            ])),
            terminated_for("clarify"),
        ],
    );
    let (result, _sid) = run_elicitation_prompt(repo, |_| accepted_answer("postgres")).await;
    assert!(
        result.is_err(),
        "multi-item clarify must surface an error: {result:?}"
    );
}

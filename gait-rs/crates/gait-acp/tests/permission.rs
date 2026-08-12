//! Phase 3 integration tests: the permission loop (session/request_permission).
//! brain#1 -> AwaitingPermission; a mock client answers; the adapter writes a
//! PermissionsReply and resumes; brain#2 completes.
use agent_client_protocol::schema::v1::{
    NewSessionRequest, PermissionOption, PermissionOptionKind, PromptRequest,
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    SelectedPermissionOutcome, StopReason,
};
use agent_client_protocol::{Client, ConnectTo, Responder};
use gait_acp::session_map::SessionMap;
use gait_core::proto::gait::log::v1::PermissionItem;
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

/// Set up a gait-init'd workspace with a mock brain; return (gait_dir, repo).
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

#[tokio::test]
async fn permission_allow_once_resumes_and_completes() {
    let (_t, gait_dir, repo) = workspace();
    // Step 0: append a Permissions event (one item) then terminate Permissions.
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
    // Step 1 (resume): stream a reply, then terminate done.
    write_frames(
        &gait_dir,
        1,
        &[
            append_event_for_turn(gait_core::proto::events::assistant_text("done".into()), 1),
            forward_event(gait_core::proto::events::assistant_text("done".into())),
            terminated(r#"{"done":{"message":"ok"}}"#),
        ],
    );

    let seen = Arc::new(Mutex::new(Vec::new()));
    let (result, sid) = run_prompt_with_client(repo, seen.clone(), |opts| {
        // Pick the Allow-once option.
        let once = opts
            .iter()
            .find(|o| o.kind == PermissionOptionKind::AllowOnce)
            .unwrap();
        RequestPermissionResponse::new(RequestPermissionOutcome::Selected(
            SelectedPermissionOutcome::new(once.option_id.clone()),
        ))
    })
    .await;

    assert_eq!(result, Ok(StopReason::EndTurn), "resume completes the turn");
    let requests = seen.lock().unwrap();
    assert_eq!(requests.len(), 1, "exactly one request_permission");
    assert!(
        requests[0]
            .iter()
            .any(|o| o.kind == PermissionOptionKind::AllowOnce)
    );
    // The reply landed on disk (a PermissionsReply event exists). The log is
    // pbjson-serialized JSON; the oneof field name for tag 14 is the
    // lowerCamelCase proto-JSON name "permissionsReply" (verified against the
    // generated gait.log.v1.serde.rs), not the Rust snake_case/PascalCase
    // spellings.
    let log = std::fs::read_to_string(gait_core::session::live::log_path(&gait_dir, &sid)).unwrap();
    assert!(
        log.contains("permissionsReply"),
        "a PermissionsReply was appended; log: {log:?}"
    );
}

#[tokio::test]
async fn permission_cancelled_finalizes_without_reply() {
    let (_t, gait_dir, repo) = workspace();
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
    // No step-1 frames: if the loop wrongly resumed, brain#2 would fall back to
    // mock_frames.bin (absent) and emit nothing -> the test would hang/err. The
    // Cancelled outcome must instead finalize immediately.
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (result, sid) = run_prompt_with_client(repo, seen.clone(), |_opts| {
        RequestPermissionResponse::new(RequestPermissionOutcome::Cancelled)
    })
    .await;

    // Phase 4: a Cancelled outcome trips the shared token; the completion path
    // resolves StopReason::Cancelled (was: a JSON-RPC error). No reply is written
    // and no second brain runs (no step-1 frames were baked). The log's oneof field
    // for a reply is the pbjson name "permissionsReply".
    assert_eq!(result, Ok(StopReason::Cancelled));
    assert_eq!(seen.lock().unwrap().len(), 1, "one request, then finalize");
    let log = std::fs::read_to_string(gait_core::session::live::log_path(&gait_dir, &sid))
        .unwrap_or_default();
    assert!(
        !log.contains("permissionsReply"),
        "cancelled turn writes no reply; got: {log:?}"
    );
}

#[tokio::test]
async fn permission_two_distinct_calls_two_requests() {
    let (_t, gait_dir, repo) = workspace();
    // Two distinct tool_call_ids in one batch -> two request_permission calls.
    write_frames(
        &gait_dir,
        0,
        &[
            append_event(gait_core::proto::events::permissions(&[
                perm_item("tcA", "ls", &["ls"]),
                perm_item("tcB", "cat x", &["cat x"]),
            ])),
            terminated(r#"{"permissions":{}}"#),
        ],
    );
    write_frames(
        &gait_dir,
        1,
        &[
            append_event_for_turn(gait_core::proto::events::assistant_text("done".into()), 1),
            forward_event(gait_core::proto::events::assistant_text("done".into())),
            terminated(r#"{"done":{"message":"ok"}}"#),
        ],
    );
    let seen = Arc::new(Mutex::new(Vec::new()));
    // Allow the first, reject the second — honored because distinct ids resume
    // independently (differential works across distinct tool calls).
    let (result, _sid) = run_prompt_with_client(repo, seen.clone(), |opts| {
        let kind = if !opts.is_empty() {
            PermissionOptionKind::AllowOnce
        } else {
            PermissionOptionKind::RejectOnce
        };
        let opt = opts.iter().find(|o| o.kind == kind).unwrap();
        RequestPermissionResponse::new(RequestPermissionOutcome::Selected(
            SelectedPermissionOutcome::new(opt.option_id.clone()),
        ))
    })
    .await;
    assert_eq!(result, Ok(StopReason::EndTurn));
    assert_eq!(
        seen.lock().unwrap().len(),
        2,
        "one request per distinct tool_call_id"
    );
}

#[tokio::test]
async fn permission_unknown_option_errors_no_reply() {
    let (_t, gait_dir, repo) = workspace();
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
    // No step-1 frames: an unknown option must NOT cause a resume.
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (result, sid) = run_prompt_with_client(repo.clone(), seen, |_opts| {
        RequestPermissionResponse::new(RequestPermissionOutcome::Selected(
            SelectedPermissionOutcome::new(
                agent_client_protocol::schema::v1::PermissionOptionId::new("bogus"),
            ),
        ))
    })
    .await;
    // An option_id we never offered is a protocol violation -> next() bails,
    // the run loop finalizes on the still-`Permissions` turn, and the prompt
    // resolves as a JSON-RPC error (no second brain, no reply written).
    assert!(
        result.is_err(),
        "an unknown option_id surfaces as a JSON-RPC error, got {result:?}"
    );
    let log_path = gait_core::session::live::log_path(&gait_dir, &sid);
    let log = std::fs::read_to_string(&log_path).unwrap_or_default();
    assert!(
        !log.contains("permissionsReply"),
        "no PermissionsReply should be appended on unknown-option path; got: {log:?}"
    );
}

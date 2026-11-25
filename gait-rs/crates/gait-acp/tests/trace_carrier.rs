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
        &[forward_event(gait_core::proto::events::assistant_text(
            "working".into(),
        ))],
    );
    std::fs::write(gait_dir.join("mock_hang_ms"), "10000").unwrap();
    let (channel, server) = gait_acp::build_agent(SessionMap::new()).into_channel_and_future();
    tokio::spawn(server);
    let cancel_meta = meta_of(&sent);
    let stop = Client
        .builder()
        .connect_with(channel, async |cx| {
            let sid = cx
                .send_request(NewSessionRequest::new(repo.clone()))
                .block_task()
                .await?
                .session_id;
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
    assert_eq!(
        cancel.span_context.trace_id().to_string(),
        trace_id_of(&sent)
    );
    assert_eq!(cancel.parent_span_id.to_string(), span_id_of(&sent));

    let prompt = finished!("acp.prompt");
    assert!(
        prompt.events.iter().any(|e| e.name == "cancel_requested"),
        "the running prompt notes the cancel"
    );
    assert!(
        prompt
            .links
            .links
            .iter()
            .any(|l| l.span_context.span_id() == cancel.span_context.span_id()),
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
            append_event(gait_core::proto::events::permissions(&[perm_item(
                "tc1",
                "ls",
                &["ls"],
            )])),
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
    assert_ne!(
        answer.span_context.trace_id().to_string(),
        trace_id_of(&sent),
        "an answer is never parented on the answerer"
    );
    assert_eq!(answer.links.links.len(), 1);
    assert_eq!(
        answer.links.links[0].span_context.trace_id().to_string(),
        trace_id_of(&sent)
    );

    // The request gait sent to the client has a span of its own, under the prompt.
    let request = finished!("acp.request_permission");
    let prompt = finished!("acp.prompt");
    assert_eq!(
        request.parent_span_id,
        prompt.span_context.span_id(),
        "the permission request sits under the prompt"
    );
    assert!(
        request
            .attributes
            .iter()
            .any(|kv| kv.key.as_str() == "acp_session_id")
    );
}

macro_rules! attr {
    ($span:expr, $key:expr) => {
        $span
            .attributes
            .iter()
            .filter(|kv| kv.key.as_str() == $key)
            .map(|kv| kv.value.to_string())
            .collect::<Vec<String>>()
    };
}

/// The telemetry gate: across a prompt that is cancelled mid-turn, no span
/// repeats an attribute key, a baggage label is never stamped twice with
/// different values (a cancel's own labels must not leak onto the prompt it
/// stops), and gait's own session id never sits under the `session_id` label.
#[tokio::test]
async fn no_span_repeats_a_key_or_conflicts_with_a_label() {
    let _captured = testing::capture_exclusive();
    let origin = tracing::info_span!("test-origin");
    let labelled = |request: &str| {
        origin.in_scope(|| {
            concord_telemetry::context::add("session_id", "grorg-s1");
            concord_telemetry::context::add("request_id", request.to_owned());
            concord_telemetry::context::add("action_id", "A9");
            carrier::capture()
        })
    };
    let prompt_carrier = labelled("mm-1");
    let cancel_carrier = {
        let other = tracing::info_span!("test-cancel-origin");
        other.in_scope(|| {
            concord_telemetry::context::add("session_id", "grorg-s1");
            concord_telemetry::context::add("request_id", "mm-2");
            carrier::capture()
        })
    };
    let (_t, gait_dir, repo) = workspace();
    write_frames(
        &gait_dir,
        0,
        &[forward_event(gait_core::proto::events::assistant_text(
            "working".into(),
        ))],
    );
    std::fs::write(gait_dir.join("mock_hang_ms"), "10000").unwrap();
    let (channel, server) = gait_acp::build_agent(SessionMap::new()).into_channel_and_future();
    tokio::spawn(server);
    let (prompt_meta, cancel_meta) = (meta_of(&prompt_carrier), meta_of(&cancel_carrier));
    Client
        .builder()
        .connect_with(channel, async |cx| {
            let sid = cx
                .send_request(NewSessionRequest::new(repo.clone()))
                .block_task()
                .await?
                .session_id;
            let (cx2, sid2) = (cx.clone(), sid.clone());
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(300)).await;
                let mut n = CancelNotification::new(sid2);
                n.meta = Some(cancel_meta);
                let _ = cx2.send_notification(n);
            });
            let mut prompt = PromptRequest::new(sid, vec!["run it".to_string().into()]);
            prompt.meta = Some(prompt_meta);
            cx.send_request(prompt).block_task().await?;
            Ok(())
        })
        .await
        .expect("connection stays alive");
    drop(origin);

    let spans = testing::spans();
    for span in &spans {
        let mut keys: Vec<&str> = span.attributes.iter().map(|kv| kv.key.as_str()).collect();
        keys.sort_unstable();
        let repeated: Vec<&str> = keys
            .windows(2)
            .filter(|w| w[0] == w[1])
            .map(|w| w[0])
            .collect();
        // The same label stamped twice with one value is the library's to
        // collapse; a key with two different values is a conflict.
        for key in repeated {
            let mut values = attr!(span, key);
            values.sort_unstable();
            values.dedup();
            assert_eq!(
                values.len(),
                1,
                "span {} holds {key} with different values: {values:?}",
                span.name
            );
        }
    }
    let prompt = finished!("acp.prompt");
    assert_eq!(
        attr!(prompt, "request_id"),
        vec!["mm-1".to_owned()],
        "the cancel's request_id leaked onto the prompt"
    );
    assert_eq!(attr!(prompt, "session_id"), vec!["grorg-s1".to_owned()]);
    assert_eq!(attr!(prompt, "acp_session_id").len(), 1);
    for span in spans
        .iter()
        .filter(|s| s.name.starts_with("acp.") || s.name.starts_with("gait."))
    {
        for value in attr!(span, "session_id") {
            assert_eq!(
                value, "grorg-s1",
                "span {} puts a gait session id under the session_id label",
                span.name
            );
        }
    }
}

/// Per-turn causes must reach each turn's brain process: the baggage captured
/// under prompt 1 lands in the brain env of turn 1, prompt 2's in turn 2, and
/// neither prompt's labels go stale into the other turn.
#[tokio::test]
async fn per_turn_causes_reach_each_turns_brain_env_and_never_go_stale() {
    let _captured = testing::capture_exclusive();
    let origin = tracing::info_span!("test-env-origin");
    let carrier1 = origin.in_scope(|| {
        concord_telemetry::context::add("action_id", "A1");
        concord_telemetry::context::add("turn_id", "T1");
        carrier::capture()
    });
    let carrier2 = origin.in_scope(|| {
        concord_telemetry::context::add("action_id", "A2");
        concord_telemetry::context::add("turn_id", "T2");
        carrier::capture()
    });
    let (_t, gait_dir, repo) = workspace();
    write_frames(
        &gait_dir,
        0,
        &[
            append_event(gait_core::proto::events::assistant_text("working".into())),
            forward_event(gait_core::proto::events::assistant_text("working".into())),
            terminated(r#"{"text":"ok"}"#),
        ],
    );
    write_frames(
        &gait_dir,
        1,
        &[
            append_event(gait_core::proto::events::assistant_text(
                "working more".into(),
            )),
            forward_event(gait_core::proto::events::assistant_text(
                "working more".into(),
            )),
            terminated(r#"{"text":"ok"}"#),
        ],
    );
    let (channel, server) = gait_acp::build_agent(SessionMap::new()).into_channel_and_future();
    tokio::spawn(server);
    let (meta1, meta2) = (meta_of(&carrier1), meta_of(&carrier2));
    Client
        .builder()
        .connect_with(channel, async |cx| {
            let sid = cx
                .send_request(NewSessionRequest::new(repo.clone()))
                .block_task()
                .await?
                .session_id;
            let mut req1 = PromptRequest::new(sid.clone(), vec!["go".to_string().into()]);
            req1.meta = Some(meta1);
            let resp1 = cx.send_request(req1).block_task().await?;
            assert_eq!(resp1.stop_reason, StopReason::EndTurn);
            let mut req2 = PromptRequest::new(sid, vec!["go again".to_string().into()]);
            req2.meta = Some(meta2);
            let resp2 = cx.send_request(req2).block_task().await?;
            assert_eq!(resp2.stop_reason, StopReason::EndTurn);
            Ok(())
        })
        .await
        .expect("connection stays alive");
    drop(origin);

    let env1 =
        std::fs::read_to_string(gait_dir.join("mock_env.0.txt")).expect("turn 1 brain env file");
    let env2 =
        std::fs::read_to_string(gait_dir.join("mock_env.1.txt")).expect("turn 2 brain env file");
    for (env, name) in [(&env1, "turn 1"), (&env2, "turn 2")] {
        let traceparent = env
            .lines()
            .find(|l| l.starts_with("TRACEPARENT="))
            .expect("traceparent line");
        assert!(
            traceparent.len() > "TRACEPARENT=".len(),
            "{name} TRACEPARENT is empty"
        );
    }
    let baggage1 = env1
        .lines()
        .find(|l| l.starts_with("BAGGAGE="))
        .expect("turn 1 baggage line");
    assert!(
        baggage1.contains("action_id=A1") && baggage1.contains("turn_id=T1"),
        "turn 1 brain baggage lost prompt 1's labels: {baggage1}"
    );
    let baggage2 = env2
        .lines()
        .find(|l| l.starts_with("BAGGAGE="))
        .expect("turn 2 baggage line");
    assert!(
        baggage2.contains("action_id=A2") && baggage2.contains("turn_id=T2"),
        "turn 2 brain baggage lost prompt 2's labels: {baggage2}"
    );
    assert!(
        !baggage2.contains("A1") && !baggage2.contains("T1"),
        "prompt 1's labels went stale into turn 2's brain env: {baggage2}"
    );
}

/// A prompt whose carrier carries no baggage must not let the gait process's
/// own inherited BAGGAGE leak into the brain's environment as a stale cause
/// list: the brain env must hold an emptied BAGGAGE line instead.
#[tokio::test]
async fn a_prompt_without_causes_leaves_no_inherited_baggage_in_the_brain_env() {
    let _captured = testing::capture_exclusive();
    let origin = tracing::info_span!("test-env-origin-empty");
    let _carrier = origin.in_scope(carrier::capture);
    let (_t, gait_dir, repo) = workspace();
    write_frames(
        &gait_dir,
        0,
        &[
            append_event(gait_core::proto::events::assistant_text("working".into())),
            forward_event(gait_core::proto::events::assistant_text("working".into())),
            terminated(r#"{"text":"ok"}"#),
        ],
    );
    let (channel, server) = gait_acp::build_agent(SessionMap::new()).into_channel_and_future();
    tokio::spawn(server);
    unsafe {
        std::env::set_var("BAGGAGE", "action_id=STALE");
    }
    Client
        .builder()
        .connect_with(channel, async |cx| {
            let sid = cx
                .send_request(NewSessionRequest::new(repo.clone()))
                .block_task()
                .await?
                .session_id;
            let req = PromptRequest::new(sid, vec!["go".to_string().into()]);
            let resp = cx.send_request(req).block_task().await?;
            assert_eq!(resp.stop_reason, StopReason::EndTurn);
            Ok(())
        })
        .await
        .expect("connection stays alive");
    drop(origin);
    unsafe {
        std::env::remove_var("BAGGAGE");
    }

    let env =
        std::fs::read_to_string(gait_dir.join("mock_env.0.txt")).expect("turn 1 brain env file");
    let baggage = env
        .lines()
        .find(|l| l.starts_with("BAGGAGE="))
        .expect("turn 1 baggage line");
    assert_eq!(
        baggage, "BAGGAGE=",
        "the stale inherited BAGGAGE must not reach the brain env"
    );
}

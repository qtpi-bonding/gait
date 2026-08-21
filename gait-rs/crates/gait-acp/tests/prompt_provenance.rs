//! End-to-end proof that an ACP provenance signature survives the prompt path.
use agent_client_protocol::schema::v1::{NewSessionRequest, PromptRequest};
use agent_client_protocol::{Client, ConnectTo};
use base64::Engine as _;
use gait_identity::Signer as _;
use std::path::Path;

fn git(dir: &Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "test@test.com")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "test@test.com")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn init_git(dir: &Path) {
    git(dir, &["init"]);
    git(dir, &["commit", "--allow-empty", "-m", "init"]);
    let _ = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["branch", "-m", "master", "main"])
        .output();
}

fn workspace(root: &Path) -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let temp = tempfile::TempDir::new_in(root).unwrap();
    let home = temp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    unsafe {
        std::env::set_var("GAIT_HOME", &home);
    }
    init_git(&home);
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init_git(&repo);
    std::fs::write(
        repo.join(".gaitref"),
        format!("gaitdir: {}\n", home.display()),
    )
    .unwrap();
    std::fs::create_dir_all(home.join("inferences/mock")).unwrap();
    std::fs::write(
        home.join("inferences/mock/INFERENCE.yaml"),
        "model: mock/model\ncompaction_threshold_tokens: 128000\n",
    )
    .unwrap();
    std::fs::write(home.join("gait.jsonc"), r#"{ "inference": "mock" }"#).unwrap();
    (temp, home, repo)
}

fn set_brain_exe() {
    unsafe {
        std::env::set_var("GAIT_BRAIN_EXE", env!("CARGO_BIN_EXE_mock_brain_acp"));
    }
}

fn bake_say_hi(home: &Path) {
    use gait_core::proto::gait::log::v1::{StreamItem, stream_item};
    use gait_proto::gait::sandbox::v1::{Append, Signal, StreamForward, Terminated, signal};
    use prost::Message;
    let assistant = gait_core::proto::envelope::mint(
        gait_core::proto::events::assistant_text("hi there".into()),
        None,
        gait_core::proto::envelope::EventScope::Step {
            run: 0,
            turn: 0,
            step: 0,
        },
    );
    let signals = [
        Signal {
            corr_id: 0,
            body: Some(signal::Body::Append(Append {
                event: Some(assistant.clone()),
            })),
        },
        Signal {
            corr_id: 0,
            body: Some(signal::Body::StreamForward(StreamForward {
                item: Some(StreamItem {
                    item: Some(stream_item::Item::Event(assistant)),
                }),
            })),
        },
        Signal {
            corr_id: 0,
            body: Some(signal::Body::Terminated(Terminated {
                turn: Some(gait_proto::gait::sandbox::v1::FinalizedTurn {
                    outcome: gait_proto::gait::sandbox::v1::FinalizedTurnOutcome::TextDone as i32,
                    message: "ok".into(),
                    ..Default::default()
                }),
                failed: false,
            })),
        },
    ];
    let mut bytes = Vec::new();
    for signal in signals {
        let encoded = signal.encode_to_vec();
        bytes.push(1);
        bytes.extend_from_slice(&(encoded.len() as u32).to_be_bytes());
        bytes.extend(encoded);
    }
    std::fs::write(home.join("mock_frames.bin"), bytes).unwrap();
}

#[tokio::test]
async fn prompt_request_meta_provenance_lands_on_the_event_and_verifies() {
    set_brain_exe();
    let (_temp, home, repo) = workspace(Path::new("/tmp"));
    bake_say_hi(&home);
    let text = "peer message via multimatrix";
    let body = serde_json::json!({"payload": {"userText": {"content": text}}});
    let hash = gait_proto::hash::content_hash(gait_proto::envelope::LOG_VERSION, &body);
    let signer = gait_identity::Ed25519Signer::generate();
    let signature = signer.sign(hash.as_bytes());
    let from = signer.public_key().to_vec();
    let meta = serde_json::Map::from_iter([(
        "gait/provenance".to_string(),
        serde_json::json!({
            "v": 1, "scheme": "ed25519",
            "from": base64::engine::general_purpose::STANDARD.encode(&from),
            "signature": base64::engine::general_purpose::STANDARD.encode(&signature),
        }),
    )]);

    let map = gait_acp::session_map::SessionMap::new();
    let (channel, server) = gait_acp::build_agent(map).into_channel_and_future();
    tokio::spawn(server);
    let sid = Client
        .connect_with(channel, async |cx| {
            let sid = cx
                .send_request(NewSessionRequest::new(repo.clone()))
                .block_task()
                .await?
                .session_id;
            let request = PromptRequest::new(sid.clone(), vec![text.to_string().into()]).meta(meta);
            let response = cx.send_request(request).block_task().await?;
            Ok((sid, response.stop_reason))
        })
        .await
        .expect("prompt succeeds")
        .0;

    let path = gait_state::session::live::log_path(&home, &sid.0);
    let events = gait_proto::wire::read_events(&path).unwrap();
    let event = events.iter().find(|event| matches!(event.body.as_ref(),
        Some(gait_proto::gait::log::v1::event::Body::Payload(payload)) if
        matches!(payload.kind.as_ref(), Some(gait_proto::gait::log::v1::payload::Kind::UserText(_)))
    )).expect("logged UserText");
    let provenance = event
        .provenance
        .as_ref()
        .expect("provenance lands on UserText");
    assert_eq!(provenance.from, from);
    assert_eq!(provenance.signature, signature);
    assert!(
        gait_identity::verify_provenance(event),
        "real signature must verify"
    );
}

use agent_client_protocol::schema::v1::{NewSessionRequest, PromptRequest};
use agent_client_protocol::Client;
use gait_events::gait::events::v1::{PromptAccepted, TurnCompleted};
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
        home.join("inferences/mock/inference.textproto"),
        "model: \"mock/model\"\ncompaction_threshold_tokens: 128000\n",
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
async fn prompt_with_meta_traceparent_parents_its_events() {
    set_brain_exe();
    let capture = concord_telemetry::testing::capture_exclusive();
    let (_temp, home, repo) = workspace(Path::new("/tmp"));
    bake_say_hi(&home);
    let meta = serde_json::Map::from_iter([(
        "traceparent".to_string(),
        serde_json::json!("00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01"),
    )]);

    let map = gait_acp::session_map::SessionMap::new();
    let (channel, server) = gait_acp::build_agent(map).into_channel_and_future();
    tokio::spawn(server);
    Client
        .connect_with(channel, async |cx| {
            let sid = cx
                .send_request(NewSessionRequest::new(repo.clone()))
                .block_task()
                .await?
                .session_id;
            let request =
                PromptRequest::new(sid, vec!["trace me".to_string().into()]).meta(meta);
            cx.send_request(request).block_task().await?;
            Ok(())
        })
        .await
        .expect("prompt succeeds");

    let expected: [u8; 16] = [
        0x0a, 0xf7, 0x65, 0x19, 0x16, 0xcd, 0x43, 0xdd, 0x84, 0x48, 0xeb, 0x21, 0x1c, 0x80, 0x31,
        0x9c,
    ];
    assert_eq!(
        capture.trace_id_for(<PromptAccepted as concord_telemetry::Event>::TYPE_NAME),
        Some(expected)
    );
    assert_eq!(
        capture.trace_id_for(<TurnCompleted as concord_telemetry::Event>::TYPE_NAME),
        Some(expected)
    );
}

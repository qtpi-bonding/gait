//! Integration coverage for ACP persisted-session attach and replay.
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    InitializeRequest, LoadSessionRequest, ResumeSessionRequest, SessionNotification, SessionUpdate,
};
use agent_client_protocol::{Client, ConnectTo};
use gait_acp::session_map::SessionMap;

fn init_git_repo(dir: &Path) {
    let run = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t.com")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?} failed");
    };
    run(&["init"]);
    run(&["commit", "--allow-empty", "-m", "init"]);
    let _ = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["branch", "-m", "master", "main"])
        .output();
}

fn persisted_session() -> (tempfile::TempDir, PathBuf, PathBuf, String) {
    let temp = tempfile::TempDir::new().unwrap();
    let gait_dir = temp.path().join("home");
    std::fs::create_dir_all(&gait_dir).unwrap();
    init_git_repo(&gait_dir);
    std::fs::write(gait_dir.join("gait.jsonc"), r#"{ "model": "mock/model" }"#).unwrap();

    let cwd = temp.path().join("repo");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(
        cwd.join(".gaitref"),
        format!("gaitdir: {}\n", gait_dir.display()),
    )
    .unwrap();

    let id = "sload123".to_string();
    gait_state::git::Repos::production()
        .traces
        .create_session_worktree(&gait_dir, &id)
        .unwrap();
    let log = gait_state::session::live::log_path(&gait_dir, &id);
    gait_state::session::log::init_session(gait_state::session::log::InitSessionOpts {
        log_path: log.clone(),
        id: id.clone(),
        model: "mock/model".into(),
        codebase_hash: String::new(),
        codebase_dirty: false,
        assigned_by: None,
        codebase_path: Some(cwd.display().to_string()),
        notify_done: false,
    })
    .unwrap();
    gait_state::proto::wire::append(
        &log,
        gait_state::proto::events::user_text("question".into(), None),
    )
    .unwrap();
    gait_state::proto::wire::append(
        &log,
        gait_state::proto::events::assistant_text("answer".into()),
    )
    .unwrap();
    (temp, gait_dir, cwd, id)
}

#[tokio::test]
async fn load_replays_history_and_attaches_a_persisted_session() {
    let (_temp, gait_dir, cwd, id) = persisted_session();
    let map = SessionMap::new();
    let (channel, server) = gait_acp::build_agent(map.clone()).into_channel_and_future();
    tokio::spawn(server);
    let updates = Arc::new(Mutex::new(Vec::new()));
    let updates_handler = updates.clone();

    let response = Client
        .builder()
        .on_receive_notification(
            move |notification: SessionNotification, _cx| {
                let updates_handler = updates_handler.clone();
                async move {
                    updates_handler.lock().unwrap().push(notification.update);
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(channel, async |cx| {
            cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            cx.send_request(LoadSessionRequest::new(id.clone(), cwd.clone()))
                .block_task()
                .await
        })
        .await
        .expect("load succeeds");

    assert_eq!(
        response.modes.unwrap().current_mode_id.0.as_ref(),
        "default"
    );
    assert!(
        map.contains(&gait_dir, &id),
        "load attaches the existing id"
    );
    assert!(matches!(
        &updates.lock().unwrap()[..],
        [
            SessionUpdate::UserMessageChunk(_),
            SessionUpdate::AgentMessageChunk(_)
        ]
    ));
}

#[tokio::test]
async fn resume_attaches_without_replaying_history() {
    let (_temp, gait_dir, cwd, id) = persisted_session();
    let map = SessionMap::new();
    let (channel, server) = gait_acp::build_agent(map.clone()).into_channel_and_future();
    tokio::spawn(server);
    let updates = Arc::new(Mutex::new(Vec::new()));
    let updates_handler = updates.clone();

    let response = Client
        .builder()
        .on_receive_notification(
            move |notification: SessionNotification, _cx| {
                let updates_handler = updates_handler.clone();
                async move {
                    updates_handler.lock().unwrap().push(notification.update);
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(channel, async |cx| {
            cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            cx.send_request(ResumeSessionRequest::new(id.clone(), cwd.clone()))
                .block_task()
                .await
        })
        .await
        .expect("resume succeeds");

    assert_eq!(
        response.modes.unwrap().current_mode_id.0.as_ref(),
        "default"
    );
    assert!(
        map.contains(&gait_dir, &id),
        "resume attaches the existing id"
    );
    assert!(
        updates.lock().unwrap().is_empty(),
        "resume must not replay history"
    );
}

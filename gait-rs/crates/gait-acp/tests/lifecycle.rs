//! Phase 7 integration tests: session/list, session/close, session/delete.
//! No mock brain needed — these handlers are pure gait-ops verbs over the
//! persisted layer. See docs/plans/2026-07-12-acp-adapter-phase7-design.md §10.
use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    CloseSessionRequest, DeleteSessionRequest, InitializeRequest, ListSessionsRequest,
    NewSessionRequest, PromptRequest,
};
use agent_client_protocol::{Client, ConnectTo};
use gait_acp::session_map::SessionMap;
use std::path::{Path, PathBuf};

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

/// gait-dir (its own git repo, session branches live under it) + a separate
/// codebase dir pointing at it via `.gaitref`. Returns (tempdir, gait_dir, codebase).
fn workspace() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let t = tempfile::TempDir::new().unwrap();
    let gait_dir = t.path().join("home");
    std::fs::create_dir_all(&gait_dir).unwrap();
    init_git_repo(&gait_dir);
    let codebase = t.path().join("repo");
    std::fs::create_dir_all(&codebase).unwrap();
    std::fs::write(
        codebase.join(".gaitref"),
        format!("gaitdir: {}\n", gait_dir.display()),
    )
    .unwrap();
    std::fs::write(gait_dir.join("gait.jsonc"), r#"{ "model": "mock/model" }"#).unwrap();
    (t, gait_dir, codebase)
}

/// Persist the on-disk artifacts (session-branch worktree + a log carrying
/// `codebase_path`) for a session id already minted via `session/new`. `ps`
/// (git-branch walk) and `session/list` (log-derived cwd) both require this;
/// `session/new` itself only records in-memory bookkeeping.
fn persist_session_fixture(gait_dir: &Path, id: &str, codebase: &Path) {
    gait_state::git::Repos::production()
        .traces
        .create_session_worktree(gait_dir, id)
        .unwrap();
    let log_path = gait_state::session::live::log_path(gait_dir, id);
    gait_state::session::log::init_session(gait_state::session::log::InitSessionOpts {
        log_path,
        id: id.to_string(),
        model: "mock/model".to_string(),
        codebase_hash: String::new(),
        codebase_dirty: false,
        assigned_by: None,
        codebase_path: Some(codebase.display().to_string()),
        notify_done: false,
    })
    .unwrap();
}

async fn drive<R>(
    map: SessionMap,
    main_fn: impl AsyncFnOnce(
        agent_client_protocol::ConnectionTo<agent_client_protocol::Agent>,
    ) -> agent_client_protocol::Result<R>,
) -> agent_client_protocol::Result<R> {
    let (channel, server_future) = gait_acp::build_agent(map).into_channel_and_future();
    tokio::spawn(server_future);
    Client.connect_with(channel, main_fn).await
}

#[tokio::test]
async fn initialize_advertises_session_lifecycle_capabilities() {
    let resp = drive(SessionMap::new(), async |cx| {
        cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
            .block_task()
            .await
    })
    .await
    .expect("initialize should succeed");

    let caps = resp.agent_capabilities.session_capabilities;
    assert!(
        resp.agent_capabilities.load_session,
        "load capability advertised"
    );
    assert!(caps.list.is_some(), "list capability advertised");
    assert!(caps.resume.is_some(), "resume capability advertised");
    assert!(caps.close.is_some(), "close capability advertised");
    assert!(caps.delete.is_some(), "delete capability advertised");
}

#[tokio::test]
async fn session_list_returns_minted_session_and_honors_cwd_filter() {
    let (_t, gait_dir, codebase) = workspace();

    let mint_map = SessionMap::new();
    let session_id = drive(mint_map, async |cx| {
        Ok(cx
            .send_request(NewSessionRequest::new(codebase.clone()))
            .block_task()
            .await?
            .session_id
            .to_string())
    })
    .await
    .expect("session/new should succeed");

    persist_session_fixture(&gait_dir, &session_id, &codebase);

    let map = SessionMap::new();
    map.insert(gait_dir.clone(), session_id.clone(), codebase.clone());

    let (all, filtered_match, filtered_miss) = drive(map, async |cx| {
        let all = cx
            .send_request(ListSessionsRequest::new().cwd(None))
            .block_task()
            .await?;
        let filtered_match = cx
            .send_request(ListSessionsRequest::new().cwd(Some(codebase.clone())))
            .block_task()
            .await?;
        let other = tempfile::TempDir::new().unwrap();
        let filtered_miss = cx
            .send_request(ListSessionsRequest::new().cwd(Some(other.path().to_path_buf())))
            .block_task()
            .await?;
        Ok((all, filtered_match, filtered_miss))
    })
    .await
    .expect("session/list should succeed");

    assert_eq!(all.sessions.len(), 1);
    assert_eq!(all.sessions[0].session_id.0.as_ref(), session_id);
    assert_eq!(all.sessions[0].cwd, codebase);

    assert_eq!(
        filtered_match.sessions.len(),
        1,
        "matching cwd filter keeps the session"
    );
    assert!(
        filtered_miss.sessions.is_empty(),
        "non-matching cwd filter returns empty"
    );
}

#[tokio::test]
async fn session_close_merges_idle_session_then_prompt_errors_unknown() {
    let (_t, gait_dir, codebase) = workspace();

    let mint_map = SessionMap::new();
    let session_id = drive(mint_map, async |cx| {
        Ok(cx
            .send_request(NewSessionRequest::new(codebase.clone()))
            .block_task()
            .await?
            .session_id
            .to_string())
    })
    .await
    .expect("session/new should succeed");

    persist_session_fixture(&gait_dir, &session_id, &codebase);

    let map = SessionMap::new();
    map.insert(gait_dir.clone(), session_id.clone(), codebase.clone());

    let (close_ok, prompt_err) = drive(map, async |cx| {
        let close_ok = cx
            .send_request(CloseSessionRequest::new(session_id.clone()))
            .block_task()
            .await
            .is_ok();
        let prompt_err = cx
            .send_request(PromptRequest::new(session_id.clone(), Vec::new()))
            .block_task()
            .await
            .is_err();
        Ok((close_ok, prompt_err))
    })
    .await
    .expect("connection stays alive");

    assert!(close_ok, "close on an idle session persists it");
    assert!(
        prompt_err,
        "a subsequent prompt to a closed session errors unknown session"
    );

    let active = gait_ops::session::list(&gait_dir, None).unwrap().active;
    assert!(
        !active.iter().any(|r| r.session_id == session_id),
        "session/<id> branch is gone after persist"
    );
}

#[tokio::test]
async fn session_delete_removes_idle_session_without_merging() {
    let (_t, gait_dir, codebase) = workspace();

    let mint_map = SessionMap::new();
    let session_id = drive(mint_map, async |cx| {
        Ok(cx
            .send_request(NewSessionRequest::new(codebase.clone()))
            .block_task()
            .await?
            .session_id
            .to_string())
    })
    .await
    .expect("session/new should succeed");

    persist_session_fixture(&gait_dir, &session_id, &codebase);

    let map = SessionMap::new();
    map.insert(gait_dir.clone(), session_id.clone(), codebase.clone());

    let delete_ok = drive(map, async |cx| {
        Ok(cx
            .send_request(DeleteSessionRequest::new(session_id.clone()))
            .block_task()
            .await
            .is_ok())
    })
    .await
    .expect("connection stays alive");

    assert!(delete_ok, "delete on an idle session discards it");
    assert!(
        !gait_state::session::live::worktree_dir(&gait_dir, &session_id).exists(),
        "worktree removed"
    );
    let active = gait_ops::session::list(&gait_dir, None).unwrap().active;
    assert!(
        !active.iter().any(|r| r.session_id == session_id),
        "session/<id> branch is gone after discard"
    );
}

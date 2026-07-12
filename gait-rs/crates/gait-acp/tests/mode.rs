//! Phase 6 integration tests: session/set_mode advertisement + activation.
//! No mock brain — set_mode is a pure append+notify handler.
use agent_client_protocol::schema::v1::{
    NewSessionRequest, SessionNotification, SessionUpdate, SetSessionModeRequest,
};
use agent_client_protocol::{Client, ConnectTo};
use gait_acp::session_map::SessionMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

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

/// gait-init'd workspace with one authored mode `plan`. Returns (tempdir, gait_dir, repo).
fn workspace() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
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
    std::fs::write(gait_dir.join("gait.jsonc"), r#"{ "model": "mock/model" }"#).unwrap();
    let md = gait_dir.join("modes").join("plan");
    std::fs::create_dir_all(&md).unwrap();
    std::fs::write(
        md.join("MODE.yaml"),
        "deny: [write]\ndescription: Plan only\n",
    )
    .unwrap();
    (t, gait_dir, repo)
}

#[tokio::test]
async fn set_mode_advertises_activates_and_confirms() {
    let (_t, _gait_dir, repo) = workspace();
    let map = SessionMap::new();
    let (channel, server) = gait_acp::build_agent(map).into_channel_and_future();
    tokio::spawn(server);

    let updates: Arc<Mutex<Vec<SessionUpdate>>> = Arc::new(Mutex::new(Vec::new()));
    let updates_c = updates.clone();

    let (advertised, set_ok, unknown_err) = Client
        .builder()
        .on_receive_notification(
            {
                let updates_c = updates_c.clone();
                move |n: SessionNotification, _cx| {
                    let updates_c = updates_c.clone();
                    async move {
                        updates_c.lock().unwrap().push(n.update);
                        Ok(())
                    }
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(channel, async |cx| {
            let resp = cx
                .send_request(NewSessionRequest::new(repo.clone()))
                .block_task()
                .await?;
            let sid = resp.session_id.clone();
            let advertised = resp.modes.clone();

            // Activate the authored `plan` mode.
            let set_ok = cx
                .send_request(SetSessionModeRequest::new(sid.clone(), "plan"))
                .block_task()
                .await
                .is_ok();

            // An unknown mode must error and append nothing.
            let unknown_err = cx
                .send_request(SetSessionModeRequest::new(sid.clone(), "nonexistent"))
                .block_task()
                .await
                .is_err();

            Ok((advertised, set_ok, unknown_err))
        })
        .await
        .expect("connection stays alive");

    // Advertisement: default baseline + the authored `plan` (with description).
    let state = advertised.expect("NewSessionResponse.modes populated");
    assert_eq!(state.current_mode_id.0.as_ref(), "default");
    let plan = state
        .available_modes
        .iter()
        .find(|m| m.id.0.as_ref() == "plan")
        .expect("plan advertised");
    assert_eq!(plan.description.as_deref(), Some("Plan only"));

    // Activation succeeded; unknown mode errored.
    assert!(set_ok, "set_mode plan returns Ok");
    assert!(unknown_err, "unknown mode is a JSON-RPC error");

    // Exactly one CurrentModeUpdate("plan") was emitted (the unknown one appends nothing).
    let ups = updates.lock().unwrap();
    let modes: Vec<String> = ups
        .iter()
        .filter_map(|u| match u {
            SessionUpdate::CurrentModeUpdate(c) => Some(c.current_mode_id.0.to_string()),
            _ => None,
        })
        .collect();
    assert_eq!(
        modes,
        vec!["plan".to_string()],
        "one confirm, for plan only"
    );
}

use gait_core::git::trace::{GitTraceRepo, TraceRepo};
use std::path::Path;
use tempfile::TempDir;

fn init_git_repo(dir: &Path) {
    let repo = git2::Repository::init(dir).unwrap();
    let mut cfg = repo.config().unwrap();
    cfg.set_str("user.name", "test").unwrap();
    cfg.set_str("user.email", "test@test.com").unwrap();
    let sig = git2::Signature::now("test", "test@test.com").unwrap();
    let tree_id = repo.index().unwrap().write_tree().unwrap();
    let tree = repo.find_tree(tree_id).unwrap();
    repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
        .unwrap();
    repo.find_branch("master", git2::BranchType::Local)
        .and_then(|mut b| {
            b.rename("main", false)?;
            Ok(())
        })
        .ok();
}

#[test]
fn finish_session_removes_from_active() {
    let ws = TempDir::new().unwrap();
    init_git_repo(ws.path());
    GitTraceRepo
        .create_session_worktree(ws.path(), "sfin001")
        .unwrap();

    let before = GitTraceRepo.list_active_sessions(ws.path()).unwrap();
    assert!(before.contains(&"sfin001".to_string()));

    GitTraceRepo.finish_session(ws.path(), "sfin001").unwrap();

    let after = GitTraceRepo.list_active_sessions(ws.path()).unwrap();
    assert!(
        !after.contains(&"sfin001".to_string()),
        "should be merged: {:?}",
        after
    );
}

#[test]
fn sessions_lists_active_only() {
    let ws = TempDir::new().unwrap();
    init_git_repo(ws.path());
    GitTraceRepo
        .create_session_worktree(ws.path(), "sactive")
        .unwrap();
    GitTraceRepo
        .create_session_worktree(ws.path(), "sfinished")
        .unwrap();
    GitTraceRepo.finish_session(ws.path(), "sfinished").unwrap();

    let active = GitTraceRepo.list_active_sessions(ws.path()).unwrap();
    assert!(
        active.contains(&"sactive".to_string()),
        "active not listed: {:?}",
        active
    );
    assert!(
        !active.contains(&"sfinished".to_string()),
        "finished should not be listed: {:?}",
        active
    );
}

#[test]
fn auth_write_list_remove_roundtrip() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("auth.json");
    gait_core::auth::write_api_key_to(&path, "anthropic", "sk-test-abc123").unwrap();
    gait_core::auth::write_api_key_to(&path, "openai", "sk-openai-xyz").unwrap();

    let list = gait_core::auth::list_credentials_from(&path);
    assert_eq!(list.len(), 2);
    assert!(
        list["anthropic"].starts_with("sk-test-"),
        "got: {}",
        list["anthropic"]
    );
    assert!(
        list["anthropic"].ends_with("..."),
        "got: {}",
        list["anthropic"]
    );

    gait_core::auth::remove_api_key_from(&path, "openai").unwrap();
    let list2 = gait_core::auth::list_credentials_from(&path);
    assert_eq!(list2.len(), 1);
    assert!(!list2.contains_key("openai"));
}

#[test]
fn models_filter_works() {
    let models = vec![
        gait_core::models::ModelInfo {
            id: "claude-3-5-sonnet".into(),
            name: "Claude 3.5".into(),
            provider: "anthropic".into(),
            context_length: Some(200_000),
        },
        gait_core::models::ModelInfo {
            id: "gpt-4o".into(),
            name: "GPT-4o".into(),
            provider: "openai".into(),
            context_length: Some(128_000),
        },
        gait_core::models::ModelInfo {
            id: "gpt-4o-mini".into(),
            name: "GPT-4o Mini".into(),
            provider: "openai".into(),
            context_length: Some(128_000),
        },
    ];
    let openai = gait_core::models::filter_models(&models, Some("openai"));
    assert_eq!(openai.len(), 2);
    let claude = gait_core::models::filter_models(&models, Some("claude"));
    assert_eq!(claude.len(), 1);
    assert_eq!(claude[0].provider, "anthropic");
}

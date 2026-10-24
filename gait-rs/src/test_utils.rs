#[cfg(test)]
pub(crate) fn init_git_repo(dir: &std::path::Path) {
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

/// Publishes one agent identity for this test process through `GAIT_AGENT_DIR`,
/// where `gait` finds it at process entry. The directory lives as long as the
/// process, so every test and any child it spawns (`__daemon-exit`) sees a valid
/// identity.
#[cfg(test)]
pub(crate) fn publish_agent_identity() {
    static AGENT: std::sync::OnceLock<identity_keyfile::testkit::TempAgentDir> =
        std::sync::OnceLock::new();
    let agent = AGENT.get_or_init(|| {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        identity_keyfile::testkit::TempAgentDir::new(now, 86_400)
    });
    std::env::set_var("GAIT_AGENT_DIR", agent.path());
}

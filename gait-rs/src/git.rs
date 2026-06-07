use std::path::Path;
use git2::{Repository, BranchType, IndexAddOption, Signature};

pub fn get_codebase_hash(codebase: &Path) -> String {
    match Repository::open(codebase) {
        Ok(repo) => {
            match repo.head() {
                Ok(head) => {
                    match head.peel_to_commit() {
                        Ok(commit) => commit.id().to_string(),
                        Err(_) => "none".to_string(),
                    }
                }
                Err(_) => "none".to_string(),
            }
        }
        Err(_) => "none".to_string(),
    }
}

pub fn get_dirty_files(codebase: &Path) -> Vec<String> {
    let Ok(repo) = Repository::open(codebase) else { return vec![] };
    let Ok(statuses) = repo.statuses(None) else { return vec![] };
    statuses.iter()
        .filter_map(|e| {
            let s = e.status();
            if s.is_empty() || s == git2::Status::CURRENT { return None; }
            e.path().map(|p| p.to_string())
        })
        .collect()
}

pub fn create_session_worktree(workspace: &Path, session_id: &str) -> anyhow::Result<()> {
    let sessions_dir = workspace.join("sessions");
    let session_dir = sessions_dir.join(session_id);
    
    // Already a worktree — idempotent
    if session_dir.join(".git").exists() {
        return Ok(());
    }
    
    // Create sessions directory (ignore error if it already exists)
    let _ = std::fs::create_dir_all(&sessions_dir);
    
    let repo = Repository::open(workspace)?;

    let branch_name = format!("session/{}", session_id);

    // Re-attach if branch exists but dir was cleaned
    if let Ok(branch) = repo.find_branch(&branch_name, BranchType::Local) {
        let reference = branch.into_reference();
        let mut opts = git2::WorktreeAddOptions::new();
        opts.reference(Some(&reference));
        repo.worktree(session_id, &session_dir, Some(&opts))?;
        return Ok(());
    }

    // Fresh: create branch from HEAD, add worktree
    let head_commit = repo.head()?.peel_to_commit()?;
    let branch = repo.branch(&branch_name, &head_commit, false)?;
    let reference = branch.into_reference();
    let mut opts = git2::WorktreeAddOptions::new();
    opts.reference(Some(&reference));
    repo.worktree(session_id, &session_dir, Some(&opts))?;

    // Initial commit: .gitignore with "lock"
    let wt_repo = Repository::open(&session_dir)?;
    std::fs::write(session_dir.join(".gitignore"), "lock\n")?;
    let sig = wt_repo.signature()
        .or_else(|_| Signature::now("gait", "gait@localhost"))?;
    let mut index = wt_repo.index()?;
    index.add_path(Path::new(".gitignore"))?;
    index.write()?;
    let tree_id = index.write_tree()?;
    let tree = wt_repo.find_tree(tree_id)?;
    let parent = wt_repo.head()?.peel_to_commit()?;
    wt_repo.commit(Some("HEAD"), &sig, &sig,
        &format!("session({}): init", session_id), &tree, &[&parent])?;

    Ok(())
}

pub fn commit_session(workspace: &Path, session_id: &str, message: &str) -> anyhow::Result<()> {
    let session_dir = workspace.join("sessions").join(session_id);
    let repo = Repository::open(&session_dir)?;
    let sig = repo.signature()
        .or_else(|_| Signature::now("gait", "gait@localhost"))?;
    let mut index = repo.index()?;
    index.add_all(["."].iter(), IndexAddOption::DEFAULT, None)?;
    index.write()?;
    let tree_id = index.write_tree()?;
    // Nothing to commit if tree matches HEAD
    let head_commit = repo.head()?.peel_to_commit()?;
    if head_commit.tree_id() == tree_id {
        return Ok(());
    }
    let tree = repo.find_tree(tree_id)?;
    repo.commit(Some("HEAD"), &sig, &sig, message, &tree, &[&head_commit])?;
    Ok(())
}

pub fn list_active_sessions(workspace: &Path) -> anyhow::Result<Vec<String>> {
    let repo = Repository::open(workspace)?;
    let main_oid = repo.find_branch("main", BranchType::Local)
        .or_else(|_| repo.find_branch("master", BranchType::Local))?
        .get().peel_to_commit()?.id();
    let mut sessions = vec![];
    for item in repo.branches(Some(BranchType::Local))? {
        let (branch, _) = item?;
        let name = branch.name()?.unwrap_or("").to_string();
        let Some(id) = name.strip_prefix("session/") else { continue };
        let branch_oid = branch.get().peel_to_commit()?.id();
        // Not merged = main is NOT a descendant of branch
        let is_merged = repo.graph_descendant_of(main_oid, branch_oid).unwrap_or(false);
        if !is_merged {
            sessions.push(id.to_string());
        }
    }
    Ok(sessions)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn init_repo(dir: &Path) -> git2::Repository {
        let repo = git2::Repository::init(dir).unwrap();
        // git2 needs user config for commits; set locally
        let mut cfg = repo.config().unwrap();
        cfg.set_str("user.name", "test").unwrap();
        cfg.set_str("user.email", "test@test.com").unwrap();
        // Create initial commit on main so branch ops work
        let sig = git2::Signature::now("test", "test@test.com").unwrap();
        let tree_id = {
            let mut index = repo.index().unwrap();
            index.write_tree().unwrap()
        };
        let tree = repo.find_tree(tree_id).unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[]).unwrap();
        drop(tree);
        // Rename branch to main
        let _ = repo.find_branch("master", git2::BranchType::Local)
            .or_else(|_| repo.find_branch("main", git2::BranchType::Local))
            .and_then(|mut b| { b.rename("main", false)?; Ok(()) });
        repo
    }

    #[test]
    fn get_codebase_hash_returns_hex() {
        let dir = TempDir::new().unwrap();
        init_repo(dir.path());
        let hash = get_codebase_hash(dir.path());
        assert_eq!(hash.len(), 40, "expected 40-char SHA1, got: {hash}");
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()), "not hex: {hash}");
    }

    #[test]
    fn get_dirty_files_empty_on_clean_repo() {
        let dir = TempDir::new().unwrap();
        init_repo(dir.path());
        assert!(get_dirty_files(dir.path()).is_empty());
    }

    #[test]
    fn get_dirty_files_lists_untracked() {
        let dir = TempDir::new().unwrap();
        init_repo(dir.path());
        std::fs::write(dir.path().join("new.txt"), "hello").unwrap();
        let dirty = get_dirty_files(dir.path());
        assert!(!dirty.is_empty(), "expected dirty files");
    }

    #[test]
    fn create_session_worktree_creates_dir_and_branch() {
        let ws = TempDir::new().unwrap();
        init_repo(ws.path());
        create_session_worktree(ws.path(), "sabc1234").unwrap();
        assert!(ws.path().join("sessions/sabc1234").exists());
        assert!(ws.path().join("sessions/sabc1234/.git").exists());
    }

    #[test]
    fn create_session_worktree_idempotent() {
        let ws = TempDir::new().unwrap();
        init_repo(ws.path());
        create_session_worktree(ws.path(), "sabc1234").unwrap();
        create_session_worktree(ws.path(), "sabc1234").unwrap(); // no error
    }

    #[test]
    fn commit_session_creates_commit() {
        let ws = TempDir::new().unwrap();
        init_repo(ws.path());
        create_session_worktree(ws.path(), "sabc1234").unwrap();
        let tx = ws.path().join("sessions/sabc1234/transcript.jsonl");
        std::fs::write(&tx, "{\"type\":\"session_start\"}\n").unwrap();
        commit_session(ws.path(), "sabc1234", "session(sabc1234): turn 1").unwrap();
        // Verify: repo in session dir has 2+ commits (init + our commit)
        let repo = git2::Repository::open(ws.path().join("sessions/sabc1234")).unwrap();
        let head = repo.head().unwrap().peel_to_commit().unwrap();
        assert_eq!(head.message().unwrap(), "session(sabc1234): turn 1");
    }

    #[test]
    fn list_active_sessions_returns_unmerged() {
        let ws = TempDir::new().unwrap();
        init_repo(ws.path());
        create_session_worktree(ws.path(), "sabc1234").unwrap();
        let sessions = list_active_sessions(ws.path()).unwrap();
        assert!(sessions.contains(&"sabc1234".to_string()), "got: {:?}", sessions);
    }
}

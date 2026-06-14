#[allow(unused_imports)]
use git2::{Repository, IndexAddOption, ResetType, Oid, Status, StatusOptions};
use std::path::Path;
use std::sync::Mutex;
use super::{signature_or_default, checkpoint_git_dir};

pub trait CheckpointRepo: Send + Sync {
    fn init(&self, gait_dir: &Path, session_id: &str, codebase: &Path) -> anyhow::Result<()>;
    fn checkpoint(&self, gait_dir: &Path, session_id: &str, codebase: &Path, turn: u32) -> anyhow::Result<String>;
    fn restore_to(&self, gait_dir: &Path, session_id: &str, codebase: &Path, sha: &str) -> anyhow::Result<()>;
    fn remove(&self, gait_dir: &Path, session_id: &str) -> anyhow::Result<()>;
    fn diff(&self, gait_dir: &Path, session_id: &str, sha: &str) -> anyhow::Result<Option<crate::proto::gait::log::v1::Changes>>;
}

pub struct GitCheckpointRepo;

impl CheckpointRepo for GitCheckpointRepo {
    fn init(&self, gait_dir: &Path, session_id: &str, codebase: &Path) -> anyhow::Result<()> {
        let cb = Repository::open(codebase).ok(); // None for non-git — shadow still created
        let git_dir = checkpoint_git_dir(gait_dir, session_id);
        if git_dir.join("HEAD").exists() { return Ok(()); }
        std::fs::create_dir_all(&git_dir)?;
        let shadow = Repository::init_bare(&git_dir)?;
        if let Some(ref cb) = cb {
            let git_path = cb.path();
            let cb_objects_raw = {
                let commondir_file = git_path.join("commondir");
                if commondir_file.exists() {
                    let rel = std::fs::read_to_string(&commondir_file).unwrap_or_default();
                    git_path.join(rel.trim()).join("objects")
                } else { git_path.join("objects") }
            };
            let cb_objects = std::fs::canonicalize(&cb_objects_raw).unwrap_or(cb_objects_raw);
            let info_dir = git_dir.join("objects").join("info");
            std::fs::create_dir_all(&info_dir)?;
            std::fs::write(info_dir.join("alternates"), format!("{}\n", cb_objects.display()))?;
        }
        let exclude_dir = git_dir.join("info");
        std::fs::create_dir_all(&exclude_dir)?;
        std::fs::write(exclude_dir.join("exclude"), ".git\n")?;
        if let Some(ref cb) = cb {
            if let Ok(head) = cb.head() {
                if let Ok(commit) = head.peel_to_commit() {
                    shadow.reference("refs/heads/rewind", commit.id(), true, "init checkpoint")?;
                    shadow.set_head("refs/heads/rewind")?;
                }
            }
        }
        Ok(())
    }

    fn checkpoint(&self, gait_dir: &Path, session_id: &str, codebase: &Path, turn: u32) -> anyhow::Result<String> {
        let git_dir = checkpoint_git_dir(gait_dir, session_id);
        if !git_dir.join("HEAD").exists() {
            return Ok(String::new()); // no shadow (non-git codebase) — checkpoint unavailable
        }

        let shadow = Repository::open_bare(&git_dir)?;
        shadow.set_workdir(codebase, false)?;

        let mut index = shadow.index()?;
        // Equivalent of `git add -A`, but skip nested git repos (linked worktrees
        // and submodules). libgit2 validates directory paths as gitlink entries
        // before consulting gitignore rules, so a directory containing a `.git`
        // file would cause "invalid path" errors without this callback guard.
        index.add_all(["."].iter(), IndexAddOption::DEFAULT, Some(&mut |path: &std::path::Path, _: &[u8]| -> i32 {
            if codebase.join(path).join(".git").exists() { 1 } else { 0 }
        }))?;
        index.write()?;
        let tree_id = index.write_tree()?;

        // Clean check: if the tree matches HEAD's tree, make no new commit.
        let parent = shadow.head().ok().and_then(|h| h.peel_to_commit().ok());
        if let Some(ref p) = parent {
            if p.tree_id() == tree_id {
                return Ok(p.id().to_string());
            }
        }

        let tree = shadow.find_tree(tree_id)?;
        let sig = signature_or_default(&shadow)?;
        let msg = format!("checkpoint({}): turn {}", session_id, turn);
        let parents: Vec<&git2::Commit> = parent.iter().collect();
        let oid = shadow.commit(Some("HEAD"), &sig, &sig, &msg, &tree, &parents)?;
        Ok(oid.to_string())
    }
    fn restore_to(&self, gait_dir: &Path, session_id: &str, codebase: &Path, sha: &str) -> anyhow::Result<()> {
        let git_dir = checkpoint_git_dir(gait_dir, session_id);
        let shadow = Repository::open_bare(&git_dir)?;
        shadow.set_workdir(codebase, false)?;

        let oid = Oid::from_str(sha)?;
        let obj = shadow.find_object(oid, None)?;
        shadow.reset(&obj, ResetType::Hard, None)?;

        // reset --hard does not remove untracked files. Delete anything the
        // working tree now reports as new so the codebase matches the snapshot
        // exactly.
        let mut so = StatusOptions::new();
        so.include_untracked(true).recurse_untracked_dirs(true).include_ignored(false);
        let statuses = shadow.statuses(Some(&mut so))?;
        for e in statuses.iter() {
            if e.status().contains(Status::WT_NEW) {
                if let Some(rel) = e.path() {
                    let abs = codebase.join(rel);
                    if abs.is_dir() {
                        let _ = std::fs::remove_dir_all(&abs);
                    } else {
                        let _ = std::fs::remove_file(&abs);
                    }
                }
            }
        }
        Ok(())
    }
    fn remove(&self, gait_dir: &Path, session_id: &str) -> anyhow::Result<()> {
        let git_dir = checkpoint_git_dir(gait_dir, session_id);
        if git_dir.exists() {
            std::fs::remove_dir_all(&git_dir)?;
        }
        Ok(())
    }
    fn diff(&self, gait_dir: &Path, session_id: &str, sha: &str) -> anyhow::Result<Option<crate::proto::gait::log::v1::Changes>> {
        use crate::proto::gait::log::v1::Changes;
        let git_dir = checkpoint_git_dir(gait_dir, session_id);
        if !git_dir.join("HEAD").exists() { return Ok(None); }
        let shadow = Repository::open_bare(&git_dir)?;
        let commit = shadow.find_commit(Oid::from_str(sha)?)?;
        let new_tree = commit.tree()?;
        let parent_tree = match commit.parent(0) { Ok(p) => Some(p.tree()?), Err(_) => None };
        let diff = shadow.diff_tree_to_tree(parent_tree.as_ref(), Some(&new_tree), None)?;
        let (mut added, mut modified, mut deleted) = (vec![], vec![], vec![]);
        diff.foreach(&mut |delta, _| {
            if let Some(p) = delta.new_file().path().or_else(|| delta.old_file().path())
                .map(|p| p.to_string_lossy().into_owned()) {
                match delta.status() {
                    git2::Delta::Added | git2::Delta::Copied => added.push(p),
                    git2::Delta::Deleted => deleted.push(p),
                    _ => modified.push(p),
                }
            }
            true
        }, None, None, None)?;
        Ok(Some(Changes { added, modified, deleted }))
    }

}

#[derive(Default)]
struct MockState {
    init_calls: Vec<String>,
    checkpoint_calls: Vec<(String, u32)>,
    restore_calls: Vec<(String, String)>,
    remove_calls: Vec<String>,
    checkpoint_error: Option<String>,
    next_sha: u32,
}

/// In-memory CheckpointRepo for orchestration tests. Records calls, no filesystem.
pub struct MockCheckpointRepo {
    state: Mutex<MockState>,
}

impl MockCheckpointRepo {
    pub fn new() -> Self { Self { state: Mutex::new(MockState::default()) } }
    pub fn inject_checkpoint_error(&self, msg: &str) {
        self.state.lock().unwrap().checkpoint_error = Some(msg.to_string());
    }
    pub fn init_calls(&self) -> Vec<String> { self.state.lock().unwrap().init_calls.clone() }
    pub fn checkpoint_calls(&self) -> Vec<(String, u32)> { self.state.lock().unwrap().checkpoint_calls.clone() }
    pub fn restore_calls(&self) -> Vec<(String, String)> { self.state.lock().unwrap().restore_calls.clone() }
    pub fn remove_calls(&self) -> Vec<String> { self.state.lock().unwrap().remove_calls.clone() }
}

impl Default for MockCheckpointRepo {
    fn default() -> Self { Self::new() }
}

impl CheckpointRepo for MockCheckpointRepo {
    fn init(&self, _ws: &Path, session_id: &str, _cb: &Path) -> anyhow::Result<()> {
        self.state.lock().unwrap().init_calls.push(session_id.to_string());
        Ok(())
    }
    fn checkpoint(&self, _ws: &Path, session_id: &str, _cb: &Path, turn: u32) -> anyhow::Result<String> {
        let mut s = self.state.lock().unwrap();
        s.checkpoint_calls.push((session_id.to_string(), turn));
        if let Some(msg) = s.checkpoint_error.clone() {
            return Err(anyhow::anyhow!(msg));
        }
        s.next_sha += 1;
        Ok(format!("mock-sha-{}", s.next_sha))
    }
    fn restore_to(&self, _ws: &Path, session_id: &str, _cb: &Path, sha: &str) -> anyhow::Result<()> {
        self.state.lock().unwrap().restore_calls.push((session_id.to_string(), sha.to_string()));
        Ok(())
    }
    fn remove(&self, _ws: &Path, session_id: &str) -> anyhow::Result<()> {
        self.state.lock().unwrap().remove_calls.push(session_id.to_string());
        Ok(())
    }
    fn diff(&self, _ws: &Path, _session_id: &str, _sha: &str) -> anyhow::Result<Option<crate::proto::gait::log::v1::Changes>> { Ok(None) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// A gait_dir with sessions/<id>/ present, plus a separate codebase repo
    /// with one commit. Returns (workspace_tmp, codebase_tmp, session_id).
    fn fixture() -> (TempDir, TempDir, String) {
        let ws = TempDir::new().unwrap();
        let session_id = "stest001".to_string();
        std::fs::create_dir_all(ws.path().join("sessions").join(&session_id)).unwrap();

        let cb = TempDir::new().unwrap();
        let repo = Repository::init(cb.path()).unwrap();
        let mut cfg = repo.config().unwrap();
        cfg.set_str("user.email", "t@t.com").unwrap();
        cfg.set_str("user.name", "T").unwrap();
        std::fs::write(cb.path().join("a.txt"), "one\n").unwrap();
        let mut idx = repo.index().unwrap();
        idx.add_path(Path::new("a.txt")).unwrap();
        idx.write().unwrap();
        let tree = repo.find_tree(idx.write_tree().unwrap()).unwrap();
        let sig = repo.signature().unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[]).unwrap();

        (ws, cb, session_id)
    }

    #[test]
    fn init_creates_shadow_db_with_alternates() {
        let (ws, cb, id) = fixture();
        GitCheckpointRepo.init(ws.path(), &id, cb.path()).unwrap();

        let git_dir = checkpoint_git_dir(ws.path(), &id);
        assert!(git_dir.join("HEAD").exists(), "shadow HEAD missing");
        let alt = git_dir.join("objects").join("info").join("alternates");
        assert!(alt.exists(), "alternates file missing");
        let content = std::fs::read_to_string(&alt).unwrap();
        assert!(content.contains(".git/objects"), "alternates should point at codebase objects");
        assert!(std::path::Path::new(content.trim()).is_absolute(), "alternates entry must be absolute, got: {content}");
    }

    #[test]
    fn init_does_not_write_codebase_git() {
        let (ws, cb, id) = fixture();
        let before = Repository::open(cb.path()).unwrap()
            .head().unwrap().peel_to_commit().unwrap().id();
        GitCheckpointRepo.init(ws.path(), &id, cb.path()).unwrap();
        let after = Repository::open(cb.path()).unwrap()
            .head().unwrap().peel_to_commit().unwrap().id();
        assert_eq!(before, after, "codebase HEAD must be untouched");
    }

    #[test]
    fn init_idempotent() {
        let (ws, cb, id) = fixture();
        GitCheckpointRepo.init(ws.path(), &id, cb.path()).unwrap();
        GitCheckpointRepo.init(ws.path(), &id, cb.path()).unwrap(); // must not error
    }

    #[test]
    fn init_creates_shadow_for_non_git_codebase() {
        let ws = TempDir::new().unwrap();
        let id = "snogit".to_string();
        std::fs::create_dir_all(ws.path().join("sessions").join(&id)).unwrap();
        let cb = TempDir::new().unwrap(); // not a git repo
        GitCheckpointRepo.init(ws.path(), &id, cb.path()).unwrap();
        assert!(checkpoint_git_dir(ws.path(), &id).join("HEAD").exists());
    }

    #[test]
    fn checkpoint_after_edit_returns_fresh_sha() {
        let (ws, cb, id) = fixture();
        GitCheckpointRepo.init(ws.path(), &id, cb.path()).unwrap();
        std::fs::write(cb.path().join("a.txt"), "two\n").unwrap();
        let sha = GitCheckpointRepo.checkpoint(ws.path(), &id, cb.path(), 1).unwrap();
        assert_eq!(sha.len(), 40);
    }

    #[test]
    fn checkpoint_no_change_returns_same_sha() {
        let (ws, cb, id) = fixture();
        GitCheckpointRepo.init(ws.path(), &id, cb.path()).unwrap();
        std::fs::write(cb.path().join("a.txt"), "two\n").unwrap();
        let first = GitCheckpointRepo.checkpoint(ws.path(), &id, cb.path(), 1).unwrap();
        let second = GitCheckpointRepo.checkpoint(ws.path(), &id, cb.path(), 2).unwrap();
        assert_eq!(first, second, "no change -> no new commit");
    }

    #[test]
    fn checkpoint_captures_shell_style_changes_and_ignores_dotgit() {
        let (ws, cb, id) = fixture();
        GitCheckpointRepo.init(ws.path(), &id, cb.path()).unwrap();
        std::fs::write(cb.path().join("created.txt"), "x\n").unwrap();
        std::fs::remove_file(cb.path().join("a.txt")).unwrap();
        let sha = GitCheckpointRepo.checkpoint(ws.path(), &id, cb.path(), 1).unwrap();

        let git_dir = checkpoint_git_dir(ws.path(), &id);
        let shadow = Repository::open_bare(&git_dir).unwrap();
        let oid = Oid::from_str(&sha).unwrap();
        let tree = shadow.find_commit(oid).unwrap().tree().unwrap();
        assert!(tree.get_name("created.txt").is_some(), "created file should be captured");
        assert!(tree.get_name("a.txt").is_none(), "deleted file should be gone");
        assert!(tree.get_name(".git").is_none(), "codebase .git must never be tracked");
    }

    #[test]
    fn checkpoint_works_without_codebase_git() {
        let ws = TempDir::new().unwrap();
        let id = "snogit2".to_string();
        std::fs::create_dir_all(ws.path().join("sessions").join(&id)).unwrap();
        let cb = TempDir::new().unwrap();
        GitCheckpointRepo.init(ws.path(), &id, cb.path()).unwrap();
        std::fs::write(cb.path().join("a.txt"), "v1\n").unwrap();
        let sha = GitCheckpointRepo.checkpoint(ws.path(), &id, cb.path(), 1).unwrap();
        assert_eq!(sha.len(), 40);
    }

    #[test]
    fn checkpoint_and_diff_work_for_non_git_codebase() {
        let ws = TempDir::new().unwrap();
        let id = "snogit3".to_string();
        std::fs::create_dir_all(ws.path().join("sessions").join(&id)).unwrap();
        let cb = TempDir::new().unwrap(); // NOT a git repo
        GitCheckpointRepo.init(ws.path(), &id, cb.path()).unwrap();
        assert!(checkpoint_git_dir(ws.path(), &id).join("HEAD").exists());
        std::fs::write(cb.path().join("f.txt"), "v1\n").unwrap();
        let sha = GitCheckpointRepo.checkpoint(ws.path(), &id, cb.path(), 1).unwrap();
        assert_eq!(sha.len(), 40);
        let ch = GitCheckpointRepo.diff(ws.path(), &id, &sha).unwrap().unwrap();
        assert_eq!(ch.added, vec!["f.txt".to_string()]);
    }

    #[test]
    fn restore_to_restores_file_content() {
        let (ws, cb, id) = fixture();
        GitCheckpointRepo.init(ws.path(), &id, cb.path()).unwrap();
        std::fs::write(cb.path().join("a.txt"), "v1\n").unwrap();
        let cp1 = GitCheckpointRepo.checkpoint(ws.path(), &id, cb.path(), 1).unwrap();
        std::fs::write(cb.path().join("a.txt"), "v2\n").unwrap();
        GitCheckpointRepo.checkpoint(ws.path(), &id, cb.path(), 2).unwrap();

        GitCheckpointRepo.restore_to(ws.path(), &id, cb.path(), &cp1).unwrap();
        assert_eq!(std::fs::read_to_string(cb.path().join("a.txt")).unwrap(), "v1\n");
    }

    #[test]
    fn restore_to_removes_files_created_after_checkpoint() {
        let (ws, cb, id) = fixture();
        GitCheckpointRepo.init(ws.path(), &id, cb.path()).unwrap();
        let cp1 = GitCheckpointRepo.checkpoint(ws.path(), &id, cb.path(), 1).unwrap();
        std::fs::write(cb.path().join("new.txt"), "later\n").unwrap();
        GitCheckpointRepo.checkpoint(ws.path(), &id, cb.path(), 2).unwrap();

        GitCheckpointRepo.restore_to(ws.path(), &id, cb.path(), &cp1).unwrap();
        assert!(!cb.path().join("new.txt").exists(), "post-checkpoint file should be removed");
    }

    #[test]
    fn restore_to_does_not_touch_codebase_git() {
        let (ws, cb, id) = fixture();
        let before = Repository::open(cb.path()).unwrap()
            .head().unwrap().peel_to_commit().unwrap().id();
        GitCheckpointRepo.init(ws.path(), &id, cb.path()).unwrap();
        std::fs::write(cb.path().join("a.txt"), "v2\n").unwrap();
        let cp1 = GitCheckpointRepo.checkpoint(ws.path(), &id, cb.path(), 1).unwrap();
        GitCheckpointRepo.restore_to(ws.path(), &id, cb.path(), &cp1).unwrap();
        let after = Repository::open(cb.path()).unwrap()
            .head().unwrap().peel_to_commit().unwrap().id();
        assert_eq!(before, after, "user's real git HEAD must be unchanged");
    }

    #[test]
    fn remove_deletes_shadow_db() {
        let (ws, cb, id) = fixture();
        GitCheckpointRepo.init(ws.path(), &id, cb.path()).unwrap();
        assert!(checkpoint_git_dir(ws.path(), &id).exists());
        GitCheckpointRepo.remove(ws.path(), &id).unwrap();
        assert!(!checkpoint_git_dir(ws.path(), &id).exists());
    }

    #[test]
    fn remove_leaves_codebase_intact() {
        let (ws, cb, id) = fixture();
        GitCheckpointRepo.init(ws.path(), &id, cb.path()).unwrap();
        GitCheckpointRepo.remove(ws.path(), &id).unwrap();
        assert!(cb.path().join("a.txt").exists(), "codebase files must remain");
    }

    #[test]
    fn remove_safe_when_absent() {
        let ws = TempDir::new().unwrap();
        GitCheckpointRepo.remove(ws.path(), "never-existed").unwrap(); // must not error
    }
    #[test]
    fn diff_reports_added_modified_deleted() {
        let (ws, cb, id) = fixture();
        GitCheckpointRepo.init(ws.path(), &id, cb.path()).unwrap();
        std::fs::write(cb.path().join("a.txt"), "two\n").unwrap();
        std::fs::write(cb.path().join("new.txt"), "x\n").unwrap();
        let sha = GitCheckpointRepo.checkpoint(ws.path(), &id, cb.path(), 1).unwrap();
        let ch = GitCheckpointRepo.diff(ws.path(), &id, &sha).unwrap().unwrap();
        assert_eq!(ch.modified, vec!["a.txt".to_string()]);
        assert_eq!(ch.added, vec!["new.txt".to_string()]);
        assert!(ch.deleted.is_empty());
    }

    #[test]
    fn init_works_for_worktree_codebase() {
        use git2::WorktreeAddOptions;
        let ws = TempDir::new().unwrap();
        let id = "swt001".to_string();
        std::fs::create_dir_all(ws.path().join("sessions").join(&id)).unwrap();

        // main codebase repo with one commit
        let main = TempDir::new().unwrap();
        let repo = Repository::init(main.path()).unwrap();
        { let mut c = repo.config().unwrap(); c.set_str("user.email","t@t.com").unwrap(); c.set_str("user.name","T").unwrap(); }
        std::fs::write(main.path().join("a.txt"), "one\n").unwrap();
        let mut idx = repo.index().unwrap();
        idx.add_path(Path::new("a.txt")).unwrap();
        idx.write().unwrap();
        let tree = repo.find_tree(idx.write_tree().unwrap()).unwrap();
        let sig = repo.signature().unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[]).unwrap();

        // linked worktree
        let wt_dir = TempDir::new().unwrap();
        let wt_path = wt_dir.path().join("wt");
        repo.worktree("wt", &wt_path, Some(&WorktreeAddOptions::new())).unwrap();

        // init shadow db pointing at the WORKTREE
        GitCheckpointRepo.init(ws.path(), &id, &wt_path).unwrap();

        let git_dir = checkpoint_git_dir(ws.path(), &id);
        assert!(git_dir.join("HEAD").exists(), "shadow HEAD missing for worktree codebase");
        let alt = std::fs::read_to_string(git_dir.join("objects").join("info").join("alternates")).unwrap();
        assert!(std::path::Path::new(alt.trim()).is_absolute());
        // alternates must point at the MAIN repo's objects (commondir), which actually exists
        assert!(std::path::Path::new(alt.trim()).exists(), "alternates objects dir must exist: {alt}");
    }
}

#[cfg(test)]
mod mock_tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn records_calls_and_injects_error() {
        let mock = MockCheckpointRepo::new();
        let ws = PathBuf::from("/ws");
        let cb = PathBuf::from("/cb");

        mock.init(&ws, "s1", &cb).unwrap();
        let sha = mock.checkpoint(&ws, "s1", &cb, 1).unwrap();
        assert_eq!(sha, "mock-sha-1");
        assert_eq!(mock.init_calls(), vec!["s1".to_string()]);
        assert_eq!(mock.checkpoint_calls(), vec![("s1".to_string(), 1u32)]);

        mock.inject_checkpoint_error("boom");
        let err = mock.checkpoint(&ws, "s1", &cb, 2).unwrap_err();
        assert!(err.to_string().contains("boom"));
        assert_eq!(mock.checkpoint_calls().len(), 2);

        mock.remove(&ws, "s1").unwrap();
        assert_eq!(mock.remove_calls(), vec!["s1".to_string()]);
    }
}

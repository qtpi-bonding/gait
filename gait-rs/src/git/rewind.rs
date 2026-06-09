#[allow(unused_imports)]
use git2::{Repository, IndexAddOption, ResetType, Oid, Status, StatusOptions};
use std::path::Path;
use super::{signature_or_default, rewind_git_dir};

pub trait RewindRepo: Send + Sync {
    fn init(&self, workspace: &Path, session_id: &str, codebase: &Path) -> anyhow::Result<()>;
    fn checkpoint(&self, workspace: &Path, session_id: &str, codebase: &Path, turn: u32) -> anyhow::Result<String>;
    fn rewind_to(&self, workspace: &Path, session_id: &str, codebase: &Path, sha: &str) -> anyhow::Result<()>;
    fn remove(&self, workspace: &Path, session_id: &str) -> anyhow::Result<()>;
}

pub struct GitRewindRepo;

impl RewindRepo for GitRewindRepo {
    fn init(&self, workspace: &Path, session_id: &str, codebase: &Path) -> anyhow::Result<()> {
        // No-op if the codebase is not a git repo — rewind simply unavailable.
        let cb = match Repository::open(codebase) {
            Ok(r) => r,
            Err(_) => {
                eprintln!("rewind: codebase {} is not a git repo; rewind unavailable", codebase.display());
                return Ok(());
            }
        };

        let git_dir = rewind_git_dir(workspace, session_id);
        if git_dir.join("HEAD").exists() {
            return Ok(()); // idempotent — already initialized
        }
        std::fs::create_dir_all(&git_dir)?;

        let shadow = Repository::init_bare(&git_dir)?;

        // Borrow the codebase's object store instead of copying it.
        let cb_objects = codebase.join(".git").join("objects");
        let info_dir = git_dir.join("objects").join("info");
        std::fs::create_dir_all(&info_dir)?;
        std::fs::write(info_dir.join("alternates"), format!("{}\n", cb_objects.display()))?;

        // Belt-and-suspenders: never track the codebase's own .git directory.
        let exclude_dir = git_dir.join("info");
        std::fs::create_dir_all(&exclude_dir)?;
        std::fs::write(exclude_dir.join("exclude"), "/.git/\n")?;

        // Seed the shadow HEAD at the codebase's current commit (if any), so the
        // first checkpoint is a child of the real HEAD. Empty/unborn repo -> leave
        // HEAD unborn; the first checkpoint becomes the root commit.
        if let Ok(head) = cb.head() {
            if let Ok(commit) = head.peel_to_commit() {
                shadow.reference("refs/heads/rewind", commit.id(), true, "init rewind")?;
                shadow.set_head("refs/heads/rewind")?;
            }
        }
        Ok(())
    }

    fn checkpoint(&self, workspace: &Path, session_id: &str, codebase: &Path, turn: u32) -> anyhow::Result<String> {
        let git_dir = rewind_git_dir(workspace, session_id);
        if !git_dir.join("HEAD").exists() {
            return Ok(String::new()); // no shadow (non-git codebase) — rewind unavailable
        }

        let shadow = Repository::open_bare(&git_dir)?;
        shadow.set_workdir(codebase, false)?;

        let mut index = shadow.index()?;
        // Equivalent of `git add -A`. libgit2 always skips a `.git` dir at the
        // worktree root; info/exclude `/.git/` is extra insurance.
        index.add_all(["."].iter(), IndexAddOption::DEFAULT, None)?;
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
        let msg = format!("rewind({}): turn {}", session_id, turn);
        let parents: Vec<&git2::Commit> = parent.iter().collect();
        let oid = shadow.commit(Some("HEAD"), &sig, &sig, &msg, &tree, &parents)?;
        Ok(oid.to_string())
    }
    fn rewind_to(&self, workspace: &Path, session_id: &str, codebase: &Path, sha: &str) -> anyhow::Result<()> {
        let git_dir = rewind_git_dir(workspace, session_id);
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
    fn remove(&self, _workspace: &Path, _session_id: &str) -> anyhow::Result<()> {
        unimplemented!("Task 7")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// A workspace with sessions/<id>/ present, plus a separate codebase repo
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
        GitRewindRepo.init(ws.path(), &id, cb.path()).unwrap();

        let git_dir = rewind_git_dir(ws.path(), &id);
        assert!(git_dir.join("HEAD").exists(), "shadow HEAD missing");
        let alt = git_dir.join("objects").join("info").join("alternates");
        assert!(alt.exists(), "alternates file missing");
        let content = std::fs::read_to_string(&alt).unwrap();
        assert!(content.contains(".git/objects"), "alternates should point at codebase objects");
    }

    #[test]
    fn init_does_not_write_codebase_git() {
        let (ws, cb, id) = fixture();
        let before = Repository::open(cb.path()).unwrap()
            .head().unwrap().peel_to_commit().unwrap().id();
        GitRewindRepo.init(ws.path(), &id, cb.path()).unwrap();
        let after = Repository::open(cb.path()).unwrap()
            .head().unwrap().peel_to_commit().unwrap().id();
        assert_eq!(before, after, "codebase HEAD must be untouched");
    }

    #[test]
    fn init_idempotent() {
        let (ws, cb, id) = fixture();
        GitRewindRepo.init(ws.path(), &id, cb.path()).unwrap();
        GitRewindRepo.init(ws.path(), &id, cb.path()).unwrap(); // must not error
    }

    #[test]
    fn init_noop_for_non_git_codebase() {
        let ws = TempDir::new().unwrap();
        let id = "snogit".to_string();
        std::fs::create_dir_all(ws.path().join("sessions").join(&id)).unwrap();
        let cb = TempDir::new().unwrap(); // not a git repo
        GitRewindRepo.init(ws.path(), &id, cb.path()).unwrap();
        assert!(!rewind_git_dir(ws.path(), &id).join("HEAD").exists());
    }

    #[test]
    fn checkpoint_after_edit_returns_fresh_sha() {
        let (ws, cb, id) = fixture();
        GitRewindRepo.init(ws.path(), &id, cb.path()).unwrap();
        std::fs::write(cb.path().join("a.txt"), "two\n").unwrap();
        let sha = GitRewindRepo.checkpoint(ws.path(), &id, cb.path(), 1).unwrap();
        assert_eq!(sha.len(), 40);
    }

    #[test]
    fn checkpoint_no_change_returns_same_sha() {
        let (ws, cb, id) = fixture();
        GitRewindRepo.init(ws.path(), &id, cb.path()).unwrap();
        std::fs::write(cb.path().join("a.txt"), "two\n").unwrap();
        let first = GitRewindRepo.checkpoint(ws.path(), &id, cb.path(), 1).unwrap();
        let second = GitRewindRepo.checkpoint(ws.path(), &id, cb.path(), 2).unwrap();
        assert_eq!(first, second, "no change -> no new commit");
    }

    #[test]
    fn checkpoint_captures_shell_style_changes_and_ignores_dotgit() {
        let (ws, cb, id) = fixture();
        GitRewindRepo.init(ws.path(), &id, cb.path()).unwrap();
        std::fs::write(cb.path().join("created.txt"), "x\n").unwrap();
        std::fs::remove_file(cb.path().join("a.txt")).unwrap();
        let sha = GitRewindRepo.checkpoint(ws.path(), &id, cb.path(), 1).unwrap();

        let git_dir = rewind_git_dir(ws.path(), &id);
        let shadow = Repository::open_bare(&git_dir).unwrap();
        let oid = Oid::from_str(&sha).unwrap();
        let tree = shadow.find_commit(oid).unwrap().tree().unwrap();
        assert!(tree.get_name("created.txt").is_some(), "created file should be captured");
        assert!(tree.get_name("a.txt").is_none(), "deleted file should be gone");
        assert!(tree.get_name(".git").is_none(), "codebase .git must never be tracked");
    }

    #[test]
    fn checkpoint_noop_when_no_shadow() {
        let ws = TempDir::new().unwrap();
        let id = "snogit2".to_string();
        std::fs::create_dir_all(ws.path().join("sessions").join(&id)).unwrap();
        let cb = TempDir::new().unwrap();
        GitRewindRepo.init(ws.path(), &id, cb.path()).unwrap();
        let sha = GitRewindRepo.checkpoint(ws.path(), &id, cb.path(), 1).unwrap();
        assert!(sha.is_empty());
    }

    #[test]
    fn rewind_to_restores_file_content() {
        let (ws, cb, id) = fixture();
        GitRewindRepo.init(ws.path(), &id, cb.path()).unwrap();
        std::fs::write(cb.path().join("a.txt"), "v1\n").unwrap();
        let cp1 = GitRewindRepo.checkpoint(ws.path(), &id, cb.path(), 1).unwrap();
        std::fs::write(cb.path().join("a.txt"), "v2\n").unwrap();
        GitRewindRepo.checkpoint(ws.path(), &id, cb.path(), 2).unwrap();

        GitRewindRepo.rewind_to(ws.path(), &id, cb.path(), &cp1).unwrap();
        assert_eq!(std::fs::read_to_string(cb.path().join("a.txt")).unwrap(), "v1\n");
    }

    #[test]
    fn rewind_to_removes_files_created_after_checkpoint() {
        let (ws, cb, id) = fixture();
        GitRewindRepo.init(ws.path(), &id, cb.path()).unwrap();
        let cp1 = GitRewindRepo.checkpoint(ws.path(), &id, cb.path(), 1).unwrap();
        std::fs::write(cb.path().join("new.txt"), "later\n").unwrap();
        GitRewindRepo.checkpoint(ws.path(), &id, cb.path(), 2).unwrap();

        GitRewindRepo.rewind_to(ws.path(), &id, cb.path(), &cp1).unwrap();
        assert!(!cb.path().join("new.txt").exists(), "post-checkpoint file should be removed");
    }

    #[test]
    fn rewind_to_does_not_touch_codebase_git() {
        let (ws, cb, id) = fixture();
        let before = Repository::open(cb.path()).unwrap()
            .head().unwrap().peel_to_commit().unwrap().id();
        GitRewindRepo.init(ws.path(), &id, cb.path()).unwrap();
        std::fs::write(cb.path().join("a.txt"), "v2\n").unwrap();
        let cp1 = GitRewindRepo.checkpoint(ws.path(), &id, cb.path(), 1).unwrap();
        GitRewindRepo.rewind_to(ws.path(), &id, cb.path(), &cp1).unwrap();
        let after = Repository::open(cb.path()).unwrap()
            .head().unwrap().peel_to_commit().unwrap().id();
        assert_eq!(before, after, "user's real git HEAD must be unchanged");
    }
}

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

    fn checkpoint(&self, _workspace: &Path, _session_id: &str, _codebase: &Path, _turn: u32) -> anyhow::Result<String> {
        unimplemented!("Task 5")
    }
    fn rewind_to(&self, _workspace: &Path, _session_id: &str, _codebase: &Path, _sha: &str) -> anyhow::Result<()> {
        unimplemented!("Task 6")
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
}

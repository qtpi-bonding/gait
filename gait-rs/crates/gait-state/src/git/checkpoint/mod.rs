use super::{checkpoint_git_dir, signature_or_default};
#[allow(unused_imports)]
use git2::{IndexAddOption, Oid, Repository, ResetType, Status, StatusOptions};
use std::path::Path;
#[cfg(any(test, feature = "testing"))]
use std::sync::Mutex;

pub trait CheckpointRepo: Send + Sync {
    fn init(&self, gait_dir: &Path, session_id: &str, codebase: &Path) -> anyhow::Result<()>;
    fn checkpoint(
        &self,
        gait_dir: &Path,
        session_id: &str,
        codebase: &Path,
        turn: u32,
    ) -> anyhow::Result<String>;
    fn restore_to(
        &self,
        gait_dir: &Path,
        session_id: &str,
        codebase: &Path,
        sha: &str,
    ) -> anyhow::Result<()>;
    fn remove(&self, gait_dir: &Path, session_id: &str) -> anyhow::Result<()>;
    fn diff(
        &self,
        gait_dir: &Path,
        session_id: &str,
        sha: &str,
    ) -> anyhow::Result<Option<crate::proto::gait::log::v1::Changes>>;
}

pub struct GitCheckpointRepo;

impl CheckpointRepo for GitCheckpointRepo {
    fn init(&self, gait_dir: &Path, session_id: &str, codebase: &Path) -> anyhow::Result<()> {
        let cb = Repository::open(codebase).ok(); // None for non-git — shadow still created
        let git_dir = checkpoint_git_dir(gait_dir, session_id);
        if git_dir.join("HEAD").exists() {
            return Ok(());
        }
        std::fs::create_dir_all(&git_dir)?;
        let shadow = Repository::init_bare(&git_dir)?;
        if let Some(ref cb) = cb {
            let git_path = cb.path();
            let cb_objects_raw = {
                let commondir_file = git_path.join("commondir");
                if commondir_file.exists() {
                    let rel = std::fs::read_to_string(&commondir_file).unwrap_or_default();
                    git_path.join(rel.trim()).join("objects")
                } else {
                    git_path.join("objects")
                }
            };
            let cb_objects = std::fs::canonicalize(&cb_objects_raw).unwrap_or(cb_objects_raw);
            let info_dir = git_dir.join("objects").join("info");
            std::fs::create_dir_all(&info_dir)?;
            std::fs::write(
                info_dir.join("alternates"),
                format!("{}\n", cb_objects.display()),
            )?;
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

    fn checkpoint(
        &self,
        gait_dir: &Path,
        session_id: &str,
        codebase: &Path,
        turn: u32,
    ) -> anyhow::Result<String> {
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
        index.add_all(
            ["."].iter(),
            IndexAddOption::DEFAULT,
            Some(&mut |path: &std::path::Path, _: &[u8]| -> i32 {
                if codebase.join(path).join(".git").exists() {
                    1
                } else {
                    0
                }
            }),
        )?;
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
    fn restore_to(
        &self,
        gait_dir: &Path,
        session_id: &str,
        codebase: &Path,
        sha: &str,
    ) -> anyhow::Result<()> {
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
        so.include_untracked(true)
            .recurse_untracked_dirs(true)
            .include_ignored(false);
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
    fn diff(
        &self,
        gait_dir: &Path,
        session_id: &str,
        sha: &str,
    ) -> anyhow::Result<Option<crate::proto::gait::log::v1::Changes>> {
        use crate::proto::gait::log::v1::Changes;
        let git_dir = checkpoint_git_dir(gait_dir, session_id);
        if !git_dir.join("HEAD").exists() {
            return Ok(None);
        }
        let shadow = Repository::open_bare(&git_dir)?;
        let commit = shadow.find_commit(Oid::from_str(sha)?)?;
        let new_tree = commit.tree()?;
        let parent_tree = match commit.parent(0) {
            Ok(p) => Some(p.tree()?),
            Err(_) => None,
        };
        let diff = shadow.diff_tree_to_tree(parent_tree.as_ref(), Some(&new_tree), None)?;
        let (mut added, mut modified, mut deleted) = (vec![], vec![], vec![]);
        diff.foreach(
            &mut |delta, _| {
                if let Some(p) = delta
                    .new_file()
                    .path()
                    .or_else(|| delta.old_file().path())
                    .map(|p| p.to_string_lossy().into_owned())
                {
                    match delta.status() {
                        git2::Delta::Added | git2::Delta::Copied => added.push(p),
                        git2::Delta::Deleted => deleted.push(p),
                        _ => modified.push(p),
                    }
                }
                true
            },
            None,
            None,
            None,
        )?;
        Ok(Some(Changes {
            added,
            modified,
            deleted,
        }))
    }
}

#[cfg(any(test, feature = "testing"))]
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
#[cfg(any(test, feature = "testing"))]
pub struct MockCheckpointRepo {
    state: Mutex<MockState>,
}
#[cfg(any(test, feature = "testing"))]
impl MockCheckpointRepo {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(MockState::default()),
        }
    }
    pub fn inject_checkpoint_error(&self, msg: &str) {
        self.state
            .lock()
            .expect("mock mutex never poisoned")
            .checkpoint_error = Some(msg.to_string());
    }
    pub fn init_calls(&self) -> Vec<String> {
        self.state
            .lock()
            .expect("mock mutex never poisoned")
            .init_calls
            .clone()
    }
    pub fn checkpoint_calls(&self) -> Vec<(String, u32)> {
        self.state
            .lock()
            .expect("mock mutex never poisoned")
            .checkpoint_calls
            .clone()
    }
    pub fn restore_calls(&self) -> Vec<(String, String)> {
        self.state
            .lock()
            .expect("mock mutex never poisoned")
            .restore_calls
            .clone()
    }
    pub fn remove_calls(&self) -> Vec<String> {
        self.state
            .lock()
            .expect("mock mutex never poisoned")
            .remove_calls
            .clone()
    }
}
#[cfg(any(test, feature = "testing"))]
impl Default for MockCheckpointRepo {
    fn default() -> Self {
        Self::new()
    }
}
#[cfg(any(test, feature = "testing"))]
impl CheckpointRepo for MockCheckpointRepo {
    fn init(&self, _ws: &Path, session_id: &str, _cb: &Path) -> anyhow::Result<()> {
        self.state
            .lock()
            .expect("mock mutex never poisoned")
            .init_calls
            .push(session_id.to_string());
        Ok(())
    }
    fn checkpoint(
        &self,
        _ws: &Path,
        session_id: &str,
        _cb: &Path,
        turn: u32,
    ) -> anyhow::Result<String> {
        let mut s = self.state.lock().expect("mock mutex never poisoned");
        s.checkpoint_calls.push((session_id.to_string(), turn));
        if let Some(msg) = s.checkpoint_error.clone() {
            return Err(anyhow::anyhow!(msg));
        }
        s.next_sha += 1;
        Ok(format!("mock-sha-{}", s.next_sha))
    }
    fn restore_to(
        &self,
        _ws: &Path,
        session_id: &str,
        _cb: &Path,
        sha: &str,
    ) -> anyhow::Result<()> {
        self.state
            .lock()
            .expect("mock mutex never poisoned")
            .restore_calls
            .push((session_id.to_string(), sha.to_string()));
        Ok(())
    }
    fn remove(&self, _ws: &Path, session_id: &str) -> anyhow::Result<()> {
        self.state
            .lock()
            .expect("mock mutex never poisoned")
            .remove_calls
            .push(session_id.to_string());
        Ok(())
    }
    fn diff(
        &self,
        _ws: &Path,
        _session_id: &str,
        _sha: &str,
    ) -> anyhow::Result<Option<crate::proto::gait::log::v1::Changes>> {
        Ok(None)
    }
}

#[cfg(test)]
mod tests;

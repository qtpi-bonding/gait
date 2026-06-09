use std::path::Path;
use git2::Repository;

/// Read-only access to the user's codebase git. gait NEVER writes here —
/// this trait deliberately has no mutating methods.
pub trait CodebaseRepo: Send + Sync {
    /// HEAD commit hash of the codebase, or "none" if not a git repo / no commits.
    fn head_hash(&self, codebase: &Path) -> String;
    /// `git status --porcelain`-style list of changed paths; empty if clean or not a repo.
    fn dirty_files(&self, codebase: &Path) -> Vec<String>;
}

/// git2-backed read-only codebase access. Zero-sized — construct inline.
pub struct GitCodebaseRepo;

impl CodebaseRepo for GitCodebaseRepo {
    fn head_hash(&self, codebase: &Path) -> String {
        match Repository::open(codebase) {
            Ok(repo) => match repo.head() {
                Ok(head) => match head.peel_to_commit() {
                    Ok(commit) => commit.id().to_string(),
                    Err(_) => "none".to_string(),
                },
                Err(_) => "none".to_string(),
            },
            Err(_) => "none".to_string(),
        }
    }

    fn dirty_files(&self, codebase: &Path) -> Vec<String> {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn init_repo(dir: &Path) {
        let repo = Repository::init(dir).unwrap();
        let mut cfg = repo.config().unwrap();
        cfg.set_str("user.email", "t@t.com").unwrap();
        cfg.set_str("user.name", "T").unwrap();
        std::fs::write(dir.join("a.txt"), "hi\n").unwrap();
        let mut idx = repo.index().unwrap();
        idx.add_path(Path::new("a.txt")).unwrap();
        idx.write().unwrap();
        let tree = repo.find_tree(idx.write_tree().unwrap()).unwrap();
        let sig = repo.signature().unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[]).unwrap();
    }

    #[test]
    fn head_hash_is_hex_for_a_repo() {
        let d = TempDir::new().unwrap();
        init_repo(d.path());
        let h = GitCodebaseRepo.head_hash(d.path());
        assert_eq!(h.len(), 40);
    }

    #[test]
    fn head_hash_is_none_for_non_repo() {
        let d = TempDir::new().unwrap();
        assert_eq!(GitCodebaseRepo.head_hash(d.path()), "none");
    }

    #[test]
    fn dirty_files_lists_changes() {
        let d = TempDir::new().unwrap();
        init_repo(d.path());
        std::fs::write(d.path().join("b.txt"), "new\n").unwrap();
        let files = GitCodebaseRepo.dirty_files(d.path());
        assert!(files.iter().any(|f| f.contains("b.txt")));
    }
}

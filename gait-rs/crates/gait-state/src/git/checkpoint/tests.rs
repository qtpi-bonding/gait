use super::*;
use std::path::PathBuf;
use tempfile::TempDir;

fn checkpoint_boundary(run: u32) -> CheckpointBoundary {
    CheckpointBoundary::RunFinal { run }
}

/// A gait_dir with sessions/<id>/ present, plus a separate codebase repo
/// with one commit. Returns (workspace_tmp, codebase_tmp, session_id).
fn fixture() -> (TempDir, TempDir, String) {
    let ws = TempDir::new().unwrap();
    let session_id = "stest001".to_string();
    std::fs::create_dir_all(crate::session::live::worktree_dir(ws.path(), &session_id)).unwrap();

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
    repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
        .unwrap();

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
    assert!(
        content.contains(".git/objects"),
        "alternates should point at codebase objects"
    );
    assert!(
        std::path::Path::new(content.trim()).is_absolute(),
        "alternates entry must be absolute, got: {content}"
    );
}

#[test]
fn init_does_not_write_codebase_git() {
    let (ws, cb, id) = fixture();
    let before = Repository::open(cb.path())
        .unwrap()
        .head()
        .unwrap()
        .peel_to_commit()
        .unwrap()
        .id();
    GitCheckpointRepo.init(ws.path(), &id, cb.path()).unwrap();
    let after = Repository::open(cb.path())
        .unwrap()
        .head()
        .unwrap()
        .peel_to_commit()
        .unwrap()
        .id();
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
    std::fs::create_dir_all(crate::session::live::worktree_dir(ws.path(), &id)).unwrap();
    let cb = TempDir::new().unwrap(); // not a git repo
    GitCheckpointRepo.init(ws.path(), &id, cb.path()).unwrap();
    assert!(checkpoint_git_dir(ws.path(), &id).join("HEAD").exists());
}

#[test]
fn checkpoint_after_edit_returns_fresh_sha() {
    let (ws, cb, id) = fixture();
    GitCheckpointRepo.init(ws.path(), &id, cb.path()).unwrap();
    std::fs::write(cb.path().join("a.txt"), "two\n").unwrap();
    let sha = GitCheckpointRepo
        .checkpoint(ws.path(), &id, cb.path(), checkpoint_boundary(1))
        .unwrap();
    assert_eq!(sha.len(), 40);
}

#[test]
fn checkpoint_no_change_returns_same_sha() {
    let (ws, cb, id) = fixture();
    GitCheckpointRepo.init(ws.path(), &id, cb.path()).unwrap();
    std::fs::write(cb.path().join("a.txt"), "two\n").unwrap();
    let first = GitCheckpointRepo
        .checkpoint(ws.path(), &id, cb.path(), checkpoint_boundary(1))
        .unwrap();
    let second = GitCheckpointRepo
        .checkpoint(ws.path(), &id, cb.path(), checkpoint_boundary(2))
        .unwrap();
    assert_eq!(first, second, "no change -> no new commit");
}

#[test]
fn checkpoint_captures_shell_style_changes_and_ignores_dotgit() {
    let (ws, cb, id) = fixture();
    GitCheckpointRepo.init(ws.path(), &id, cb.path()).unwrap();
    std::fs::write(cb.path().join("created.txt"), "x\n").unwrap();
    std::fs::remove_file(cb.path().join("a.txt")).unwrap();
    let sha = GitCheckpointRepo
        .checkpoint(ws.path(), &id, cb.path(), checkpoint_boundary(1))
        .unwrap();

    let git_dir = checkpoint_git_dir(ws.path(), &id);
    let shadow = Repository::open_bare(&git_dir).unwrap();
    let oid = Oid::from_str(&sha).unwrap();
    let tree = shadow.find_commit(oid).unwrap().tree().unwrap();
    assert!(
        tree.get_name("created.txt").is_some(),
        "created file should be captured"
    );
    assert!(
        tree.get_name("a.txt").is_none(),
        "deleted file should be gone"
    );
    assert!(
        tree.get_name(".git").is_none(),
        "codebase .git must never be tracked"
    );
}

#[test]
fn checkpoint_works_without_codebase_git() {
    let ws = TempDir::new().unwrap();
    let id = "snogit2".to_string();
    std::fs::create_dir_all(crate::session::live::worktree_dir(ws.path(), &id)).unwrap();
    let cb = TempDir::new().unwrap();
    GitCheckpointRepo.init(ws.path(), &id, cb.path()).unwrap();
    std::fs::write(cb.path().join("a.txt"), "v1\n").unwrap();
    let sha = GitCheckpointRepo
        .checkpoint(ws.path(), &id, cb.path(), checkpoint_boundary(1))
        .unwrap();
    assert_eq!(sha.len(), 40);
}

#[test]
fn checkpoint_and_diff_work_for_non_git_codebase() {
    let ws = TempDir::new().unwrap();
    let id = "snogit3".to_string();
    std::fs::create_dir_all(crate::session::live::worktree_dir(ws.path(), &id)).unwrap();
    let cb = TempDir::new().unwrap(); // NOT a git repo
    GitCheckpointRepo.init(ws.path(), &id, cb.path()).unwrap();
    assert!(checkpoint_git_dir(ws.path(), &id).join("HEAD").exists());
    let baseline = GitCheckpointRepo
        .checkpoint(ws.path(), &id, cb.path(), checkpoint_boundary(0))
        .unwrap();
    std::fs::write(cb.path().join("f.txt"), "v1\n").unwrap();
    let final_sha = GitCheckpointRepo
        .checkpoint(ws.path(), &id, cb.path(), checkpoint_boundary(1))
        .unwrap();
    assert_eq!(final_sha.len(), 40);
    let ch = GitCheckpointRepo
        .diff_between(ws.path(), &id, &baseline, &final_sha)
        .unwrap()
        .unwrap();
    assert_eq!(ch.added[0].path, "f.txt");
    assert_eq!(ch.added[0].added_lines, Some(1));
    assert_eq!(ch.file_count, 1);
}

#[test]
fn baseline_captures_dirty_start_and_unchanged_run_has_empty_diff() {
    let (ws, cb, id) = fixture();
    GitCheckpointRepo.init(ws.path(), &id, cb.path()).unwrap();
    std::fs::write(cb.path().join("a.txt"), "dirty before run\n").unwrap();
    let baseline = GitCheckpointRepo
        .checkpoint(ws.path(), &id, cb.path(), checkpoint_boundary(0))
        .unwrap();
    let final_sha = GitCheckpointRepo
        .checkpoint(ws.path(), &id, cb.path(), checkpoint_boundary(1))
        .unwrap();
    assert_eq!(baseline, final_sha);
    let summary = GitCheckpointRepo
        .diff_between(ws.path(), &id, &baseline, &final_sha)
        .unwrap()
        .unwrap();
    assert_eq!(summary.file_count, 0);
    assert!(summary.added.is_empty() && summary.modified.is_empty() && summary.removed.is_empty());
}

#[test]
fn baseline_to_final_reports_delete_and_rename_deterministically() {
    let (ws, cb, id) = fixture();
    GitCheckpointRepo.init(ws.path(), &id, cb.path()).unwrap();
    std::fs::write(cb.path().join("delete.txt"), "gone\n").unwrap();
    std::fs::write(cb.path().join("rename-old.txt"), "same content\n").unwrap();
    let baseline = GitCheckpointRepo
        .checkpoint(ws.path(), &id, cb.path(), checkpoint_boundary(0))
        .unwrap();
    std::fs::remove_file(cb.path().join("delete.txt")).unwrap();
    std::fs::rename(
        cb.path().join("rename-old.txt"),
        cb.path().join("rename-new.txt"),
    )
    .unwrap();
    let final_sha = GitCheckpointRepo
        .checkpoint(ws.path(), &id, cb.path(), checkpoint_boundary(1))
        .unwrap();
    let summary = GitCheckpointRepo
        .diff_between(ws.path(), &id, &baseline, &final_sha)
        .unwrap()
        .unwrap();
    assert_eq!(summary.file_count, 2);
    assert_eq!(summary.removed[0].path, "delete.txt");
    assert_eq!(summary.modified[0].path, "rename-new.txt");
}

#[test]
fn binary_change_keeps_file_identity_but_omits_unreliable_line_totals() {
    let (ws, cb, id) = fixture();
    GitCheckpointRepo.init(ws.path(), &id, cb.path()).unwrap();
    std::fs::write(cb.path().join("blob.bin"), [0, 159, 146, 150]).unwrap();
    let baseline = GitCheckpointRepo
        .checkpoint(ws.path(), &id, cb.path(), checkpoint_boundary(0))
        .unwrap();
    std::fs::write(cb.path().join("blob.bin"), [0, 159, 146, 151]).unwrap();
    let final_sha = GitCheckpointRepo
        .checkpoint(ws.path(), &id, cb.path(), checkpoint_boundary(1))
        .unwrap();
    let summary = GitCheckpointRepo
        .diff_between(ws.path(), &id, &baseline, &final_sha)
        .unwrap()
        .unwrap();
    assert_eq!(summary.modified[0].path, "blob.bin");
    assert_eq!(summary.modified[0].added_lines, None);
    assert_eq!(summary.added_lines, None);
    assert_eq!(summary.removed_lines, None);
}

#[test]
fn restore_to_restores_file_content() {
    let (ws, cb, id) = fixture();
    GitCheckpointRepo.init(ws.path(), &id, cb.path()).unwrap();
    std::fs::write(cb.path().join("a.txt"), "v1\n").unwrap();
    let cp1 = GitCheckpointRepo
        .checkpoint(ws.path(), &id, cb.path(), checkpoint_boundary(1))
        .unwrap();
    std::fs::write(cb.path().join("a.txt"), "v2\n").unwrap();
    GitCheckpointRepo
        .checkpoint(ws.path(), &id, cb.path(), checkpoint_boundary(2))
        .unwrap();

    GitCheckpointRepo
        .restore_to(ws.path(), &id, cb.path(), &cp1)
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(cb.path().join("a.txt")).unwrap(),
        "v1\n"
    );
}

#[test]
fn restore_to_removes_files_created_after_checkpoint() {
    let (ws, cb, id) = fixture();
    GitCheckpointRepo.init(ws.path(), &id, cb.path()).unwrap();
    let cp1 = GitCheckpointRepo
        .checkpoint(ws.path(), &id, cb.path(), checkpoint_boundary(1))
        .unwrap();
    std::fs::write(cb.path().join("new.txt"), "later\n").unwrap();
    GitCheckpointRepo
        .checkpoint(ws.path(), &id, cb.path(), checkpoint_boundary(2))
        .unwrap();

    GitCheckpointRepo
        .restore_to(ws.path(), &id, cb.path(), &cp1)
        .unwrap();
    assert!(
        !cb.path().join("new.txt").exists(),
        "post-checkpoint file should be removed"
    );
}

#[test]
fn restore_to_does_not_touch_codebase_git() {
    let (ws, cb, id) = fixture();
    let before = Repository::open(cb.path())
        .unwrap()
        .head()
        .unwrap()
        .peel_to_commit()
        .unwrap()
        .id();
    GitCheckpointRepo.init(ws.path(), &id, cb.path()).unwrap();
    std::fs::write(cb.path().join("a.txt"), "v2\n").unwrap();
    let cp1 = GitCheckpointRepo
        .checkpoint(ws.path(), &id, cb.path(), checkpoint_boundary(1))
        .unwrap();
    GitCheckpointRepo
        .restore_to(ws.path(), &id, cb.path(), &cp1)
        .unwrap();
    let after = Repository::open(cb.path())
        .unwrap()
        .head()
        .unwrap()
        .peel_to_commit()
        .unwrap()
        .id();
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
    assert!(
        cb.path().join("a.txt").exists(),
        "codebase files must remain"
    );
}

#[test]
fn remove_safe_when_absent() {
    let ws = TempDir::new().unwrap();
    GitCheckpointRepo
        .remove(ws.path(), "never-existed")
        .unwrap(); // must not error
}
#[test]
fn diff_reports_added_modified_deleted() {
    let (ws, cb, id) = fixture();
    GitCheckpointRepo.init(ws.path(), &id, cb.path()).unwrap();
    let baseline = GitCheckpointRepo
        .checkpoint(ws.path(), &id, cb.path(), checkpoint_boundary(0))
        .unwrap();
    std::fs::write(cb.path().join("a.txt"), "two\n").unwrap();
    std::fs::write(cb.path().join("new.txt"), "x\n").unwrap();
    let final_sha = GitCheckpointRepo
        .checkpoint(ws.path(), &id, cb.path(), checkpoint_boundary(1))
        .unwrap();
    let ch = GitCheckpointRepo
        .diff_between(ws.path(), &id, &baseline, &final_sha)
        .unwrap()
        .unwrap();
    assert_eq!(ch.modified[0].path, "a.txt");
    assert_eq!(ch.added[0].path, "new.txt");
    assert!(ch.removed.is_empty());
    assert_eq!(ch.file_count, 2);
    assert_eq!(ch.added_lines, Some(2));
    assert_eq!(ch.removed_lines, Some(1));
}

#[test]
fn init_works_for_worktree_codebase() {
    use git2::WorktreeAddOptions;
    let ws = TempDir::new().unwrap();
    let id = "swt001".to_string();
    std::fs::create_dir_all(crate::session::live::worktree_dir(ws.path(), &id)).unwrap();

    // main codebase repo with one commit
    let main = TempDir::new().unwrap();
    let repo = Repository::init(main.path()).unwrap();
    {
        let mut c = repo.config().unwrap();
        c.set_str("user.email", "t@t.com").unwrap();
        c.set_str("user.name", "T").unwrap();
    }
    std::fs::write(main.path().join("a.txt"), "one\n").unwrap();
    let mut idx = repo.index().unwrap();
    idx.add_path(Path::new("a.txt")).unwrap();
    idx.write().unwrap();
    let tree = repo.find_tree(idx.write_tree().unwrap()).unwrap();
    let sig = repo.signature().unwrap();
    repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
        .unwrap();

    // linked worktree
    let wt_dir = TempDir::new().unwrap();
    let wt_path = wt_dir.path().join("wt");
    repo.worktree("wt", &wt_path, Some(&WorktreeAddOptions::new()))
        .unwrap();

    // init shadow db pointing at the WORKTREE
    GitCheckpointRepo.init(ws.path(), &id, &wt_path).unwrap();

    let git_dir = checkpoint_git_dir(ws.path(), &id);
    assert!(
        git_dir.join("HEAD").exists(),
        "shadow HEAD missing for worktree codebase"
    );
    let alt =
        std::fs::read_to_string(git_dir.join("objects").join("info").join("alternates")).unwrap();
    assert!(std::path::Path::new(alt.trim()).is_absolute());
    // alternates must point at the MAIN repo's objects (commondir), which actually exists
    assert!(
        std::path::Path::new(alt.trim()).exists(),
        "alternates objects dir must exist: {alt}"
    );
}

#[test]
fn records_calls_and_injects_error() {
    let mock = MockCheckpointRepo::new();
    let ws = PathBuf::from("/ws");
    let cb = PathBuf::from("/cb");

    mock.init(&ws, "s1", &cb).unwrap();
    let sha = mock
        .checkpoint(&ws, "s1", &cb, checkpoint_boundary(1))
        .unwrap();
    assert_eq!(sha, "mock-sha-1");
    assert_eq!(mock.init_calls(), vec!["s1".to_string()]);
    assert_eq!(
        mock.checkpoint_calls(),
        vec![("s1".to_string(), checkpoint_boundary(1))]
    );

    mock.inject_checkpoint_error("boom");
    let err = mock
        .checkpoint(&ws, "s1", &cb, checkpoint_boundary(2))
        .unwrap_err();
    assert!(err.to_string().contains("boom"));
    assert_eq!(mock.checkpoint_calls().len(), 2);

    mock.remove(&ws, "s1").unwrap();
    assert_eq!(mock.remove_calls(), vec!["s1".to_string()]);
}

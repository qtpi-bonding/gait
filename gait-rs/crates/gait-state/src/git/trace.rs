use super::with_gait_trailer;
use git2::{BranchType, IndexAddOption, Repository, Signature};
use std::path::Path;

pub trait TraceRepo: Send + Sync {
    fn init_workspace(&self, gait_dir: &Path) -> anyhow::Result<()>;
    fn create_session_worktree(&self, gait_dir: &Path, session_id: &str) -> anyhow::Result<()>;
    fn commit_session(
        &self,
        gait_dir: &Path,
        session_id: &str,
        message: &str,
    ) -> anyhow::Result<()>;
    fn list_active_sessions(&self, gait_dir: &Path) -> anyhow::Result<Vec<String>>;
    fn finish_session(&self, gait_dir: &Path, session_id: &str) -> anyhow::Result<()>;
    fn discard_session(&self, gait_dir: &Path, session_id: &str) -> anyhow::Result<()>;
    fn create_branch_worktree(
        &self,
        gait_dir: &Path,
        base_branch: &str,
        new_branch: &str,
        worktree_path: &Path,
    ) -> anyhow::Result<()>;
    fn remove_branch_worktree(&self, gait_dir: &Path, worktree_path: &Path) -> anyhow::Result<()>;
    fn merge_branch_ff(
        &self,
        gait_dir: &Path,
        target_branch: &str,
        source_branch: &str,
    ) -> anyhow::Result<()>;
}

pub struct GitTraceRepo;

impl TraceRepo for GitTraceRepo {
    fn init_workspace(&self, gait_dir: &Path) -> anyhow::Result<()> {
        // git init — no-op if already a repo
        match Repository::init(gait_dir) {
            Ok(_) | Err(_) => {}
        }
        Ok(())
    }

    fn create_session_worktree(&self, gait_dir: &Path, session_id: &str) -> anyhow::Result<()> {
        let worktree_dir = crate::session::live::worktree_dir(gait_dir, session_id);

        // Allow-list content: only sessions/<id>/log.jsonl and
        // sessions/<id>/diagnostic.jsonl are ever tracked. It ignores itself
        // (no `!.gitignore` line) — `.gitignore` lives on disk, untracked,
        // never committed into the session tree. Committing it would make it
        // a tracked file at the tree root, and main's own root `.gitignore`
        // (from `gait init`) is a *different* blob at that same path —
        // committing this one would add/add-conflict with it on the very
        // first persist (an orphan branch has no merge base, so both sides'
        // "add" of a root .gitignore collide). See design spec §2/§3/§5.
        //
        // Written unconditionally at the end of this function (both on the
        // fresh-branch path AND the re-attach path below) — NOT only on the
        // fresh path. If it were fresh-path-only, a session whose `.active/<id>`
        // dir was cleaned up but whose branch survived would re-attach a
        // worktree with no `.gitignore` on disk at all, and the next
        // `commit_session`'s `add_all(".")` would stage `lock`/`codebase.git/`/
        // the session's own `gait.jsonc` at the tree root — silently
        // corrupting the tree back into the exact add/add-collision shape
        // this design eliminates, just via the re-attach path instead of the
        // fresh one. (Caught in a second plan-review pass.)
        const GITIGNORE_CONTENT: &str =
            "*\n!sessions/\n!sessions/*/\n!sessions/*/log.jsonl\n!sessions/*/diagnostic.jsonl\n";

        // Already a worktree — idempotent. Still (re-)write .gitignore: see
        // above, this must not be a no-op. The runtime output root is likewise
        // restored if a caller removed it between turns.
        if worktree_dir.join(".git").exists() {
            std::fs::write(worktree_dir.join(".gitignore"), GITIGNORE_CONTENT)?;
            std::fs::create_dir_all(crate::session::live::outputs_dir(gait_dir, session_id))?;
            return Ok(());
        }

        let _ = std::fs::create_dir_all(gait_dir.join(".active"));

        let repo = Repository::open(gait_dir)?;
        let branch_name = format!("session/{}", session_id);

        // Re-attach if branch exists but dir was cleaned
        if let Ok(branch) = repo.find_branch(&branch_name, BranchType::Local) {
            // libgit2 keeps a worktree's admin metadata at
            // `.git/worktrees/<session_id>/` independently of the working
            // directory. If a previous worktree for this session_id was
            // removed by deleting `.active/<id>/` directly (rather than via
            // `git worktree remove`), that admin entry survives and
            // `repo.worktree(session_id, ...)` below fails with "directory
            // exists" — it refuses to reuse a stale admin dir under the same
            // name. Prune it first if present so re-attach is idempotent
            // regardless of how the previous worktree directory disappeared.
            if let Ok(existing) = repo.find_worktree(session_id) {
                let mut prune_opts = git2::WorktreePruneOptions::new();
                prune_opts.working_tree(true);
                let _ = existing.prune(Some(&mut prune_opts));
            }
            let reference = branch.into_reference();
            let mut opts = git2::WorktreeAddOptions::new();
            opts.reference(Some(&reference));
            repo.worktree(session_id, &worktree_dir, Some(&opts))?;
            std::fs::write(worktree_dir.join(".gitignore"), GITIGNORE_CONTENT)?;
            std::fs::create_dir_all(crate::session::live::outputs_dir(gait_dir, session_id))?;
            return Ok(());
        }

        // Build an EMPTY orphan root tree.
        let empty_tree_id = repo.treebuilder(None)?.write()?;
        let tree = repo.find_tree(empty_tree_id)?;

        let sig = repo
            .signature()
            .or_else(|_| Signature::now("gait", "gait@localhost"))?;
        // Orphan commit: no parents, not a child of main's current tip. `None`
        // for the update_ref arg — this returns a dangling oid; it does not
        // move any ref (in particular it is NOT main's HEAD).
        let commit_oid = repo.commit(
            None,
            &sig,
            &sig,
            &format!("session({}): init", session_id),
            &tree,
            &[],
        )?;
        let commit = repo.find_commit(commit_oid)?;

        let branch = repo.branch(&branch_name, &commit, false)?;
        let reference = branch.into_reference();
        let mut opts = git2::WorktreeAddOptions::new();
        opts.reference(Some(&reference));
        repo.worktree(session_id, &worktree_dir, Some(&opts))?;

        // Now that the (empty) tree is checked out, write .gitignore directly
        // to disk.
        std::fs::write(worktree_dir.join(".gitignore"), GITIGNORE_CONTENT)?;
        std::fs::create_dir_all(crate::session::live::outputs_dir(gait_dir, session_id))?;

        Ok(())
    }

    fn commit_session(
        &self,
        gait_dir: &Path,
        session_id: &str,
        message: &str,
    ) -> anyhow::Result<()> {
        let session_dir = crate::session::live::worktree_dir(gait_dir, session_id);
        let repo = Repository::open(&session_dir)?;
        let sig = repo
            .signature()
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
        repo.commit(
            Some("HEAD"),
            &sig,
            &sig,
            &with_gait_trailer(message),
            &tree,
            &[&head_commit],
        )?;
        Ok(())
    }

    fn list_active_sessions(&self, gait_dir: &Path) -> anyhow::Result<Vec<String>> {
        let repo = Repository::open(gait_dir)?;
        let main_oid = repo
            .find_branch("main", BranchType::Local)
            .or_else(|_| repo.find_branch("master", BranchType::Local))?
            .get()
            .peel_to_commit()?
            .id();
        let mut sessions = vec![];
        for item in repo.branches(Some(BranchType::Local))? {
            let (branch, _) = item?;
            let name = branch.name()?.unwrap_or("").to_string();
            let Some(id) = name.strip_prefix("session/") else {
                continue;
            };
            let branch_oid = branch.get().peel_to_commit()?.id();
            // Not merged = main is NOT a descendant of branch
            let is_merged = repo
                .graph_descendant_of(main_oid, branch_oid)
                .unwrap_or(false);
            if !is_merged {
                sessions.push(id.to_string());
            }
        }
        Ok(sessions)
    }

    fn finish_session(&self, gait_dir: &Path, session_id: &str) -> anyhow::Result<()> {
        let repo = Repository::open(gait_dir)?;
        let branch_name = format!("session/{}", session_id);

        // If no such branch, nothing to do
        if repo.find_branch(&branch_name, BranchType::Local).is_err() {
            return Ok(());
        }

        // Remove the worktree directory if it exists (required before branch ops).
        // Prune worktree registration first (while dir still exists), then remove the dir.
        let session_dir = crate::session::live::worktree_dir(gait_dir, session_id);
        if session_dir.exists() {
            if let Ok(wt) = repo.find_worktree(session_dir.to_str().unwrap_or("")) {
                let mut o = git2::WorktreePruneOptions::new();
                o.valid(true);
                let _ = wt.prune(Some(&mut o));
            }
            let _ = std::fs::remove_dir_all(&session_dir);
        }

        // Merge session branch into HEAD
        let branch_ref = repo
            .find_branch(&branch_name, BranchType::Local)?
            .into_reference();
        let branch_commit = branch_ref.peel_to_commit()?;
        let annotated = repo.find_annotated_commit(branch_commit.id())?;

        let mut merge_opts = git2::MergeOptions::new();
        let mut checkout_opts = git2::build::CheckoutBuilder::new();
        repo.merge(
            &[&annotated],
            Some(&mut merge_opts),
            Some(&mut checkout_opts),
        )?;

        // Commit the merge
        let sig = repo
            .signature()
            .or_else(|_| Signature::now("gait", "gait@localhost"))?;
        let mut index = repo.index()?;
        if index.has_conflicts() {
            repo.cleanup_state()?;
            anyhow::bail!(
                "session {} has merge conflicts with main; resolve manually",
                session_id
            );
        }
        index.write()?;
        let tree_id = index.write_tree()?;
        let tree = repo.find_tree(tree_id)?;
        let head_commit = repo.head()?.peel_to_commit()?;
        repo.commit(
            Some("HEAD"),
            &sig,
            &sig,
            &with_gait_trailer(&format!("session({}): finish", session_id)),
            &tree,
            &[&head_commit, &branch_commit],
        )?;
        repo.cleanup_state()?;

        // Delete the session branch
        let mut b = repo.find_branch(&branch_name, BranchType::Local)?;
        b.delete()?;

        Ok(())
    }

    fn discard_session(&self, gait_dir: &Path, session_id: &str) -> anyhow::Result<()> {
        let repo = Repository::open(gait_dir)?;
        let branch_name = format!("session/{}", session_id);

        // Prune worktree registration (while the dir still exists), then remove
        // the dir. This wipes worktree + log.jsonl + codebase.git + lock in one
        // shot, since all of them live under session_dir.
        let session_dir = crate::session::live::worktree_dir(gait_dir, session_id);
        if session_dir.exists() {
            if let Ok(wt) = repo.find_worktree(session_dir.to_str().unwrap_or("")) {
                let mut o = git2::WorktreePruneOptions::new();
                o.valid(true);
                let _ = wt.prune(Some(&mut o));
            }
            let _ = std::fs::remove_dir_all(&session_dir);
        }

        // Delete the branch WITHOUT merging. git2's Branch::delete removes the
        // ref regardless of merge status (unlike `git branch -d`) — exactly the
        // no-merge semantics discard wants. No-op if the branch is already gone.
        if let Ok(mut b) = repo.find_branch(&branch_name, BranchType::Local) {
            b.delete()?;
        }
        Ok(())
    }

    fn create_branch_worktree(
        &self,
        gait_dir: &Path,
        base_branch: &str,
        new_branch: &str,
        worktree_path: &Path,
    ) -> anyhow::Result<()> {
        let repo = Repository::open(gait_dir)?;
        let base = repo.find_branch(base_branch, BranchType::Local)?;
        let base_commit = base.into_reference().peel_to_commit()?;
        let branch = repo.branch(new_branch, &base_commit, false)?;
        let reference = branch.into_reference();
        let mut opts = git2::WorktreeAddOptions::new();
        opts.reference(Some(&reference));
        let wt_name = new_branch.replace('/', "-");
        repo.worktree(&wt_name, worktree_path, Some(&opts))?;
        Ok(())
    }

    fn remove_branch_worktree(&self, gait_dir: &Path, worktree_path: &Path) -> anyhow::Result<()> {
        if worktree_path.exists() {
            std::fs::remove_dir_all(worktree_path)?;
        }
        let repo = Repository::open(gait_dir)?;
        let mut prune_opts = git2::WorktreePruneOptions::new();
        prune_opts.valid(true);
        let canonical = worktree_path
            .canonicalize()
            .unwrap_or_else(|_| worktree_path.to_path_buf());
        for name in repo.worktrees()?.iter().flatten() {
            if let Ok(wt) = repo.find_worktree(name) {
                let wt_canonical = wt
                    .path()
                    .canonicalize()
                    .unwrap_or_else(|_| wt.path().to_path_buf());
                if wt_canonical == canonical {
                    wt.prune(Some(&mut prune_opts))?;
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    fn merge_branch_ff(
        &self,
        gait_dir: &Path,
        target_branch: &str,
        source_branch: &str,
    ) -> anyhow::Result<()> {
        let repo = Repository::open(gait_dir)?;
        let source = repo.find_branch(source_branch, BranchType::Local)?;
        let source_oid = source.into_reference().peel_to_commit()?.id();
        let target_ref = format!("refs/heads/{}", target_branch);
        repo.reference(&target_ref, source_oid, true, "workflow ff-merge")?;
        Ok(())
    }
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
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
            .unwrap();
        drop(tree);
        // Rename branch to main
        let _ = repo
            .find_branch("master", git2::BranchType::Local)
            .or_else(|_| repo.find_branch("main", git2::BranchType::Local))
            .and_then(|mut b| {
                b.rename("main", false)?;
                Ok(())
            });
        repo
    }

    /// Like `init_repo`, but also commits a root `.gitignore` matching what
    /// `gait init` actually produces (`crates/gait-cli/src/cli/init.rs`).
    /// The plain `init_repo` commits an empty tree, which can never exercise
    /// the add/add collision this test guards against — a regression test
    /// built on `init_repo` alone would pass even if the fix regressed.
    fn init_repo_like_gait_init(dir: &Path) -> git2::Repository {
        let repo = init_repo(dir);
        std::fs::write(dir.join(".gitignore"), ".active/\nUSER.md\n.gait-ws.lock\n").unwrap();
        let sig = git2::Signature::now("test", "test@test.com").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new(".gitignore")).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let parent = repo.head().unwrap().peel_to_commit().unwrap();
        repo.commit(
            Some("HEAD"),
            &sig,
            &sig,
            "add root .gitignore",
            &tree,
            &[&parent],
        )
        .unwrap();
        drop(tree);
        drop(parent);
        repo
    }

    #[test]
    fn two_sessions_persist_cleanly_in_either_order() {
        for reverse in [false, true] {
            let ws = TempDir::new().unwrap();
            init_repo_like_gait_init(ws.path());
            let git_ws = GitTraceRepo;

            git_ws
                .create_session_worktree(ws.path(), "sA00001")
                .unwrap();
            git_ws
                .create_session_worktree(ws.path(), "sB00002")
                .unwrap();

            let log_a = ws.path().join(".active/sA00001/sessions/sA00001/log.jsonl");
            std::fs::create_dir_all(log_a.parent().unwrap()).unwrap();
            std::fs::write(&log_a, "{\"session\":\"A\"}\n").unwrap();
            git_ws
                .commit_session(ws.path(), "sA00001", "session(sA00001): turn 1")
                .unwrap();

            let log_b = ws.path().join(".active/sB00002/sessions/sB00002/log.jsonl");
            std::fs::create_dir_all(log_b.parent().unwrap()).unwrap();
            std::fs::write(&log_b, "{\"session\":\"B\"}\n").unwrap();
            git_ws
                .commit_session(ws.path(), "sB00002", "session(sB00002): turn 1")
                .unwrap();

            let order: [&str; 2] = if reverse {
                ["sB00002", "sA00001"]
            } else {
                ["sA00001", "sB00002"]
            };
            for id in order {
                git_ws.finish_session(ws.path(), id).unwrap_or_else(|e| {
                    panic!("finish_session({id}) failed (reverse={reverse}): {e}")
                });
            }

            let repo = git2::Repository::open(ws.path()).unwrap();
            let head_tree = repo.head().unwrap().peel_to_tree().unwrap();
            let sessions_tree = head_tree
                .get_path(Path::new("sessions"))
                .unwrap()
                .to_object(&repo)
                .unwrap()
                .peel_to_tree()
                .unwrap();

            for (id, expected) in [("sA00001", "A"), ("sB00002", "B")] {
                let entry = sessions_tree
                    .get_path(Path::new(&format!("{id}/log.jsonl")))
                    .unwrap_or_else(|_| {
                        panic!("sessions/{id}/log.jsonl missing on main (reverse={reverse})")
                    });
                let blob = entry.to_object(&repo).unwrap().peel_to_blob().unwrap();
                let content = std::str::from_utf8(blob.content()).unwrap();
                assert!(
                    content.contains(expected),
                    "sessions/{id}/log.jsonl should contain {expected:?}, got {content:?} (reverse={reverse})"
                );
            }

            // main's own root .gitignore must be untouched — proves the two
            // trees never collided on it.
            let gi_entry = head_tree.get_path(Path::new(".gitignore")).unwrap();
            let gi_blob = gi_entry.to_object(&repo).unwrap().peel_to_blob().unwrap();
            assert_eq!(
                std::str::from_utf8(gi_blob.content()).unwrap(),
                ".active/\nUSER.md\n.gait-ws.lock\n",
                "main's root .gitignore must survive persist unchanged (reverse={reverse})"
            );
        }
    }

    #[test]
    fn create_session_worktree_root_commit_is_orphan_and_empty() {
        let ws = TempDir::new().unwrap();
        init_repo_like_gait_init(ws.path());
        let git_ws = GitTraceRepo;
        git_ws
            .create_session_worktree(ws.path(), "sorph01")
            .unwrap();

        let wt_repo = git2::Repository::open(ws.path().join(".active/sorph01")).unwrap();
        let head = wt_repo.head().unwrap().peel_to_commit().unwrap();
        assert_eq!(
            head.parent_count(),
            0,
            "session's init commit must be an orphan"
        );

        let tree = head.tree().unwrap();
        assert_eq!(
            tree.iter().count(),
            0,
            "session's orphan tree must be EMPTY — .gitignore lives on disk, untracked, \
             never committed (committing it would collide with main's own, different, \
             root .gitignore on persist — see design spec §2/§3/§5)"
        );

        // .gitignore must still exist on disk (untracked) so commit_session's
        // add_all respects it.
        let gi = std::fs::read_to_string(ws.path().join(".active/sorph01/.gitignore")).unwrap();
        assert_eq!(
            gi,
            "*\n!sessions/\n!sessions/*/\n!sessions/*/log.jsonl\n!sessions/*/diagnostic.jsonl\n"
        );
        // .gitignore must be ignored (by itself), not merely untracked, so
        // commit_session's add_all(".") won't accidentally stage it. Use
        // `status_should_ignore` (the libgit2 equivalent of `git check-ignore`)
        // directly — `repo.statuses()` only surfaces ignored files in its
        // enumeration under a specific StatusOptions combination for untracked
        // single files, and is the wrong tool for "is this one path ignored?".
        // `.expect` instead of `?` because this is a #[test] returning ().
        assert!(
            wt_repo
                .status_should_ignore(Path::new(".gitignore"))
                .expect("status_should_ignore failed"),
            ".gitignore must be ignored by itself"
        );
        let outputs = crate::session::live::outputs_dir(ws.path(), "sorph01");
        assert!(outputs.is_dir(), "session output root must be provisioned");
        std::fs::write(outputs.join("marker.txt"), "runtime-only").unwrap();
        assert!(
            wt_repo
                .status_should_ignore(Path::new("outputs/marker.txt"))
                .expect("status_should_ignore failed"),
            "session outputs must remain outside persisted Git history"
        );
    }

    #[test]
    fn create_session_worktree_reattach_still_writes_gitignore() {
        let ws = TempDir::new().unwrap();
        init_repo_like_gait_init(ws.path());
        let git_ws = GitTraceRepo;
        git_ws
            .create_session_worktree(ws.path(), "sreatt1")
            .unwrap();

        // Simulate the worktree directory being cleaned up while the branch survives.
        std::fs::remove_dir_all(ws.path().join(".active/sreatt1")).unwrap();

        // Re-attach.
        git_ws
            .create_session_worktree(ws.path(), "sreatt1")
            .unwrap();

        let gi = std::fs::read_to_string(ws.path().join(".active/sreatt1/.gitignore")).unwrap();
        assert_eq!(
            gi,
            "*\n!sessions/\n!sessions/*/\n!sessions/*/log.jsonl\n!sessions/*/diagnostic.jsonl\n",
            ".gitignore must be re-written on re-attach, not left absent"
        );
        assert!(
            crate::session::live::outputs_dir(ws.path(), "sreatt1").is_dir(),
            "output root must be recreated on re-attach"
        );
    }

    #[test]
    fn create_session_worktree_creates_dir_and_branch() {
        let ws = TempDir::new().unwrap();
        init_repo(ws.path());
        let git_ws = GitTraceRepo;
        git_ws
            .create_session_worktree(ws.path(), "sabc1234")
            .unwrap();
        assert!(ws.path().join(".active/sabc1234").exists());
        assert!(ws.path().join(".active/sabc1234/.git").exists());
    }

    #[test]
    fn create_session_worktree_idempotent() {
        let ws = TempDir::new().unwrap();
        init_repo(ws.path());
        let git_ws = GitTraceRepo;
        git_ws
            .create_session_worktree(ws.path(), "sabc1234")
            .unwrap();
        git_ws
            .create_session_worktree(ws.path(), "sabc1234")
            .unwrap(); // no error
    }

    #[test]
    fn commit_session_creates_commit() {
        let ws = TempDir::new().unwrap();
        init_repo(ws.path());
        let git_ws = GitTraceRepo;
        git_ws
            .create_session_worktree(ws.path(), "sabc1234")
            .unwrap();
        let tx = crate::session::live::log_path(ws.path(), "sabc1234");
        std::fs::create_dir_all(tx.parent().unwrap()).unwrap();
        std::fs::write(&tx, "{\"type\":\"session_start\"}\n").unwrap();
        git_ws
            .commit_session(ws.path(), "sabc1234", "session(sabc1234): turn 1")
            .unwrap();
        // Verify: repo in session dir has 2+ commits (init + our commit)
        let repo = git2::Repository::open(ws.path().join(".active/sabc1234")).unwrap();
        let head = repo.head().unwrap().peel_to_commit().unwrap();
        assert_eq!(
            head.message().unwrap(),
            "session(sabc1234): turn 1\n\nCo-Authored-By: gait <gait@localhost>"
        );
    }

    #[test]
    fn commit_session_never_tracks_runtime_outputs() {
        let ws = TempDir::new().unwrap();
        init_repo(ws.path());
        let git_ws = GitTraceRepo;
        git_ws
            .create_session_worktree(ws.path(), "soutput1")
            .unwrap();

        let tx = crate::session::live::log_path(ws.path(), "soutput1");
        std::fs::create_dir_all(tx.parent().unwrap()).unwrap();
        std::fs::write(&tx, "{\"type\":\"session_start\"}\n").unwrap();
        let output =
            crate::session::live::outputs_dir(ws.path(), "soutput1").join("shell/full.txt");
        std::fs::create_dir_all(output.parent().unwrap()).unwrap();
        std::fs::write(&output, "must not persist").unwrap();

        git_ws
            .commit_session(ws.path(), "soutput1", "session(soutput1): turn 1")
            .unwrap();

        let repo =
            git2::Repository::open(crate::session::live::worktree_dir(ws.path(), "soutput1"))
                .unwrap();
        let tree = repo.head().unwrap().peel_to_tree().unwrap();
        assert!(
            tree.get_path(Path::new("outputs")).is_err(),
            "runtime outputs must not be present in the session commit"
        );
        assert_eq!(
            std::fs::read_to_string(output).unwrap(),
            "must not persist",
            "committing a turn must not delete live outputs"
        );
    }

    #[test]
    fn list_active_sessions_returns_unmerged() {
        let ws = TempDir::new().unwrap();
        init_repo(ws.path());
        let git_ws = GitTraceRepo;
        git_ws
            .create_session_worktree(ws.path(), "sabc1234")
            .unwrap();
        let sessions = git_ws.list_active_sessions(ws.path()).unwrap();
        assert!(
            sessions.contains(&"sabc1234".to_string()),
            "got: {:?}",
            sessions
        );
    }

    #[test]
    fn finish_session_merges_into_main() {
        let ws = TempDir::new().unwrap();
        init_repo(ws.path());
        let git_ws = GitTraceRepo;
        git_ws
            .create_session_worktree(ws.path(), "sfin001")
            .unwrap();
        // Write a file under the nested sessions/<id>/ path so there's
        // something tracked (and thus mergeable). The root-level result.txt
        // would be silently ignored by the worktree's allow-list .gitignore.
        let tx = crate::session::live::log_path(ws.path(), "sfin001");
        std::fs::create_dir_all(tx.parent().unwrap()).unwrap();
        std::fs::write(&tx, "{\"session\":\"fin001\"}\n").unwrap();
        git_ws
            .commit_session(ws.path(), "sfin001", "session(sfin001): turn 1")
            .unwrap();
        git_ws.finish_session(ws.path(), "sfin001").unwrap();
        // After finish: session branch should be merged into main
        let repo = git2::Repository::open(ws.path()).unwrap();
        let sessions = git_ws.list_active_sessions(ws.path()).unwrap();
        assert!(
            !sessions.contains(&"sfin001".to_string()),
            "should be merged: {:?}",
            sessions
        );
        // The merge commit should mention the session id
        let msg = repo
            .head()
            .unwrap()
            .peel_to_commit()
            .unwrap()
            .message()
            .unwrap()
            .to_string();
        assert!(
            msg.contains("sfin001"),
            "expected merge commit msg to contain session id, got: {msg}"
        );
    }

    #[test]
    fn init_workspace_creates_git_repo() {
        let dir = TempDir::new().unwrap();
        let git_ws = GitTraceRepo;
        git_ws.init_workspace(dir.path()).unwrap();
        assert!(dir.path().join(".git").exists());
        // Idempotent — calling again does not error
        git_ws.init_workspace(dir.path()).unwrap();
    }

    #[test]
    fn finish_session_noop_when_not_found() {
        let ws = TempDir::new().unwrap();
        init_repo(ws.path());
        let git_ws = GitTraceRepo;
        // Should not error for a non-existent session
        git_ws.finish_session(ws.path(), "snonexistent").unwrap();
    }

    #[test]
    fn discard_session_removes_dir_and_branch_without_merge() {
        let ws = TempDir::new().unwrap();
        init_repo(ws.path());
        let git_ws = GitTraceRepo;
        git_ws
            .create_session_worktree(ws.path(), "sdisc01")
            .unwrap();
        let tx = crate::session::live::log_path(ws.path(), "sdisc01");
        std::fs::create_dir_all(tx.parent().unwrap()).unwrap();
        std::fs::write(&tx, "{\"session\":\"disc01\"}\n").unwrap();
        git_ws
            .commit_session(ws.path(), "sdisc01", "session(sdisc01): turn 1")
            .unwrap();

        // Record main HEAD before discard.
        let repo = git2::Repository::open(ws.path()).unwrap();
        let head_before = repo.head().unwrap().peel_to_commit().unwrap().id();

        git_ws.discard_session(ws.path(), "sdisc01").unwrap();

        // Dir gone.
        assert!(!ws.path().join(".active/sdisc01").exists());
        // Branch gone (not in active list, and find_branch fails).
        assert!(repo
            .find_branch("session/sdisc01", git2::BranchType::Local)
            .is_err());
        // main HEAD unchanged — no merge commit created.
        let head_after = repo.head().unwrap().peel_to_commit().unwrap().id();
        assert_eq!(head_before, head_after, "discard must not touch main");
    }

    #[test]
    fn discard_session_noop_when_not_found() {
        let ws = TempDir::new().unwrap();
        init_repo(ws.path());
        let git_ws = GitTraceRepo;
        git_ws.discard_session(ws.path(), "snope").unwrap(); // must not error
    }

    #[test]
    fn create_and_remove_branch_worktree_works() {
        let ws = TempDir::new().unwrap();
        let repo = Repository::init(ws.path()).unwrap();
        let sig = git2::Signature::now("t", "t@t.com").unwrap();
        let tree_id = repo.index().unwrap().write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
            .unwrap();
        // Rename to main
        repo.find_branch("master", BranchType::Local)
            .and_then(|mut b| {
                b.rename("main", false)?;
                Ok(())
            })
            .ok();

        let git_ws = GitTraceRepo;
        let wt_path = ws.path().join("branches").join("feat");
        // Create the parent directory — git2 worktree creates only the final component
        std::fs::create_dir_all(wt_path.parent().unwrap()).unwrap();
        git_ws
            .create_branch_worktree(ws.path(), "main", "branch/feat", &wt_path)
            .unwrap();
        assert!(wt_path.join(".git").exists());

        git_ws.remove_branch_worktree(ws.path(), &wt_path).unwrap();
        assert!(!wt_path.join(".git").exists());
    }
}

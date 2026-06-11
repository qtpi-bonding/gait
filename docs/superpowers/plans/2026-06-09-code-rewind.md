---
status: done
---

# Code Rewind Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add per-turn rewind of codebase file state via a disposable workspace-side "shadow" git database (`GIT_DIR` in the workspace, `GIT_WORK_TREE` = the codebase), so a user can roll the codebase back to any past turn without ever writing to the codebase's real `.git`.

**Architecture:** Restructure today's `git.rs` bag-of-functions into a `git/` module with three traits, each owning exactly one git database: `WorkspaceRepo` (workspace `.git`), `CodebaseRepo` (read-only, user's repo), `RewindRepo` (the disposable shadow db). Big-bang migration — the free functions are removed and all ~20 callers move to the trait methods (compiler-guided, safe). The shadow db borrows the codebase's object store via `objects/info/alternates` so creation is cheap. Each completed turn commits a snapshot into the shadow and records the SHA in the `TurnComplete` event; `gait rewind` resets to it.

**Tech Stack:** Rust, `git2` crate, `tokio` (sync git ops are fine — gait's git layer is synchronous), `anyhow`.

**Spec:** `docs/superpowers/specs/2026-06-09-code-rewind-design.md`

---

## File Structure

| File | Responsibility |
|---|---|
| `src/git/mod.rs` | Module root: re-exports, shared helpers (`with_gait_trailer`, `signature_or_default`, `rewind_git_dir`, strict open). Replaces `src/git.rs`. |
| `src/git/codebase.rs` | `CodebaseRepo` trait + `GitCodebaseRepo` — **read-only** access to the user's repo (HEAD hash, dirty files). |
| `src/git/workspace.rs` | `WorkspaceRepo` trait + `GitWorkspaceRepo` — workspace `.git`: session worktrees, transcript commits, init, finish. |
| `src/git/rewind.rs` | `RewindRepo` trait + `GitRewindRepo` + `MockRewindRepo` — the shadow db: init/checkpoint/rewind_to/remove. |
| `src/session/transcript.rs` | `TurnComplete` gains `code_checkpoint: Option<String>` (internal rewind index). |
| `src/turn.rs` | Wire `RewindRepo::init` (session start) + `checkpoint` (turn end) via a single helper. |
| `src/bin/gait.rs` | New `gait rewind` subcommand; wire `RewindRepo::remove` into `gait finish`. |

**Migration note (big-bang):** `src/git.rs` currently exposes free functions called from `turn.rs`, `harness.rs`, `tools/spawn_agent.rs`, `workflow/runner.rs`, `bin/gait.rs`, `tools/list_agents.rs`. Tasks 2–3 replace every one of these with a trait-method call. The unit structs (`GitWorkspaceRepo`, `GitCodebaseRepo`, `GitRewindRepo`) are zero-sized — construct inline at call sites, e.g. `GitWorkspaceRepo.commit_session(ws, id, msg)`.

---

## Task 1: Convert `git.rs` into a `git/` module with shared helpers

**Files:**
- Create: `src/git/mod.rs` (moved from `src/git.rs`)
- Delete: `src/git.rs`

This task is a pure mechanical move plus adding shared helpers. No behavior change. All existing free functions stay `pub` for now (Tasks 2–3 convert them to traits).

- [ ] **Step 1: Move the file**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs
mkdir -p src/git
git mv src/git.rs src/git/mod.rs
```

- [ ] **Step 2: Add shared helpers to `src/git/mod.rs`**

At the top of `src/git/mod.rs`, below the existing `use` lines, add the submodule declarations and shared helpers. Keep the existing `with_gait_trailer` but make it `pub(crate)`:

```rust
pub mod codebase;
pub mod workspace;
pub mod rewind;

use std::path::{Path, PathBuf};

pub(crate) fn with_gait_trailer(message: &str) -> String {
    format!("{}\n\nCo-Authored-By: gait <gait@localhost>", message)
}

/// Signature for gait-authored commits, falling back to a fixed identity
/// when the repo/user has none configured.
pub(crate) fn signature_or_default(repo: &git2::Repository) -> anyhow::Result<git2::Signature<'static>> {
    match repo.signature() {
        Ok(sig) => Ok(sig.to_owned()),
        Err(_) => Ok(git2::Signature::now("gait", "gait@localhost")?),
    }
}

/// On-disk location of a session's shadow (rewind) git database.
/// Lives inside the session worktree but is gitignored from the transcript.
pub(crate) fn rewind_git_dir(workspace: &Path, session_id: &str) -> PathBuf {
    workspace.join("sessions").join(session_id).join("codebase.git")
}
```

Delete the now-duplicated `fn with_gait_trailer` that previously lived in the file body (it is replaced by the `pub(crate)` version above — there must be exactly one definition).

- [ ] **Step 3: Build to verify the move compiles**

Run: `cargo build 2>&1 | tail -20`
Expected: compiles. The new empty submodules (`codebase`, `workspace`, `rewind`) do not exist yet — so this step will FAIL with "file not found for module". Create empty stubs to satisfy the compiler:

```bash
printf '' > src/git/codebase.rs
printf '' > src/git/workspace.rs
printf '' > src/git/rewind.rs
```

- [ ] **Step 4: Build again**

Run: `cargo build 2>&1 | tail -20`
Expected: compiles cleanly (empty modules are valid).

- [ ] **Step 5: Run tests**

Run: `cargo test 2>&1 | tail -15`
Expected: same pass count as before the move (188), 0 failures.

- [ ] **Step 6: Commit**

```bash
git add src/git/
git commit -m "refactor: split git.rs into git/ module with shared helpers"
```

---

## Task 2: `CodebaseRepo` trait (read-only) + migrate callers

**Files:**
- Modify: `src/git/codebase.rs`
- Modify: `src/git/mod.rs` (remove `get_codebase_hash`, `get_dirty_files`)
- Modify callers: `src/turn.rs`, `src/tools/spawn_agent.rs`

The read-only surface. By contract it has **no write methods** — this is the type-level guarantee that gait never writes the user's real git.

- [ ] **Step 1: Write the trait + impl in `src/git/codebase.rs`**

Move the bodies of `get_codebase_hash` and `get_dirty_files` verbatim out of `mod.rs` into trait methods here:

```rust
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
```

- [ ] **Step 2: Remove the old free functions from `src/git/mod.rs`**

Delete `pub fn get_codebase_hash(...)` and `pub fn get_dirty_files(...)` (and `pub fn isCodebaseDirty`/`is_codebase_dirty` if present) from `src/git/mod.rs`. Their logic now lives in `codebase.rs`.

- [ ] **Step 3: Migrate callers in `src/turn.rs`**

At `src/turn.rs:364-366`, replace:

```rust
    let codebase_hash = git::get_codebase_hash(&opts.codebase_path);
    let dirty_files = git::get_dirty_files(&opts.codebase_path);
    let codebase_dirty = !dirty_files.is_empty();
```

with:

```rust
    use crate::git::codebase::{CodebaseRepo, GitCodebaseRepo};
    let codebase_hash = GitCodebaseRepo.head_hash(&opts.codebase_path);
    let dirty_files = GitCodebaseRepo.dirty_files(&opts.codebase_path);
    let codebase_dirty = !dirty_files.is_empty();
```

- [ ] **Step 4: Migrate the caller in `src/tools/spawn_agent.rs`**

At `src/tools/spawn_agent.rs:67`, replace:

```rust
        let codebase_hash = git::get_codebase_hash(&self.codebase_path);
```

with:

```rust
        let codebase_hash = crate::git::codebase::GitCodebaseRepo
            .head_hash(&self.codebase_path);
```

(`use crate::git::codebase::CodebaseRepo;` at the top of the file so the method resolves.)

- [ ] **Step 5: Build**

Run: `cargo build 2>&1 | tail -20`
Expected: compiles. If any other site referenced `git::get_codebase_hash`/`get_dirty_files`, the compiler names it — fix it the same way.

- [ ] **Step 6: Test**

Run: `cargo test 2>&1 | tail -15`
Expected: 188+3 (new codebase tests) pass, 0 fail.

- [ ] **Step 7: Commit**

```bash
git add src/git/codebase.rs src/git/mod.rs src/turn.rs src/tools/spawn_agent.rs
git commit -m "refactor: CodebaseRepo trait (read-only) replaces codebase git free fns"
```

---

## Task 3: `WorkspaceRepo` trait + migrate all callers

**Files:**
- Modify: `src/git/workspace.rs`
- Modify: `src/git/mod.rs` (remove the migrated free functions)
- Modify callers: `src/turn.rs`, `src/harness.rs`, `src/tools/spawn_agent.rs`, `src/workflow/runner.rs`, `src/bin/gait.rs`, `src/tools/list_agents.rs`

Owns the workspace `.git`. The real `git.rs` has exactly these 8 workspace functions (verified — there is **no** `remove_session_worktree`): `init_workspace`, `create_session_worktree`, `commit_session`, `list_active_sessions`, `finish_session`, `create_branch_worktree`, `remove_branch_worktree`, `merge_branch_ff`.

- [ ] **Step 1: Define the trait + impl in `src/git/workspace.rs`**

Move the bodies of those 8 functions verbatim from `mod.rs` into trait methods. Use the shared `with_gait_trailer` / `signature_or_default` from `mod.rs` (`use super::{with_gait_trailer, signature_or_default};`). One behavior change: in `create_session_worktree`, the initial `.gitignore` must also exclude the shadow db so it is never committed into transcript history:

```rust
use std::path::Path;
use git2::{Repository, BranchType, IndexAddOption};
use super::{with_gait_trailer, signature_or_default};

pub trait WorkspaceRepo: Send + Sync {
    fn init_workspace(&self, workspace: &Path) -> anyhow::Result<()>;
    fn create_session_worktree(&self, workspace: &Path, session_id: &str) -> anyhow::Result<()>;
    fn commit_session(&self, workspace: &Path, session_id: &str, message: &str) -> anyhow::Result<()>;
    fn list_active_sessions(&self, workspace: &Path) -> anyhow::Result<Vec<String>>;
    fn finish_session(&self, workspace: &Path, session_id: &str) -> anyhow::Result<()>;
    fn create_branch_worktree(&self, workspace: &Path, base_branch: &str, new_branch: &str, worktree_path: &Path) -> anyhow::Result<()>;
    fn remove_branch_worktree(&self, workspace: &Path, worktree_path: &Path) -> anyhow::Result<()>;
    fn merge_branch_ff(&self, workspace: &Path, target_branch: &str, source_branch: &str) -> anyhow::Result<()>;
}

pub struct GitWorkspaceRepo;

impl WorkspaceRepo for GitWorkspaceRepo {
    // PASTE the existing bodies from git/mod.rs here, renamed as methods
    // (replace `pub fn name(args)` with `fn name(&self, args)`), EXCEPT
    // for the one change in create_session_worktree noted below.
}
```

In the migrated `create_session_worktree`, change the `.gitignore` write so it reads:

```rust
    std::fs::write(session_dir.join(".gitignore"), "lock\ncodebase.git/\n")?;
```

(Search the original body for the line writing `"lock\n"` and replace it with the two-line form above.)

- [ ] **Step 2: Remove the migrated free functions from `src/git/mod.rs`**

Delete all 8: `init_workspace`, `create_session_worktree`, `commit_session`, `list_active_sessions`, `finish_session`, `create_branch_worktree`, `remove_branch_worktree`, `merge_branch_ff` from `mod.rs`. Move the `#[cfg(test)]` tests in `mod.rs` that exercise these (e.g. the `list_active_sessions` tests at git.rs:328,343) into `workspace.rs`'s test module, converting calls to `GitWorkspaceRepo.method(...)` form. After this step, `mod.rs` contains only the shared helpers + submodule declarations.

- [ ] **Step 3: Migrate production callers**

Replace each call. The argument lists are unchanged — only the prefix changes from `git::fn(` to `GitWorkspaceRepo.fn(`, with `use crate::git::workspace::{WorkspaceRepo, GitWorkspaceRepo};` added to each file's imports.

| Site | Old | New |
|---|---|---|
| `src/turn.rs:357` | `git::create_session_worktree(&opts.workspace, &opts.session_id)?;` | `GitWorkspaceRepo.create_session_worktree(&opts.workspace, &opts.session_id)?;` |
| `src/turn.rs:647,969,1036,1044,1050` | `git::commit_session(&opts.workspace, &opts.session_id, …)?;` | `GitWorkspaceRepo.commit_session(&opts.workspace, &opts.session_id, …)?;` |
| `src/harness.rs:90` | `git::commit_session(…)` | `GitWorkspaceRepo.commit_session(…)` |
| `src/tools/spawn_agent.rs:53,85` | `git::create_session_worktree(…)` / `git::commit_session(…)` | `GitWorkspaceRepo.create_session_worktree(…)` / `.commit_session(…)` |
| `src/workflow/runner.rs:31,57,94,152,197,217,222` | `git::create_session_worktree(…)` / `git::commit_session(…)` | `GitWorkspaceRepo.create_session_worktree(…)` / `.commit_session(…)` |
| `src/bin/gait.rs:390` | `git::init_workspace(&workspace)` | `GitWorkspaceRepo.init_workspace(&workspace)` |
| `src/bin/gait.rs:487` | `git::finish_session(&workspace, &session_id)` | `GitWorkspaceRepo.finish_session(&workspace, &session_id)` |
| `src/bin/gait.rs:501` | `git::list_active_sessions(&workspace)` | `GitWorkspaceRepo.list_active_sessions(&workspace)` |
| `src/bin/gait.rs:634` | `gait_ai::git::list_active_sessions(&workspace)` | `gait_ai::git::workspace::GitWorkspaceRepo.list_active_sessions(&workspace)` (trait in scope) |
| `src/bin/gait.rs:710` | `gait_ai::git::commit_session(…)` | `gait_ai::git::workspace::GitWorkspaceRepo.commit_session(…)` (with the trait in scope) |

(If `merge_branch_ff` has any caller the compiler flags, migrate it the same way: `GitWorkspaceRepo.merge_branch_ff(…)`.)

- [ ] **Step 4: Migrate test callers**

In `src/turn.rs` (lines ~1719, 1802, 1904) and `src/tools/list_agents.rs` (lines ~164, 169, 171, 180), replace `crate::git::create_session_worktree(...)` / `crate::git::commit_session(...)` with `crate::git::workspace::GitWorkspaceRepo.create_session_worktree(...)` / `.commit_session(...)` and add `use crate::git::workspace::WorkspaceRepo;` to the test module.

- [ ] **Step 5: Build**

Run: `cargo build 2>&1 | tail -30`
Expected: compiles. The compiler enumerates any remaining `git::` free-fn references — fix each by the same mechanical substitution.

- [ ] **Step 6: Test**

Run: `cargo test 2>&1 | tail -15`
Expected: all pass (188+ codebase/workspace tests), 0 fail.

- [ ] **Step 7: Commit**

```bash
git add src/git/ src/turn.rs src/harness.rs src/tools/ src/workflow/runner.rs src/bin/gait.rs
git commit -m "refactor: WorkspaceRepo trait replaces workspace git free fns; gitignore codebase.git"
```

---

## Task 4: `RewindRepo` trait + `GitRewindRepo::init`

**Files:**
- Modify: `src/git/rewind.rs`
- Test: `src/git/rewind.rs` (`#[cfg(test)]`)

`init` creates the shadow db borrowing the codebase's objects via alternates. Idempotent; a no-op (logged) when the codebase is not a git repo.

- [ ] **Step 1: Write the failing test in `src/git/rewind.rs`**

```rust
use std::path::Path;
use git2::{Repository, IndexAddOption, ResetType, Oid, Status, StatusOptions};
use super::{signature_or_default, rewind_git_dir};

pub trait RewindRepo: Send + Sync {
    fn init(&self, workspace: &Path, session_id: &str, codebase: &Path) -> anyhow::Result<()>;
    fn checkpoint(&self, workspace: &Path, session_id: &str, codebase: &Path, turn: u32) -> anyhow::Result<String>;
    fn rewind_to(&self, workspace: &Path, session_id: &str, codebase: &Path, sha: &str) -> anyhow::Result<()>;
    fn remove(&self, workspace: &Path, session_id: &str) -> anyhow::Result<()>;
}

pub struct GitRewindRepo;

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
```

- [ ] **Step 2: Run to confirm failure**

Run: `cargo test --lib git::rewind 2>&1 | tail -15`
Expected: FAIL — `init`/`checkpoint`/`rewind_to`/`remove` are not implemented for `GitRewindRepo`.

- [ ] **Step 3: Implement `init` (and stub the other three methods so the impl compiles)**

Add below the trait/struct definitions in `src/git/rewind.rs`:

```rust
impl RewindRepo for GitRewindRepo {
    fn init(&self, workspace: &Path, session_id: &str, codebase: &Path) -> anyhow::Result<()> {
        // No-op if the codebase is not a git repo — rewind simply unavailable.
        let cb = match Repository::open(codebase) {
            Ok(r) => r,
            Err(_) => {
                tracing::info!(codebase = %codebase.display(), "rewind: codebase is not a git repo; rewind unavailable");
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
        // first checkpoint is a child of the real HEAD. Empty/unborn repo → leave
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
```

- [ ] **Step 4: Run the init tests**

Run: `cargo test --lib git::rewind::tests::init 2>&1 | tail -15`
Expected: the four `init_*` tests PASS.

- [ ] **Step 5: Commit**

```bash
git add src/git/rewind.rs
git commit -m "feat(rewind): RewindRepo trait + GitRewindRepo::init (shadow db via alternates)"
```

---

## Task 5: `GitRewindRepo::checkpoint`

**Files:**
- Modify: `src/git/rewind.rs`

Stage the codebase working tree into the shadow and commit. No-op (returns current HEAD) when nothing changed. Captures shell-made changes — the property Claude Code's snapshot store lacks.

- [ ] **Step 1: Write the failing tests (append to the `tests` module)**

```rust
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
        assert_eq!(first, second, "no change → no new commit");
    }

    #[test]
    fn checkpoint_captures_shell_style_changes_and_ignores_dotgit() {
        let (ws, cb, id) = fixture();
        GitRewindRepo.init(ws.path(), &id, cb.path()).unwrap();
        // Simulate a shell-created file and a shell-deleted file.
        std::fs::write(cb.path().join("created.txt"), "x\n").unwrap();
        std::fs::remove_file(cb.path().join("a.txt")).unwrap();
        let sha = GitRewindRepo.checkpoint(ws.path(), &id, cb.path(), 1).unwrap();

        // Inspect the shadow commit's tree: created.txt present, a.txt gone,
        // and the codebase's own .git not tracked.
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
        // Non-git codebase → init was a no-op → checkpoint returns empty string.
        let ws = TempDir::new().unwrap();
        let id = "snogit2".to_string();
        std::fs::create_dir_all(ws.path().join("sessions").join(&id)).unwrap();
        let cb = TempDir::new().unwrap();
        GitRewindRepo.init(ws.path(), &id, cb.path()).unwrap();
        let sha = GitRewindRepo.checkpoint(ws.path(), &id, cb.path(), 1).unwrap();
        assert!(sha.is_empty());
    }
```

- [ ] **Step 2: Run to confirm failure**

Run: `cargo test --lib git::rewind::tests::checkpoint 2>&1 | tail -15`
Expected: FAIL with `unimplemented!("Task 5")`.

- [ ] **Step 3: Implement `checkpoint` (replace the Task-5 stub)**

```rust
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
```

- [ ] **Step 4: Run the checkpoint tests**

Run: `cargo test --lib git::rewind::tests::checkpoint 2>&1 | tail -15`
Expected: all four `checkpoint_*` tests PASS.

- [ ] **Step 5: Commit**

```bash
git add src/git/rewind.rs
git commit -m "feat(rewind): GitRewindRepo::checkpoint — snapshot codebase working tree"
```

---

## Task 6: `GitRewindRepo::rewind_to`

**Files:**
- Modify: `src/git/rewind.rs`

Hard-reset the codebase working tree to a checkpoint SHA, and remove files created after it (reset --hard alone leaves untracked files behind).

- [ ] **Step 1: Write the failing tests (append to the `tests` module)**

```rust
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
```

- [ ] **Step 2: Run to confirm failure**

Run: `cargo test --lib git::rewind::tests::rewind_to 2>&1 | tail -15`
Expected: FAIL with `unimplemented!("Task 6")`.

- [ ] **Step 3: Implement `rewind_to` (replace the Task-6 stub)**

```rust
    fn rewind_to(&self, workspace: &Path, session_id: &str, codebase: &Path, sha: &str) -> anyhow::Result<()> {
        let git_dir = rewind_git_dir(workspace, session_id);
        let shadow = Repository::open_bare(&git_dir)?;
        shadow.set_workdir(codebase, false)?;

        let oid = Oid::from_str(sha)?;
        let obj = shadow.find_object(oid, None)?;
        shadow.reset(&obj, ResetType::Hard, None)?;

        // reset --hard does not remove untracked files. Delete anything the
        // working tree now reports as new so the codebase matches the snapshot
        // exactly (mirrors archebuild's GitOps::reset).
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
```

- [ ] **Step 4: Run the rewind_to tests**

Run: `cargo test --lib git::rewind::tests::rewind_to 2>&1 | tail -15`
Expected: all three `rewind_to_*` tests PASS.

- [ ] **Step 5: Commit**

```bash
git add src/git/rewind.rs
git commit -m "feat(rewind): GitRewindRepo::rewind_to — reset codebase to a checkpoint"
```

---

## Task 7: `GitRewindRepo::remove`

**Files:**
- Modify: `src/git/rewind.rs`

Delete the shadow db — it is disposable scratch.

- [ ] **Step 1: Write the failing tests (append to the `tests` module)**

```rust
    #[test]
    fn remove_deletes_shadow_db() {
        let (ws, cb, id) = fixture();
        GitRewindRepo.init(ws.path(), &id, cb.path()).unwrap();
        assert!(rewind_git_dir(ws.path(), &id).exists());
        GitRewindRepo.remove(ws.path(), &id).unwrap();
        assert!(!rewind_git_dir(ws.path(), &id).exists());
    }

    #[test]
    fn remove_leaves_codebase_intact() {
        let (ws, cb, id) = fixture();
        GitRewindRepo.init(ws.path(), &id, cb.path()).unwrap();
        GitRewindRepo.remove(ws.path(), &id).unwrap();
        assert!(cb.path().join("a.txt").exists(), "codebase files must remain");
    }

    #[test]
    fn remove_safe_when_absent() {
        let ws = TempDir::new().unwrap();
        GitRewindRepo.remove(ws.path(), "never-existed").unwrap(); // must not error
    }
```

- [ ] **Step 2: Run to confirm failure**

Run: `cargo test --lib git::rewind::tests::remove 2>&1 | tail -15`
Expected: FAIL with `unimplemented!("Task 7")`.

- [ ] **Step 3: Implement `remove` (replace the Task-7 stub)**

```rust
    fn remove(&self, workspace: &Path, session_id: &str) -> anyhow::Result<()> {
        let git_dir = rewind_git_dir(workspace, session_id);
        if git_dir.exists() {
            std::fs::remove_dir_all(&git_dir)?;
        }
        Ok(())
    }
```

- [ ] **Step 4: Run the remove tests + the whole rewind module**

Run: `cargo test --lib git::rewind 2>&1 | tail -15`
Expected: every `git::rewind` test passes.

- [ ] **Step 5: Commit**

```bash
git add src/git/rewind.rs
git commit -m "feat(rewind): GitRewindRepo::remove — delete disposable shadow db"
```

---

## Task 8: `MockRewindRepo` for orchestration tests

**Files:**
- Modify: `src/git/rewind.rs`

In-memory impl that records calls and can inject a checkpoint error — for testing `turn.rs` wiring without a real repo. Mirrors archebuild's `MockWorktreeManager`.

- [ ] **Step 1: Write the failing test (append a separate `mock_tests` module)**

```rust
#[cfg(test)]
mod mock_tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn records_calls_and_injects_error() {
        let mock = MockRewindRepo::new();
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
        // The errored call is still recorded.
        assert_eq!(mock.checkpoint_calls().len(), 2);

        mock.remove(&ws, "s1").unwrap();
        assert_eq!(mock.remove_calls(), vec!["s1".to_string()]);
    }
}
```

- [ ] **Step 2: Run to confirm failure**

Run: `cargo test --lib git::rewind::mock_tests 2>&1 | tail -15`
Expected: FAIL — `MockRewindRepo` does not exist.

- [ ] **Step 3: Implement `MockRewindRepo`**

```rust
use std::sync::Mutex;

#[derive(Default)]
struct MockState {
    init_calls: Vec<String>,
    checkpoint_calls: Vec<(String, u32)>,
    rewind_calls: Vec<(String, String)>,
    remove_calls: Vec<String>,
    checkpoint_error: Option<String>,
    next_sha: u32,
}

/// In-memory RewindRepo for orchestration tests. Records calls, no filesystem.
pub struct MockRewindRepo {
    state: Mutex<MockState>,
}

impl MockRewindRepo {
    pub fn new() -> Self { Self { state: Mutex::new(MockState::default()) } }
    pub fn inject_checkpoint_error(&self, msg: &str) {
        self.state.lock().unwrap().checkpoint_error = Some(msg.to_string());
    }
    pub fn init_calls(&self) -> Vec<String> { self.state.lock().unwrap().init_calls.clone() }
    pub fn checkpoint_calls(&self) -> Vec<(String, u32)> { self.state.lock().unwrap().checkpoint_calls.clone() }
    pub fn rewind_calls(&self) -> Vec<(String, String)> { self.state.lock().unwrap().rewind_calls.clone() }
    pub fn remove_calls(&self) -> Vec<String> { self.state.lock().unwrap().remove_calls.clone() }
}

impl Default for MockRewindRepo {
    fn default() -> Self { Self::new() }
}

impl RewindRepo for MockRewindRepo {
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
    fn rewind_to(&self, _ws: &Path, session_id: &str, _cb: &Path, sha: &str) -> anyhow::Result<()> {
        self.state.lock().unwrap().rewind_calls.push((session_id.to_string(), sha.to_string()));
        Ok(())
    }
    fn remove(&self, _ws: &Path, session_id: &str) -> anyhow::Result<()> {
        self.state.lock().unwrap().remove_calls.push(session_id.to_string());
        Ok(())
    }
}
```

- [ ] **Step 4: Run the mock test**

Run: `cargo test --lib git::rewind::mock_tests 2>&1 | tail -15`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/git/rewind.rs
git commit -m "test(rewind): MockRewindRepo for orchestration tests"
```

---

## Task 9: Add `code_checkpoint` to the `TurnComplete` event

**Files:**
- Modify: `src/session/transcript.rs:198-204` (the `TurnComplete` variant)
- Modify all `TurnComplete { … }` literal sites: `src/turn.rs:635`, `src/turn.rs:957`, `src/session/transcript.rs` (test sites ~464, 472, 563), `src/session/context.rs:253`, `src/tools/list_agents.rs:173`

- [ ] **Step 1: Add the field to the enum variant**

In `src/session/transcript.rs`, change the `TurnComplete` variant to:

```rust
    TurnComplete {
        usage: TurnUsage,
        codebase_hash: String,
        codebase_dirty: bool,
        /// Shadow-git checkpoint SHA for this turn (internal; powers `gait rewind`).
        /// None for legacy transcripts and non-git codebases.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        code_checkpoint: Option<String>,
        output: TurnOutput,
        ts: u64,
    },
```

- [ ] **Step 2: Build to enumerate the literal sites**

Run: `cargo build 2>&1 | tail -30`
Expected: FAIL — "missing field `code_checkpoint`" at each construction site. The compiler lists them.

- [ ] **Step 3: Add `code_checkpoint: None` to every NON-production literal**

For the test/non-turn sites — `src/session/transcript.rs` (~464, 472, 563), `src/session/context.rs:253`, `src/tools/list_agents.rs:173` — add `code_checkpoint: None,` to each `TurnComplete { … }` literal (alongside `codebase_dirty`).

(The two production sites in `turn.rs` are wired in Task 10 — for now add `code_checkpoint: None,` there too so the build is green; Task 10 replaces the value.)

- [ ] **Step 4: Build + test**

Run: `cargo build 2>&1 | tail -10 && cargo test 2>&1 | tail -15`
Expected: compiles; all tests pass (the new optional field defaults cleanly).

- [ ] **Step 5: Commit**

```bash
git add src/session/transcript.rs src/session/context.rs src/tools/list_agents.rs src/turn.rs
git commit -m "feat(transcript): add internal code_checkpoint field to TurnComplete"
```

---

## Task 10: Wire `init` + `checkpoint` into the turn loop

**Files:**
- Modify: `src/turn.rs`
- Test: `src/turn.rs` (`#[cfg(test)]`)

**CRITICAL ordering constraint (verified against the live code):** the two production `TurnComplete` sites today **emit the event FIRST, then call `commit_session`** (turn.rs:635-648 and 957-970). That order is load-bearing: `commit_session` git-commits `transcript.jsonl` *after* the `TurnComplete` line has been appended, so each turn's event lands in its own commit. If you commit *before* emit, the turn's `TurnComplete` is only committed on the next turn — and the **last turn's event is never committed**. So we must NOT introduce a "commit then emit" helper.

Instead the helper does **checkpoint only** (no commit). We call it *before* emit to obtain the SHA for the event, and leave the existing `commit_session` call *after* emit exactly where it is.

Also note the real field names (do not invent): the event uses `output: TurnOutput { message, result }` where `TurnOutput.message: String` (transcript.rs:23-25). `count_turn_complete` returns `usize` (transcript.rs:315).

- [ ] **Step 1: Write the failing orchestration test (append to turn.rs test module)**

```rust
    #[test]
    fn checkpoint_codebase_returns_sha_and_records_call() {
        use crate::git::rewind::MockRewindRepo;
        let ws = tempfile::TempDir::new().unwrap();
        let codebase = ws.path().to_path_buf(); // mock ignores the path
        let rewind = MockRewindRepo::new();

        let sha = checkpoint_codebase(&rewind, ws.path(), "scp1", &codebase, 1);

        assert_eq!(sha.as_deref(), Some("mock-sha-1"));
        assert_eq!(rewind.checkpoint_calls(), vec![("scp1".to_string(), 1u32)]);
    }

    #[test]
    fn checkpoint_codebase_tolerates_error() {
        use crate::git::rewind::MockRewindRepo;
        let ws = tempfile::TempDir::new().unwrap();
        let codebase = ws.path().to_path_buf();
        let rewind = MockRewindRepo::new();
        rewind.inject_checkpoint_error("disk full");

        // A checkpoint failure must NOT panic or propagate — it yields None.
        let sha = checkpoint_codebase(&rewind, ws.path(), "scp2", &codebase, 1);
        assert!(sha.is_none());
    }
```

- [ ] **Step 2: Run to confirm failure**

Run: `cargo test --lib turn::tests::checkpoint_codebase 2>&1 | tail -15`
Expected: FAIL — `checkpoint_codebase` not defined.

- [ ] **Step 3: Implement the `checkpoint_codebase` helper in `src/turn.rs`**

Add near the other free helpers (e.g. below `build_turn_context`). It does **not** commit — it only snapshots the codebase and returns the SHA:

```rust
use crate::git::rewind::RewindRepo;

/// Snapshot the codebase into the shadow db for this turn. Returns the
/// checkpoint SHA, or None if the codebase is not a git repo or the checkpoint
/// failed — a checkpoint failure must never fail the turn. Does NOT commit the
/// transcript; the caller's existing `commit_session` runs after emit.
fn checkpoint_codebase(
    rewind: &dyn RewindRepo,
    workspace: &Path,
    session_id: &str,
    codebase: &Path,
    turn: u32,
) -> Option<String> {
    match rewind.checkpoint(workspace, session_id, codebase, turn) {
        Ok(sha) if !sha.is_empty() => Some(sha),
        Ok(_) => None, // non-git codebase
        Err(e) => {
            // NOTE: `tracing` is NOT a dependency in this project — use eprintln!.
            eprintln!("rewind checkpoint failed for session {}: {}; rewind unavailable for this turn", session_id, e);
            None
        }
    }
}
```

- [ ] **Step 4: Wire `init` at session start**

After `src/turn.rs:357` (`GitWorkspaceRepo.create_session_worktree(...)` from Task 3), add:

```rust
    crate::git::rewind::GitRewindRepo
        .init(&opts.workspace, &opts.session_id, &opts.codebase_path)?;
```

- [ ] **Step 5: Insert the checkpoint at the implicit-output site (turn.rs:631-650)**

Make THREE surgical changes to this block — do not rewrite the rest of it:

(a) Change the `turn_n` line to cast `usize → u32`:

```rust
            let turn_n = count_turn_complete(&events_now) as u32 + 1;
```

(b) Immediately after that `turn_n` line and **before** `emit_event(...)`, add:

```rust
            let code_checkpoint = checkpoint_codebase(
                &crate::git::rewind::GitRewindRepo,
                &opts.workspace, &opts.session_id, &opts.codebase_path, turn_n,
            );
```

(c) Change the placeholder `code_checkpoint: None,` (added in Task 9 Step 3) in the `TurnComplete { … }` literal to `code_checkpoint,` so it uses the local computed in (b). Leave `output: TurnOutput { message: message.clone(), result: None }`, the `commit_session` call, the hooks loop, and the `return Ok(TurnResult::Done { message, result: None })` all unchanged.

The `commit_session` call at turn.rs:647-648 stays exactly where it is (after emit). The `&format!("session({}): turn {}", opts.session_id, turn_n)` argument now passes a `u32`, which formats identically.

- [ ] **Step 6: Insert the checkpoint at the explicit `output()` site (turn.rs:953-972)**

Same three changes to that block:

(a) `let turn_n = count_turn_complete(&events_now) as u32 + 1;`

(b) After `turn_n`, before `emit_event`:

```rust
                        let code_checkpoint = checkpoint_codebase(
                            &crate::git::rewind::GitRewindRepo,
                            &opts.workspace, &opts.session_id, &opts.codebase_path, turn_n,
                        );
```

(c) Change the placeholder `code_checkpoint: None,` (added in Task 9 Step 3) in that `TurnComplete { … }` literal to `code_checkpoint,`. Leave `output: TurnOutput { message: message.clone(), result: result.clone() }`, the `commit_session` call (turn.rs:969), the hooks loop, and `return Ok(TurnResult::Done { message, result })` unchanged.

- [ ] **Step 7: Build + verify existing turn tests still pass (ordering preserved)**

Run: `cargo build 2>&1 | tail -10 && cargo test --lib turn:: 2>&1 | tail -20`
Expected: PASS — including `tc_event` / `has_turn_complete` tests (turn.rs:1238-1343). Because emit-then-commit order is unchanged, these must not regress. If any fail, you changed the order — revert to: cast `turn_n`, compute `code_checkpoint` before emit, emit, then the original `commit_session`.

- [ ] **Step 8: Add a real-repo integration test (append to turn.rs test module)**

Most existing `run_turn` tests set `codebase_path: ws.path()` (the workspace, which is itself a git repo) — that degenerate case still works but checkpoints the whole workspace. This new test uses a **separate** git codebase so the assertion is meaningful:

```rust
    #[tokio::test]
    async fn run_turn_records_code_checkpoint_for_git_codebase() {
        // Copy the closest existing run_turn test in this file (e.g. the one at
        // ~turn.rs:1145) for provider/tool setup. The ONLY differences:
        //   1. Create a separate codebase dir that is its own git repo with one
        //      commit, and set TurnOpts.codebase_path to it (NOT ws.path()):
        //        let cb = tempfile::TempDir::new().unwrap();
        //        let repo = git2::Repository::init(cb.path()).unwrap();
        //        // configure user, write a file, add, commit "init" (see Task 4 fixture)
        //   2. After the turn completes, assert the event carries a checkpoint
        //      and the shadow db exists:
        //        let events = read_events(&tx_path).unwrap();
        //        let cp = events.iter().rev().find_map(|e| match e {
        //            AnyEvent::TurnComplete { code_checkpoint, .. } => Some(code_checkpoint.clone()),
        //            _ => None }).flatten();
        //        assert!(cp.is_some(), "expected a code checkpoint SHA");
        //        assert!(crate::git::rewind_git_dir(ws.path(), session_id).exists());
    }
```

- [ ] **Step 9: Build + full test**

Run: `cargo build 2>&1 | tail -10 && cargo test 2>&1 | tail -15`
Expected: all pass.

- [ ] **Step 10: Commit**

```bash
git add src/turn.rs
git commit -m "feat(turn): init shadow db at session start, checkpoint codebase each turn"
```

---

## Task 11: `gait rewind` CLI command

**Files:**
- Modify: `src/bin/gait.rs`

```
gait rewind <session-id> --turn <n> [--code] [--transcript] [--workspace <path>]
gait rewind <session-id> --list   [--workspace <path>]
```

`--list` prints each turn with its summary + checkpoint. A rewind with neither `--code` nor `--transcript` does both. Requires no in-progress turn (acquire the session lock first).

- [ ] **Step 1: Read the existing CLI dispatch to confirm the pattern**

Run: `grep -n "=> cmd_\|fn cmd_\|match args" src/bin/gait.rs | head -40`
Confirmed pattern (do not assume — verify): dispatch is a **manual** `match args[1].as_str()` (~bin/gait.rs:107), and each subcommand is a `"name" => cmd_name(&args[2..]),` arm whose `cmd_name(argv: &[String])` wrapper parses its own flags (see `cmd_finish` at ~:478). Follow this exactly — there is no clap derive.

- [ ] **Step 2: Add the dispatch arm + the `cmd_rewind` arg-parser wrapper**

In the `match args[1].as_str()` block (~:107), add:

```rust
        "rewind" => cmd_rewind(&args[2..]),
```

Then add the wrapper that parses argv and calls the typed handler (model it on `cmd_finish`):

```rust
fn cmd_rewind(argv: &[String]) -> anyhow::Result<()> {
    let mut session_id: Option<String> = None;
    let mut turn: Option<u32> = None;
    let mut code = false;
    let mut transcript = false;
    let mut list = false;
    let mut workspace: Option<PathBuf> = None;

    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "--turn" => { i += 1; turn = Some(argv.get(i)
                .ok_or_else(|| anyhow::anyhow!("--turn needs a value"))?.parse()?); }
            "--code" => code = true,
            "--transcript" => transcript = true,
            "--list" => list = true,
            "--workspace" => { i += 1; workspace = Some(PathBuf::from(argv.get(i)
                .ok_or_else(|| anyhow::anyhow!("--workspace needs a value"))?)); }
            other if !other.starts_with("--") && session_id.is_none() => {
                session_id = Some(other.to_string());
            }
            other => anyhow::bail!("unexpected argument: {}", other),
        }
        i += 1;
    }

    let session_id = session_id.ok_or_else(|| anyhow::anyhow!("usage: gait rewind <session-id> [--turn N | --list] [--code] [--transcript]"))?;
    let workspace = workspace.unwrap_or_else(|| std::env::current_dir().unwrap());
    rewind_command(&session_id, turn, code, transcript, list, workspace)
}
```

- [ ] **Step 3: Implement the typed handler**

```rust
fn rewind_command(
    session_id: &str,
    turn: Option<u32>,
    code: bool,
    transcript: bool,
    list: bool,
    workspace: PathBuf,
) -> anyhow::Result<()> {
    use gait_ai::session::transcript::{read_events, AnyEvent};
    use gait_ai::git::rewind::{RewindRepo, GitRewindRepo};

    let session_dir = workspace.join("sessions").join(session_id);
    let tx_path = session_dir.join("transcript.jsonl");
    let events = read_events(&tx_path)?;

    // Collect (turn_index, summary, checkpoint) from TurnComplete events.
    let mut turns: Vec<(u32, String, Option<String>)> = Vec::new();
    for e in &events {
        if let AnyEvent::TurnComplete { output, code_checkpoint, .. } = e {
            let idx = turns.len() as u32 + 1;
            turns.push((idx, output.summary.clone(), code_checkpoint.clone()));
        }
    }

    if list {
        for (idx, summary, cp) in &turns {
            let mark = cp.as_deref().map(|s| &s[..s.len().min(8)]).unwrap_or("--------");
            println!("turn {idx}  [{mark}]  {summary}");
        }
        return Ok(());
    }

    let turn = turn.ok_or_else(|| anyhow::anyhow!("--turn <n> is required (or use --list)"))?;
    let (_, _, checkpoint) = turns.iter()
        .find(|(idx, _, _)| *idx == turn)
        .ok_or_else(|| anyhow::anyhow!("no turn {} in session {}", turn, session_id))?
        .clone();

    // Default (neither flag) = both axes.
    let do_code = code || (!code && !transcript);
    let do_transcript = transcript || (!code && !transcript);

    // Guard: refuse if a turn is in progress (lock held by a live PID).
    let lock = session_dir.join("lock");
    if lock.exists() {
        if let Ok(pid) = std::fs::read_to_string(&lock).unwrap_or_default().trim().parse::<i32>() {
            if unsafe { libc::kill(pid, 0) == 0 } {
                anyhow::bail!("session {} is running (PID {}); stop it before rewinding", session_id, pid);
            }
        }
    }

    if do_code {
        let codebase = gait_ai::session::session_json::read_session_json(&workspace, session_id)
            .codebase_path
            .map(PathBuf::from)
            .ok_or_else(|| anyhow::anyhow!("session.json has no codebase_path; cannot rewind code"))?;
        match checkpoint.as_deref() {
            Some(sha) => {
                GitRewindRepo.rewind_to(&workspace, session_id, &codebase, sha)?;
                println!("code rewound to turn {turn} ({})", &sha[..sha.len().min(8)]);
            }
            None => println!("turn {turn} has no code checkpoint (non-git codebase); code not rewound"),
        }
    }

    if do_transcript {
        rewind_transcript_to_turn(&workspace, session_id, turn)?;
        println!("transcript rewound to turn {turn}");
    }
    Ok(())
}
```

- [ ] **Step 4: Implement `rewind_transcript_to_turn`**

Reset the workspace `session/<id>` branch so its committed transcript no longer contains turns after `turn`. The exact commit summary is `session(<id>): turn N` (turn.rs:648,970; a gait `Co-Authored-By` trailer follows on later lines, but `commit.summary()` returns only the first line). Match the **full** summary — not a `"turn N"` suffix — to avoid matching workflow commits (`workflow: …`) or the `session(<id>): init` commit, and to avoid any ambiguity:

```rust
fn rewind_transcript_to_turn(workspace: &Path, session_id: &str, turn: u32) -> anyhow::Result<()> {
    let session_dir = workspace.join("sessions").join(session_id);
    let repo = git2::Repository::open(&session_dir)?;
    let needle = format!("session({}): turn {}", session_id, turn);
    let mut walk = repo.revwalk()?;
    walk.push_head()?;
    let mut target: Option<git2::Oid> = None;
    for oid in walk {
        let oid = oid?;
        let commit = repo.find_commit(oid)?;
        if commit.summary() == Some(needle.as_str()) {
            target = Some(oid);
            break;
        }
    }
    let oid = target.ok_or_else(|| anyhow::anyhow!(
        "no commit for turn {} in session {} (transcript rewind applies to interactive turn sessions, not workflows)",
        turn, session_id))?;
    let obj = repo.find_object(oid, None)?;
    repo.reset(&obj, git2::ResetType::Hard, None)?;
    Ok(())
}
```

- [ ] **Step 5: Build**

Run: `cargo build 2>&1 | tail -20`
Expected: compiles. If `read_session_json` is not `pub` at that path, adjust the import to its actual location (`gait_ai::session::read_session_json`).

- [ ] **Step 6: Manual smoke test**

```bash
cargo build --release --target-dir /tmp/gait-target 2>&1 | tail -3
GAIT=/tmp/gait-target/release/gait
WS=$(mktemp -d); CB=$(mktemp -d)
cd "$CB" && git init -q && git config user.email x@x.com && git config user.name x \
  && echo one > f.txt && git add f.txt && git commit -qm init && cd -
cd "$WS" && "$GAIT" init >/dev/null 2>&1 || true
# Run two turns (mock/real model per your auth); then:
"$GAIT" rewind <session-id> --list --workspace "$WS"
"$GAIT" rewind <session-id> --turn 1 --code --workspace "$WS"
cat "$CB"/f.txt   # expect turn-1 content
```

Expected: `--list` prints turns with checkpoint prefixes; `--turn 1 --code` restores the codebase to turn 1; the codebase's own `git log` is unchanged.

- [ ] **Step 7: Commit**

```bash
git add src/bin/gait.rs
git commit -m "feat(cli): gait rewind — restore code and/or transcript to a past turn"
```

---

## Task 12: Clean up the shadow db on `gait finish`

**Files:**
- Modify: `src/bin/gait.rs:487` (the `finish` handler)

- [ ] **Step 1: Add `rewind.remove` after the successful finish**

At `src/bin/gait.rs`, in the finish handler right after `GitWorkspaceRepo.finish_session(&workspace, &session_id)` succeeds (from Task 3), add:

```rust
    use gait_ai::git::rewind::{RewindRepo, GitRewindRepo};
    if let Err(e) = GitRewindRepo.remove(&workspace, &session_id) {
        eprintln!("warning: failed to remove rewind db for {}: {}", session_id, e);
    }
```

(`finish_session` already removes the session worktree dir; `remove` deletes `sessions/<id>/codebase.git` specifically in case the worktree removal left it — harmless if already gone.)

- [ ] **Step 2: Build + test**

Run: `cargo build 2>&1 | tail -10 && cargo test 2>&1 | tail -15`
Expected: all pass.

- [ ] **Step 3: Commit**

```bash
git add src/bin/gait.rs
git commit -m "feat(finish): remove disposable shadow db on gait finish"
```

---

## Final Verification

- [ ] **Full suite green**

Run: `cargo test 2>&1 | tail -15`
Expected: all tests pass (original 188 + new codebase/workspace/rewind/turn tests), 0 failures.

- [ ] **No leftover free-fn references**

Run: `grep -rn "git::get_codebase_hash\|git::get_dirty_files\|git::create_session_worktree\|git::commit_session\|git::list_active_sessions\|git::finish_session\|git::init_workspace\|git::create_branch_worktree\|git::remove_branch_worktree\|git::merge_branch_ff" src/`
Expected: no matches — every caller migrated to a trait method.

- [ ] **Clippy clean**

Run: `cargo clippy 2>&1 | tail -20`
Expected: no new warnings introduced by these changes.
```

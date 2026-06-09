# Code Rewind — Design

> Per-turn rewind of codebase file state, analogous to Claude Code's `/rewind`,
> built so gait composes cleanly with outer orchestrators (e.g. archebuild) that
> may already own the codebase's real git.

**Date:** 2026-06-09
**Status:** Design — pending implementation plan

---

## Goal

Let a user roll the codebase back to the file state it had at the end of any
previous turn in a session — without touching the codebase's real git history,
and without conflicting with any outer tool that owns the codebase's commit
cadence.

Rewind has two independent axes:

| Axis | What it restores | Mechanism |
|---|---|---|
| **Transcript** | Conversation history | reset on the workspace `session/<id>` branch (already exists) |
| **Code** | Codebase file state | reset on a **shadow git** in the workspace (this design) |

The two are decoupled: a user can rewind code only, transcript only, or both —
matching Claude Code's "restore code", "restore conversation", "restore both".
This design covers the **code** axis; the transcript axis already works via
session worktrees ([2026-06-05-session-worktrees](../plans/2026-06-05-session-worktrees.md)).

---

## Background: why not the obvious approaches

- **Commit on a `gait/session/<id>` branch in the codebase repo.** Powerful
  (captures shell changes too), but writes to the codebase's `.git`. When gait
  is composed under an orchestrator that *also* commits there (archebuild
  manages per-DAG feature branches + worktrees in the codebase), the two layers
  fight over the same git — double commits, competing branches. gait must not
  assume it owns the codebase's git.

- **Custom file-snapshot store (Claude Code's approach).** Backs up files
  before each Write/Edit. Clean, but misses changes made by the `shell` tool
  (rm/mv/sed), which is central to gait. Reimplements half of git.

The ownership principle that resolves this: **whoever creates the codebase
worktree owns its git.** gait, handed a `--codebase` path, treats it as
*borrowed* — it never writes to that path's `.git` unless the user explicitly
asks.

---

## Approach: the inverted worktree ("shadow git")

A normal git worktree is *one git database, many working trees*. We invert it:
**one working tree (`/user/repo`), with a second git database in the workspace**
that snapshots it.

Git supports this natively via the `GIT_DIR` / `GIT_WORK_TREE` split:

```
git --git-dir=<workspace>/sessions/<id>/codebase.git \
    --work-tree=/user/repo \
    add -A && commit
```

This commits the *file state of `/user/repo`* into a database living in the
workspace, **without ever writing to `/user/repo/.git`**. Rewind is the mirror —
`reset --hard <sha>` with the same flag pair rewrites the files in `/user/repo`
back to a past turn. (git2 equivalent: open the shadow git-dir and
`repo.set_workdir("/user/repo", false)`.)

### Why this satisfies composition

gait never touches the codebase's own `.git`. If archebuild owns the codebase's
real git and commits its own branches there, gait's shadow database is
orthogonal — no double-commits, no competing branches. gait owns *its* git (in
the workspace); the codebase's git stays owned by whoever owns it.

### Key properties

- **The shadow git is disposable scratch, not history.** The user's
  `/user/repo/.git` is the only real history. `codebase.git/` exists solely so
  we can `reset --hard` to a past turn. On `gait finish` / session cleanup,
  `rm -rf sessions/<id>/codebase.git/` and nothing of value is lost.
- **Invisible to the agent.** The agent already sees the *real* repo HEAD as
  `codebase_hash` in its `<gait-state>` context — that stays. The shadow git's
  per-turn checkpoint SHAs are gait-internal, stored in the `TurnComplete`
  event, never surfaced to the agent. As far as the agent knows, it is just
  editing files.
- **Cheap to create.** Not a literal deep copy of `.git`. The shadow borrows the
  codebase's object store via `objects/info/alternates` (or `git clone --local`
  hardlinks). Copy refs + a HEAD pointer; share objects. The shadow's first
  commit is a child of the real HEAD so diffs stay coherent.
- **The nested `.git` is a non-issue.** Git always auto-ignores a directory
  named `.git` at the work-tree root, so `add -A` will not stage
  `/user/repo/.git`. The shadow *does* honor the codebase's `.gitignore` (it is
  in the work tree) — which is what we want.

### What it does NOT solve: parallel isolation

The shadow git gives independent *history*, but the files on disk live in one
place. Two agents editing `/user/repo` simultaneously clobber each other on
disk — git can't prevent that. Parallel agents on one codebase each need their
own physical checkout; providing those is the **orchestrator's** job
(consistent with the ownership principle). For gait's normal single-agent
sequential mode, one working tree + one shadow git is all that's needed.

---

## Architecture

Two unrelated git databases live in each session folder, committed in lockstep
per turn:

```
workspace/
  sessions/<id>/
    .git              ← transcript git (branch session/<id>)
                         tracks transcript.jsonl, session.json   [EXISTING]
    transcript.jsonl
    session.json
    codebase.git/     ← shadow git (this design)
                         snapshots /user/repo file state per turn [NEW]
```

`codebase.git/` is gitignored from the transcript git — it is never committed
into transcript history.

### Component: `ShadowRepo` trait

Following archebuild's trait + mock pattern (`WorktreeManager`,
`VersionControlOps`) so the codebase-git capability is injectable and testable.

```rust
/// Disposable, workspace-side git database that snapshots a borrowed
/// working tree (the codebase) for per-turn rewind. Never writes to the
/// codebase's own .git.
pub trait ShadowRepo: Send + Sync {
    /// Create sessions/<id>/codebase.git as a shadow of `codebase_path`,
    /// borrowing its object store via alternates. Idempotent — attaches if
    /// already present. No-op if the codebase is not a git repo (rewind
    /// simply unavailable; logged).
    fn init(&self, workspace: &Path, session_id: &str, codebase_path: &Path)
        -> Result<()>;

    /// Stage all of the codebase working tree and commit into the shadow.
    /// Returns the checkpoint SHA. No-op (returns current HEAD) if nothing
    /// changed since last checkpoint.
    fn checkpoint(&self, workspace: &Path, session_id: &str, codebase_path: &Path,
        turn: u32) -> Result<String>;

    /// reset --hard <sha> with work-tree = codebase_path. Rewrites the
    /// codebase files to the state captured at that checkpoint.
    fn rewind_to(&self, workspace: &Path, session_id: &str, codebase_path: &Path,
        sha: &str) -> Result<()>;

    /// rm -rf sessions/<id>/codebase.git — disposable scratch.
    fn remove(&self, workspace: &Path, session_id: &str) -> Result<()>;
}

pub struct GitShadowRepo; // git2-backed
// pub struct MockShadowRepo; // tests
```

Implementation notes (from archebuild's hard-won lessons):
- Use git2 with `tokio::task::spawn_blocking` if the call sites are async; gait's
  `git.rs` is currently synchronous, so plain git2 is fine.
- Open the shadow strictly by its git-dir (do **not** use
  `Repository::discover`, which can walk up into the codebase's real `.git` —
  archebuild hit cross-worktree `index.lock` corruption from exactly this).
- `checkpoint` detects "nothing staged" by diffing tree-to-index and returns the
  existing HEAD SHA rather than creating an empty commit.

### Wiring into the turn loop (`turn.rs`)

`run_turn` already computes `codebase_hash` (real HEAD) and writes a
`TurnComplete` event. Add:

1. **Session start:** `shadow.init(workspace, session_id, codebase_path)` —
   alongside `create_session_worktree`.
2. **End of turn**, after the existing transcript commit:
   `let checkpoint = shadow.checkpoint(workspace, session_id, codebase_path, turn)`.
3. **Record the checkpoint** in the `TurnComplete` event (new field, internal).

### Transcript event change

`AnyEvent::TurnComplete` gains one optional, internal field:

```rust
TurnComplete {
    usage: TurnUsage,
    codebase_hash: String,        // real repo HEAD — unchanged, agent-visible
    codebase_dirty: bool,
    code_checkpoint: Option<String>, // NEW: shadow git SHA — internal, for rewind
    output: TurnOutput,
    ts: u64,
}
```

`Option` so legacy transcripts and non-git codebases deserialize cleanly.
This is the rewind index: turn N → `code_checkpoint` SHA.

---

## CLI surface

```
gait rewind <session-id> --turn <n> [--code] [--transcript] [--workspace <path>]
```

- `--code` only → `shadow.rewind_to(... sha_from_turn_n)`. Transcript untouched.
- `--transcript` only → reset workspace `session/<id>` branch to turn N's commit.
  Codebase files untouched.
- both (default if neither flag given) → both resets, to the same turn.
- `gait rewind <id> --list` → print turns with timestamps + summaries so the
  user can pick a point (reads `TurnComplete` events).

Rewind requires no in-progress turn for the session (same constraint as
`removeSessionWorktree`). Acquire the session lock before resetting.

---

## Lifecycle & cleanup

- **`gait finish <id>`** — after merging the transcript branch, `shadow.remove`
  deletes `codebase.git/`. The codebase files are left exactly as the last turn
  produced them; the user commits to their real repo whenever they choose.
- **Crash safety** — the shadow is reconstructable-irrelevant: it is scratch. If
  a session dir is orphaned, its `codebase.git/` is removed with it. `init` is
  idempotent and re-attaches on resume.
- **"User says commit"** — gait does nothing special: the work already lives in
  the codebase working tree. The user (or orchestrator) runs a normal commit in
  the real repo. The shadow was only ever an undo-buffer.

---

## Testing

Mirror archebuild's git test style (real temp repos via `tempfile`, git2 for
assertions):

- `init` creates `codebase.git` borrowing objects via alternates; does not write
  to the codebase `.git`.
- `checkpoint` after a file edit returns a fresh SHA; a second checkpoint with no
  change returns the same SHA (no empty commit).
- `rewind_to` an earlier SHA restores file content **and** removes files created
  after that checkpoint; verify `/user/repo/.git` HEAD is unchanged throughout.
- shell-tool changes (create/delete/rename a file via a shell command) are
  captured and rewound — the property Claude Code's snapshot store lacks.
- A `--code`-only rewind leaves the transcript branch untouched, and vice versa.
- `remove` deletes `codebase.git` and the codebase working tree is left intact.
- Non-git codebase: `init` is a logged no-op; `checkpoint` returns `None`-ish;
  rewind reports unavailable rather than erroring the turn.

---

## Out of scope

- Per-tool-call (sub-turn) checkpoints — turn granularity only, matching Claude
  Code's "each user message".
- Workflow-node-level rewind — workflows are a meta-layer; revisit separately.
- Rewinding *across* sessions or after `gait finish`.
- Parallel-agent working-tree isolation — orchestrator's responsibility.
```

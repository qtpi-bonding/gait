---
status: done
---

# Rename: WorkspaceRepo → SessionRepo, RewindRepo → CheckpointRepo, gait rewind → gait undo

## Why

The current names are inconsistent. `CodebaseRepo` is named after what it manages.
`WorkspaceRepo` is named after a directory. `RewindRepo` is named after a user action.
This plan makes all three names consistent — each named after what it *manages or stores* —
and renames the user-facing `gait rewind` / `/rewind` to `gait undo` / `/u`.

## Final naming

| Layer | Before | After | Rationale |
|-------|--------|-------|-----------|
| Trait | `WorkspaceRepo` | `SessionRepo` | manages session branches + worktrees |
| Git impl | `GitWorkspaceRepo` | `GitSessionRepo` | follows impl naming pattern |
| Trait | `RewindRepo` | `CheckpointRepo` | manages per-turn code snapshots |
| Git impl | `GitRewindRepo` | `GitCheckpointRepo` | follows impl naming pattern |
| Mock | `MockRewindRepo` | `MockCheckpointRepo` | follows mock naming pattern |
| Mock field | `rewind_calls` | `restore_calls` | matches renamed method |
| Mock accessor | `rewind_calls()` | `restore_calls()` | follows field rename |
| `Repos` field | `repos.rewind` | `repos.checkpoints` | matches trait name |
| Method | `rewind_to()` | `restore_to()` | reads like English: "restore to checkpoint" |
| Method | `checkpoint()` | keep | already correct |
| Method | `init()` | keep | already correct |
| Method | `remove()` | keep | already correct |
| Internal fn | `rewind_git_dir()` | `checkpoint_git_dir()` | matches new module name |
| Commit string | `"init rewind"` | `"init checkpoint"` | shadow git commit message |
| Commit string | `"rewind({}): turn {}"` | `"checkpoint({}): turn {}"` | shadow git commit message |
| Branch ref | `refs/heads/rewind` (shadow db) | **leave unchanged** | backward compat: existing on-disk sessions already have this ref; renaming breaks them |
| File | `src/git/workspace.rs` | `src/git/session.rs` | matches trait |
| File | `src/git/rewind.rs` | `src/git/checkpoint.rs` | matches trait |
| CLI subcommand | `gait rewind` | `gait undo` | user-facing, universally understood |
| CLI fn | `cmd_rewind` | `cmd_undo` | internal, follows subcommand name |
| CLI fn | `rewind_command` | `undo_command` | internal |
| REPL command | (not yet impl) | `/u` / `/u N` | short form, matches `gait undo` |
| Transcript comment | `gait rewind` ref | `gait undo` ref | keep accurate |

`code_checkpoint` in the transcript struct stays — it's already the right name.

`init_workspace` on the trait stays — it initialises the workspace git repo itself, not a
session, so the name is correct even on `SessionRepo`.

Module paths: `git::session::SessionRepo` lives under `src/git/session.rs` and is distinct
from `crate::session` (`src/session/mod.rs`). Rust resolves these as separate paths; no
conflict, though documentation should be clear about the distinction.

## Files touched

- `src/git/workspace.rs` → rename to `src/git/session.rs`, rename all types inside
- `src/git/rewind.rs` → rename to `src/git/checkpoint.rs`, rename all types inside
  (includes `rewind_git_dir` → `checkpoint_git_dir`, commit message strings, mock fields)
- `src/git/mod.rs` — update `pub mod` declarations, imports, `Repos` struct fields,
  `rewind_git_dir` fn rename + all callers inside the module
- `src/turn.rs` — update imports, `rewind` param → `checkpoints`, method calls, comments,
  any `rewind_git_dir` calls
- `src/tools/list_agents.rs` — update imports + call sites
- `src/bin/gait.rs` — rename subcommand, fns, usage strings, `repos.rewind` field
- `src/session/transcript.rs` — update comment only (line ~202: "powers `gait rewind`")
- `tests/integration_commands.rs` — update imports + call sites
- `tests/integration_turn.rs` (if it imports rewind) — update imports

## REPL `/u` implementation (new work, same PR)

Add to the `gait chat` slash-command handler:

```
/u          → list turns for current session (like gait undo --list)
/u N        → restore to turn N (like gait undo --turn N --code --transcript)
```

Error cases:
- `/u N` when session has no checkpoints → print an error line, `continue 'spawn` back to
  readline (do not re-enter model)
- `/u N` where N is out of range → same: print error, loop back to readline

After a successful `/u N`, write the undo event to the transcript and `continue 'spawn` so
the model re-enters with the restored context.

## Order of changes

1. Rename files: `workspace.rs` → `session.rs`, `rewind.rs` → `checkpoint.rs`
2. Rename types + methods + internal fn + commit strings inside those files (replace_all)
3. Update `src/git/mod.rs` imports, `pub mod` names, `rewind_git_dir` → `checkpoint_git_dir`,
   `Repos` struct field
4. Update all other consumers: `turn.rs`, `list_agents.rs`, `bin/gait.rs`,
   `tests/integration_commands.rs`, `tests/integration_turn.rs` — grep for any remaining
   `rewind`, `WorkspaceRepo`, `MockRewindRepo` occurrences
5. Update CLI subcommand string `"rewind"` → `"undo"` and all usage strings
6. Update transcript comment (`src/session/transcript.rs`)
7. `cargo test` — all green before touching REPL
8. Add `/u` / `/u N` REPL commands in `chat.rs`
9. Final `cargo test`

## What does NOT change

- `refs/heads/rewind` in the shadow git db — leave for backward compat with existing sessions
- `code_checkpoint` field on `TurnComplete` transcript event — already correct
- `checkpoint()` method name on the trait — already correct
- `init_workspace` method name on the trait — still correct (initialises workspace repo)
- `CodebaseRepo` — unaffected
- The shadow git db behaviour — names only

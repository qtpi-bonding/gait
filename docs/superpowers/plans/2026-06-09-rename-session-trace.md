---
status: done
---

# Rename: SessionRepo → TraceRepo

## Why

`SessionRepo` is named after the scope (a session) rather than what it manages.
What it actually manages is the **session trace**: transcript, command outputs, logs,
tool results — the full observability record of an LLM run. `TraceRepo` names the
concept, not the scope.

## Rename table

| Before | After |
|--------|-------|
| `SessionRepo` (trait) | `TraceRepo` |
| `GitSessionRepo` (impl) | `GitTraceRepo` |
| `Repos.workspace` field | `Repos.traces` |
| `repos.workspace` call sites | `repos.traces` |
| `pub mod session` in git/mod.rs | `pub mod trace` |
| `src/git/session.rs` file | `src/git/trace.rs` |
| `use crate::git::session::...` imports | `use crate::git::trace::...` |

## Method names — keep as-is

All methods on the trait stay unchanged:
- `init_workspace` — still correct (initialises the workspace git repo itself)
- `create_session_worktree` — still correct (creates a worktree for a session)
- `commit_session` — still correct (commits session artifacts)
- `list_active_sessions` — still correct
- `finish_session` — still correct
- `create_branch_worktree` / `remove_branch_worktree` / `merge_branch_ff` — still correct

These methods operate on sessions; "session" in their names refers to their arguments,
not to the repo concept. No rename needed.

## Files touched

- `src/git/session.rs` → rename file to `src/git/trace.rs`, rename trait + impl inside
- `src/git/mod.rs` — `pub mod session` → `pub mod trace`, update imports, `Repos.workspace` → `Repos.traces`
- `src/turn.rs` — update imports, `repos.workspace` → `repos.traces`
- `src/tools/list_agents.rs` — update imports + call sites
- `src/bin/gait.rs` — update imports + `repos.workspace` → `repos.traces`
- `src/chat.rs` — update imports + `repos.workspace` → `repos.traces`
- `tests/integration_commands.rs` — update imports + call sites
- `tests/integration_turn.rs` — update imports if present

## What does NOT change

- `crate::session` module (`src/session/`) — unaffected, different module
- `CheckpointRepo`, `CodebaseRepo` — unaffected
- All method names on `TraceRepo`
- Any `session_id` variable names — those refer to the session concept, not the repo

## Order of changes

1. `cp src/git/session.rs src/git/trace.rs` then rename trait/impl inside
2. Update `src/git/mod.rs`
3. Update all consumers (grep for `SessionRepo`, `GitSessionRepo`, `repos.workspace`, `git::session`)
4. Delete `src/git/session.rs`
5. `cargo test` — all green

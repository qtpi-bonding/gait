# Repo Dependency Injection Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax.

**Goal:** Make the three git repos (`CodebaseRepo`, `WorkspaceRepo`, `RewindRepo`) injectable via a single `Repos` bundle, threaded from composition roots (CLI commands, workflow runner) through `TurnOpts`/`run_turn` and the tool layer (`spawn_agent`), instead of hardcoding the concrete `Git*Repo` structs at call sites.

**Architecture:** A `Repos { codebase, workspace, rewind }` bundle of `Arc<dyn ...>` trait objects, with `Repos::production()` returning the `Git*` impls. Reusable components (`run_turn`, `SpawnAgentTool`) RECEIVE a `Repos`; entry points (composition roots) CONSTRUCT `Repos::production()` and pass it down. This closes review finding S2 (run_turn's init/checkpoint wiring becomes mock-testable) and makes the 3-git-context design's seams explicit and swappable.

**Tech Stack:** Rust, `git2`, `std::sync::Arc`, `anyhow`. Behavior-preserving refactor — the full test suite (currently 384 passing) is the safety net. `tracing` is NOT a dependency (use `eprintln!`).

**Branch:** continue on `design/code-rewind`. NEVER stage/commit `README.md`, `gait.jsonc`, or `docs/*`.

**Spec context:** completes the architecture from `docs/superpowers/specs/2026-06-09-code-rewind-design.md` (three bounded repo traits) by making them injectable.

---

## File Structure

| File | Change |
|---|---|
| `src/git/mod.rs` | Add `Repos` bundle + `Repos::production()`. Re-export. |
| `src/turn.rs` | `TurnOpts` gains `repos: Repos`; `run_turn` + `apply_curate_filter` use `opts.repos.*`; all `TurnOpts` constructors set `repos`. |
| `src/tools/spawn_agent.rs` | `SpawnAgentTool` gains a `repos: Repos` field; uses it + forwards to child `TurnOpts`. |
| `src/tools/mod.rs` | `builtin_tools(...)` gains a `repos: Repos` param, passes to `SpawnAgentTool::new`. |
| `src/bin/gait.rs` | `cmd_run`/`cmd_chat`/`cmd_finish`/`cmd_rewind`/`init` build `Repos::production()` and pass/use it. |
| `src/workflow/exec_prompt.rs` | Build `Repos::production()`, pass to `builtin_tools` + `TurnOpts`. |
| `src/workflow/runner.rs` | Build `Repos::production()`, use for `create_session_worktree`/`commit_session`. |
| `src/harness.rs` | Use an injected/owned `Repos` for `commit_session`. |

**Current direct `Git*Repo` production call sites (must migrate):** `turn.rs` (create_session_worktree, 5× commit_session, rewind init, 2× checkpoint_codebase arg, codebase head_hash/dirty_files, curate rewind.remove); `tools/spawn_agent.rs` (create_session_worktree, commit_session, codebase head_hash); `workflow/runner.rs` (create_session_worktree, 6× commit_session); `bin/gait.rs` (init_workspace, finish_session, list_active_sessions, rewind remove + rewind_to); `harness.rs:91` (commit_session). **Test call sites keep using `Git*Repo` directly** (they test the impls / use real temp repos) — only `TurnOpts` construction in tests must add `repos`.

---

## Task 1: Add the `Repos` bundle

**Files:** `src/git/mod.rs`

- [ ] **Step 1: Add the bundle to `src/git/mod.rs`**

Append (the helper trait imports are needed for the `Arc<dyn ...>` types):

```rust
use std::sync::Arc;
use crate::git::codebase::{CodebaseRepo, GitCodebaseRepo};
use crate::git::workspace::{WorkspaceRepo, GitWorkspaceRepo};
use crate::git::rewind::{RewindRepo, GitRewindRepo};

/// Bundle of the three git-context repos, injected from composition roots.
/// `run_turn` and tools receive a `Repos`; entry points build `Repos::production()`.
#[derive(Clone)]
pub struct Repos {
    pub codebase: Arc<dyn CodebaseRepo>,
    pub workspace: Arc<dyn WorkspaceRepo>,
    pub rewind: Arc<dyn RewindRepo>,
}

impl Repos {
    /// The real git-backed implementations used in production.
    pub fn production() -> Self {
        Repos {
            codebase: Arc::new(GitCodebaseRepo),
            workspace: Arc::new(GitWorkspaceRepo),
            rewind: Arc::new(GitRewindRepo),
        }
    }
}
```

- [ ] **Step 2: Confirm it's exported**

`git` is already `pub mod git;` in `lib.rs`, and `mod.rs` items are public, so `crate::git::Repos` is reachable. Verify there's no `lib.rs` re-export needed by building in Step 3.

- [ ] **Step 3: Build**

Run: `cargo build 2>&1 | tail -15`
Expected: compiles (the `use` of the trait/impl paths resolves; `Repos` is unused so far — a dead_code warning is acceptable, or add `#[allow(dead_code)]` on `Repos` and `production` if the build denies warnings).

- [ ] **Step 4: Commit**

```bash
git add src/git/mod.rs
git commit -m "feat(git): Repos bundle (codebase/workspace/rewind) with production()"
```

---

## Task 2: Inject `Repos` into `TurnOpts` and `run_turn`

**Files:** `src/turn.rs`

- [ ] **Step 1: Add the field to `TurnOpts`**

In `pub struct TurnOpts { ... }`, add (anywhere in the struct):

```rust
    /// Injected git repos (codebase/workspace/rewind).
    pub repos: crate::git::Repos,
```

- [ ] **Step 2: Use `opts.repos` in `run_turn` — replace the hardcoded structs**

Make these substitutions inside `run_turn` (the bodies, not tests):

- `GitWorkspaceRepo.create_session_worktree(&opts.workspace, &opts.session_id)?;`
  → `opts.repos.workspace.create_session_worktree(&opts.workspace, &opts.session_id)?;`
- `crate::git::rewind::GitRewindRepo.init(&opts.workspace, &opts.session_id, &opts.codebase_path)?;`
  → `opts.repos.rewind.init(&opts.workspace, &opts.session_id, &opts.codebase_path)?;`
- `let codebase_hash = GitCodebaseRepo.head_hash(&opts.codebase_path);`
  → `let codebase_hash = opts.repos.codebase.head_hash(&opts.codebase_path);`
- `let dirty_files = GitCodebaseRepo.dirty_files(&opts.codebase_path);`
  → `let dirty_files = opts.repos.codebase.dirty_files(&opts.codebase_path);`
- The 5× `GitWorkspaceRepo.commit_session(&opts.workspace, &opts.session_id, ...)?;`
  → `opts.repos.workspace.commit_session(&opts.workspace, &opts.session_id, ...)?;` (all five)
- At the two `checkpoint_codebase(&crate::git::rewind::GitRewindRepo, ...)` call sites:
  → `checkpoint_codebase(opts.repos.rewind.as_ref(), ...)`

The `checkpoint_codebase` helper signature already takes `rewind: &dyn RewindRepo` — no change to the helper. (`as_ref()` on `Arc<dyn RewindRepo>` yields `&dyn RewindRepo`.)

Remove the now-unused `use crate::git::workspace::{WorkspaceRepo, GitWorkspaceRepo};` / `GitCodebaseRepo` imports IF they become unused — but the trait imports (`WorkspaceRepo`, `CodebaseRepo`, `RewindRepo`) must stay in scope for the method calls to resolve. Keep `use crate::git::workspace::WorkspaceRepo;`, `use crate::git::codebase::CodebaseRepo;`, `use crate::git::rewind::RewindRepo;` (trait imports); drop the concrete `Git*Repo` imports only if the compiler flags them unused.

- [ ] **Step 3: Fix `apply_curate_filter` (the internal `run_turn` caller at ~turn.rs:339)**

It constructs a `TurnOpts` and (post-T15) calls `GitRewindRepo.remove(...)`. Build a local production bundle and use it:

At the top of the function body add:
```rust
    let repos = crate::git::Repos::production();
```
Add `repos: repos.clone(),` to its `TurnOpts { ... }` literal. Change the cleanup line `let _ = crate::git::rewind::GitRewindRepo.remove(workspace, &session_id);` to `let _ = repos.rewind.remove(workspace, &session_id);`.

- [ ] **Step 4: Build to list every `TurnOpts` literal missing `repos`**

Run: `cargo build 2>&1 | tail -40`
Expected: "missing field `repos`" at each `TurnOpts { ... }` in `src/turn.rs` tests (~14 sites at lines 1196, 1265, 1320, 1369, 1460, 1507, 1548, 1615, 1683, 1812, 1891, 2006, 2142, 2208 — the compiler is authoritative).

- [ ] **Step 5: Add `repos: crate::git::Repos::production(),` to every test `TurnOpts` literal the compiler flags**

For each flagged test-module `TurnOpts { ... }`, add `repos: crate::git::Repos::production(),`. (Tests use real temp git repos, so production repos are correct here.)

- [ ] **Step 6: Build + test**

Run: `cargo build 2>&1 | tail -10 && cargo test --lib turn:: 2>&1 | tail -15 && cargo test 2>&1 | tail -8`
Expected: compiles; all pass (baseline 384; same count — no tests added/removed yet).

- [ ] **Step 7: Commit**

```bash
git add src/turn.rs
git commit -m "refactor(turn): inject Repos via TurnOpts; run_turn uses opts.repos.*"
```

---

## Task 3: Thread `Repos` into the tool layer (`spawn_agent`)

**Files:** `src/tools/spawn_agent.rs`, `src/tools/mod.rs`

- [ ] **Step 1: Add a `repos` field to `SpawnAgentTool`**

In `src/tools/spawn_agent.rs`, add `repos: crate::git::Repos` to the `struct SpawnAgentTool { ... }` and to `SpawnAgentTool::new(...)`. Update `new`'s signature to accept it last:

```rust
    pub fn new(current_session_id: String, workspace: PathBuf, codebase_path: PathBuf, repos: crate::git::Repos) -> Self {
        Self { current_session_id, workspace, codebase_path, repos, /* ...existing fields... */ }
    }
```
(Match the actual existing field set; just add `repos`.)

- [ ] **Step 2: Use `self.repos` in `execute`**

Replace in `execute`:
- `GitWorkspaceRepo.create_session_worktree(&self.workspace, &new_id)?;` → `self.repos.workspace.create_session_worktree(&self.workspace, &new_id)?;`
- `crate::git::codebase::GitCodebaseRepo.head_hash(&self.codebase_path)` → `self.repos.codebase.head_hash(&self.codebase_path)`
- `GitWorkspaceRepo.commit_session(...)` → `self.repos.workspace.commit_session(...)`
- In the child `TurnOpts { ... }` that spawn_agent builds for the child session, add `repos: self.repos.clone(),`.

Keep trait imports (`WorkspaceRepo`, `CodebaseRepo`) in scope; drop concrete `Git*Repo` imports if unused.

- [ ] **Step 3: `builtin_tools` takes and forwards `repos`**

In `src/tools/mod.rs`, change the signature of `builtin_tools` to accept `repos: crate::git::Repos` (add as the last param):

```rust
pub fn builtin_tools(
    codebase_path: PathBuf,
    session_id: String,
    workspace: PathBuf,
    mcp_tools: Vec<McpToolDef>,
    skills: Vec<SkillDef>,
    repos: crate::git::Repos,
) -> Vec<Box<dyn Tool>> {
```
and update the `SpawnAgentTool::new(...)` line inside to pass `repos.clone()`:
```rust
        SpawnAgentTool::new(session_id.clone(), workspace.clone(), codebase_path.clone(), repos.clone()).boxed(),
```

- [ ] **Step 4: Fix `spawn_agent.rs` tests + the `builtin_tools` test**

`SpawnAgentTool::new(...)` test calls (spawn_agent.rs:140, 181) and the `builtin_tools_returns_fifteen` test (tools/mod.rs:~130) now need a `repos` argument — pass `crate::git::Repos::production()`.

- [ ] **Step 5: Build**

Run: `cargo build 2>&1 | tail -25`
Expected: errors ONLY at the external `builtin_tools(...)` call sites (exec_prompt.rs, bin/gait.rs) — those are fixed in Task 4. Within `tools/`, it compiles. (You may temporarily see the two external call-site errors; that's expected — proceed to Task 4. If you want a green commit here, do Task 4 before committing; otherwise commit tools + the call-site fixes together. To keep tasks atomic, DEFER the commit: do Steps 1-4, confirm `cargo build` shows only the two expected external-call-site errors, then move to Task 4 and commit both together at Task 4 Step 6.)

(No commit in Task 3 — Task 4 fixes the callers and commits tools/ + roots together, because `builtin_tools`'s signature change cannot compile standalone.)

---

## Task 4: Composition roots build and pass `Repos`

**Files:** `src/bin/gait.rs`, `src/workflow/exec_prompt.rs`, `src/workflow/runner.rs`, `src/harness.rs`

- [ ] **Step 1: `src/workflow/exec_prompt.rs`**

Before the `builtin_tools(...)` call (line ~54) and the `TurnOpts { ... }` (line ~59), add `let repos = crate::git::Repos::production();`. Pass `repos.clone()` as the new last arg to `builtin_tools(...)`, and add `repos,` (or `repos: repos,`) to the `TurnOpts { ... }` literal.

- [ ] **Step 2: `src/bin/gait.rs` — `cmd_run` (and `cmd_chat` if it builds tools/TurnOpts)**

At the `builtin_tools(...)` call site and the `TurnOpts { ... }` (line ~348), add `let repos = gait_ai::git::Repos::production();` and pass `repos.clone()` to `builtin_tools(...)` and `repos,` to `TurnOpts { ... }`. (If `cmd_chat` separately constructs tools/TurnOpts, do the same there. Grep `builtin_tools(` and `TurnOpts {` in bin/gait.rs to find all.)

- [ ] **Step 3: `src/bin/gait.rs` — `cmd_finish`, `cmd_rewind`, `init`**

These call repos directly:
- `init` handler: `GitWorkspaceRepo.init_workspace(&workspace)` → build `let repos = gait_ai::git::Repos::production();` then `repos.workspace.init_workspace(&workspace)`.
- `cmd_finish`: `GitWorkspaceRepo.finish_session(...)` + `GitRewindRepo.remove(...)` + `list_active_sessions` → use a local `repos.workspace.*` / `repos.rewind.remove(...)`.
- `cmd_rewind`: the `GitRewindRepo.rewind_to(...)` call → `repos.rewind.rewind_to(...)`; if it lists sessions via `GitWorkspaceRepo.list_active_sessions`, route through `repos.workspace`.
Build `let repos = gait_ai::git::Repos::production();` once near the top of each handler. Keep the trait imports (`use gait_ai::git::workspace::WorkspaceRepo; use gait_ai::git::rewind::RewindRepo;`) in scope; drop now-unused concrete `Git*Repo` imports if the compiler flags them.

- [ ] **Step 4: `src/workflow/runner.rs`**

Near the top of the function(s) doing git ops, add `let repos = crate::git::Repos::production();`. Replace the `GitWorkspaceRepo.create_session_worktree(...)` and all 6 `GitWorkspaceRepo.commit_session(...)` with `repos.workspace.create_session_worktree(...)` / `repos.workspace.commit_session(...)`. If `repos` would be moved into a closure/loop, `.clone()` it as needed (it's cheap — `Arc`s).

- [ ] **Step 5: `src/harness.rs`**

`harness.rs:91` does `GitWorkspaceRepo.commit_session(...)`. Add `let repos = crate::git::Repos::production();` in scope (or, if the harness struct is long-lived and constructed at a composition root, give it a `repos: Repos` field — but a local `production()` is acceptable here since harness is itself near the entry point). Replace with `repos.workspace.commit_session(...)`. Keep `use crate::git::workspace::WorkspaceRepo;`.

- [ ] **Step 6: Build, test, commit (tools/ + roots together)**

Run: `cargo build 2>&1 | tail -25`
Fix any remaining `builtin_tools`/`TurnOpts`/`Git*Repo` errors the compiler reports (there should be none beyond the sites above). Then:
Run: `cargo test 2>&1 | tail -8`
Expected: all pass (baseline 384).

```bash
git add src/tools/spawn_agent.rs src/tools/mod.rs src/workflow/exec_prompt.rs src/workflow/runner.rs src/bin/gait.rs src/harness.rs
git commit -m "refactor: thread Repos through tool layer and composition roots (full DI)"
```

---

## Task 5: Mock-inject wiring test (closes S2) + final verification

**Files:** `src/turn.rs` (test module)

- [ ] **Step 1: Write the run_turn wiring test with an injected `MockRewindRepo`**

Append to `src/turn.rs`'s `#[cfg(test)] mod tests`. Model the turn-driving setup on `run_turn_records_code_checkpoint_for_git_codebase` (separate git codebase + mock provider). The difference: build `Repos` with a `MockRewindRepo` you keep a handle to, and after the turn assert it was called. (Workspace/codebase stay real `Git*` so the worktree + transcript actually work.)

```rust
    #[tokio::test]
    async fn run_turn_calls_rewind_init_and_checkpoint_via_injected_repo() {
        use std::sync::Arc;
        use crate::git::rewind::MockRewindRepo;
        // ... copy the codebase/provider/workspace setup from
        // run_turn_records_code_checkpoint_for_git_codebase ...

        let mock = Arc::new(MockRewindRepo::new());
        let repos = crate::git::Repos {
            codebase: Arc::new(crate::git::codebase::GitCodebaseRepo),
            workspace: Arc::new(crate::git::workspace::GitWorkspaceRepo),
            rewind: mock.clone(), // Arc<MockRewindRepo> coerces to Arc<dyn RewindRepo>
        };

        // Build TurnOpts as in the model test, but set `repos`.
        // run one turn:
        // run_turn(TurnOpts { /* ... */, repos }, provider, tools).await.unwrap();

        // init called exactly once at session start:
        assert_eq!(mock.init_calls().len(), 1, "rewind.init should be called once");
        // checkpoint called for turn 1:
        assert_eq!(mock.checkpoint_calls(), vec![(session_id.to_string(), 1u32)]);
    }
```

Note: `Arc<MockRewindRepo>` coerces to `Arc<dyn RewindRepo>` in the struct field; keep the `mock` clone to read `init_calls()`/`checkpoint_calls()` after the turn. Adapt `session_id` to the actual value used.

- [ ] **Step 2: Run the new test + full suite**

Run: `cargo test --lib turn::tests::run_turn_calls_rewind 2>&1 | tail -15`
Expected: PASS — proving init/checkpoint are invoked through the injected repo (S2 closed). Then `cargo test 2>&1 | tail -8` (baseline 384 + 1 = 385).

- [ ] **Step 3: Commit**

```bash
git add src/turn.rs
git commit -m "test(turn): mock-inject RewindRepo to verify run_turn wiring (closes review S2)"
```

- [ ] **Step 4: Final verification**

(a) No direct `Git*Repo` usage left in production (non-test, non-impl, non-`production()`) code paths that should be injected. Run:
```
grep -rn "GitWorkspaceRepo\.\|GitCodebaseRepo\.\|GitRewindRepo\." src/ | grep -v "test\|Repos::production\|fn production"
```
Expected: only the bodies of `Repos::production()` reference the `Git*Repo` constructors (and those use `Arc::new(GitXxxRepo)`, not method calls, so this grep should be empty or only show test files). Report anything unexpected.

(b) Full suite: `cargo test 2>&1 | grep "test result:"` — report total passing/failing.

(c) Clippy: `cargo clippy 2>&1 | tail -15` — no new errors.

---

## Out of scope

- `MockCodebaseRepo` / `MockWorkspaceRepo` — not added unless a future test needs them (existing tests use real temp repos).
- Changing the trait method signatures — pure injection refactor, behavior-preserving.
- Any non-git dependency injection.

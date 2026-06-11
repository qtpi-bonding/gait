---
status: done
---

# Session + TurnInput (Dissolve TurnOpts) — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Dissolve the `TurnOpts` god-bag into three honest types — a durable `Session` (identity + bindings), the live `ResolvedProfile` (capabilities, from Plan 3), and a per-call `TurnInput` (task/auto/max_iter) — and change `run_turn` to `run_turn(&Session, ResolvedProfile, TurnInput) -> TurnResult`.

**Architecture:** `TurnOpts` (18 fields, 20 construction sites) mixes durable identity (`session_id`, `workspace`, `codebase_path`, `model`, `repos`), resolved capabilities (`allow`, `deny`, `mcp_registry`, `mcp_clients`, `skill_registry`, `config_enabled`, `hooks`, `output_schema_retries`, `system_prompt`), and per-call inputs (`task`, `auto`, `max_iter`). After Plan 3, the capabilities already live on `ResolvedProfile`. This plan moves the durable fields to `Session`, the per-call fields to `TurnInput`, deletes `TurnOpts`, and rewires `run_turn`. `provider` and `tools` (today separate args) move onto `ResolvedProfile`.

**Tech Stack:** Rust, `cargo test`. No new dependencies.

**Scope:** Plan 4 — the trunk. Depends on Plan 3 (`ResolvedProfile` exists). Highest-churn plan (20 `TurnOpts {}` sites: 3 production + 17 tests). A `ResolvedProfile::for_test` constructor keeps the test migration mechanical.

**Deliberate deviation from the spec signature:** the spec wrote `run_turn(&mut Session, &ResolvedProfile, ...)`. Implementation takes `caps: ResolvedProfile` **by value** (not `&`) because `run_turn` owns and mutates the tools vec (it pushes MCP tools during the post-compaction reload loop, and `Box<dyn Tool>` is not `Clone`). `Session` is `&` (not `&mut`) because `run_turn` persists mutable state to disk via `SessionJson`/the `Log`, never to the struct. A fresh `ResolvedProfile` is built per process anyway (re-resolution), so by-value is correct.

---

> ## ⚠️ RE-GROUND CORRECTIONS (2026-06-10, post-Plan-3) — these OVERRIDE the stale numbers below
>
> The body of this plan predates Plans 1–3. Verified against current code, apply these:
>
> **Counts:** `TurnOpts` has **17 fields** (not 18). There are **2 production** run_turn sites + **1 curate rebuild** (turn.rs:342) + **15 turn.rs tests** + **5 integration tests** = the full migration set. ("3 production + 17 tests" is wrong.)
>
> **Locations (current):** `struct TurnOpts` = `turn.rs:23-48`; `impl Default for TurnOpts` = `turn.rs:50-72`; `pub async fn run_turn(opts, provider, tools)` = `turn.rs:383-387`. lib.rs:36 = `pub use turn::{TurnOpts, run_turn, TurnResult}` (no `Session` export yet). curate `TurnOpts{}` rebuild = `turn.rs:342`.
>
> **Production migration is now pure DELETION (simpler than the body's "build then dissolve").** Both `bin/gait.rs:233` (cmd_run) and `exec_prompt.rs:52` already build a `resolved: ResolvedProfile` (Plan 3). Migrate by: delete the `TurnOpts{ …resolved.X.clone()… }` literal + the trailing `provider, tools` args, then call `run_turn(&Session::load(...), resolved, TurnInput{ task, auto, max_iter:None })`, moving `resolved` whole. **`exec_prompt.rs` passes an INJECTED `provider` arg, not `resolved.provider`** — so set `resolved.provider = provider;` before the move (load-bearing; keep). Ignore the body's stale "before" snippets.
>
> **Task 4 test table — renumber (names are correct & in order):** 1219, 1290, 1347, 1398, 1472, 1521, 1564, 1633, 1703, 1834, 1915, 2032, 2170, 2238, 2313. (Test #1 is `run_turn_writes_transcript_and_commits`.) Title "17 test sites" → **15**.
>
> **ADD Task 4b — integration tests (the body MISSES these; Task 5's `cargo test` will fail to build without them):** migrate 5 sites that do `run_turn(TurnOpts{ … ..TurnOpts::default() }, provider, tools)` and import `turn::TurnOpts`:
> - `tests/integration_turn.rs:74, 144, 153`
> - `tests/integration_coordination.rs:78, 156`
> Replace with `Session{…}`/`Session::load` + `ResolvedProfile::for_test(provider, tools, allow, deny)` + `TurnInput{…}`; change `use turn::{TurnOpts,…}` → `use turn::{TurnInput,…}` + `session::Session` + `profile::ResolvedProfile` (all `pub`, reachable from the integration crate).
>
> **Task 5 grep gate — broaden** to `grep -rn "TurnOpts" gait-rs/src gait-rs/tests --include="*.rs"` (the integration tests live in `gait-rs/tests/`, outside `src`, so the body's `src`-only grep misses them).
>
> **profile.rs:** adding the new `hooks` field to `ResolvedProfile` requires `hooks: vec![]` in the `resolve()` literal at **profile.rs:128** (only non-test literal). `ResolvedProfile::for_test` is brand new (no collision).
>
> **Intentional deviations — do NOT "fix":** (a) `output_schema_retries` stays on `ResolvedProfile` (where Plan 3 put it), even though the spec lists it under TurnInput — the body never reads it. (b) No run_turn body site *moves* `opts.repos` (all are `Arc` method calls), so `session.repos.X` needs no `.clone()`.

---

## File Structure

| File | Responsibility after this plan |
|---|---|
| `gait-rs/src/session/session_obj.rs` | **(new)** `Session` (durable runtime bundle) + `Session::load`/`new`. Reads/writes `SessionJson` for `assigned_by`/`codebase_path`/`token_totals`. |
| `gait-rs/src/turn.rs` | `TurnInput` struct; `run_turn(&Session, ResolvedProfile, TurnInput)`; `TurnOpts` + its `Default` **deleted**. Body reads `session.*` / `caps.*` / `input.*`. |
| `gait-rs/src/profile.rs` | `ResolvedProfile` gains `hooks: Vec<Box<dyn Hook>>` + a `for_test(...)` constructor. |
| `gait-rs/src/bin/gait.rs`, `workflow/exec_prompt.rs` | Build `Session` + `TurnInput`; call the new `run_turn`. |
| `gait-rs/src/lib.rs` | Re-export `Session`, `TurnInput`; drop `TurnOpts`. |

Target types:

```rust
// session/session_obj.rs
pub struct Session {
    pub id: String,
    pub workspace: std::path::PathBuf,
    pub codebase_path: std::path::PathBuf,
    pub model: String,
    pub assigned_by: Option<String>,
    pub repos: crate::git::Repos,
}

// turn.rs
pub struct TurnInput {
    pub task: Option<String>,   // None on resume
    pub auto: bool,
    pub max_iter: Option<usize>,
}
```

---

### Task 1: Add `Session` + `TurnInput`; extend `ResolvedProfile`

**Files:**
- Create: `gait-rs/src/session/session_obj.rs`
- Modify: `gait-rs/src/session/mod.rs` (`pub mod session_obj;` + re-export `Session`)
- Modify: `gait-rs/src/turn.rs` (add `TurnInput`)
- Modify: `gait-rs/src/profile.rs` (`ResolvedProfile.hooks` + `for_test`)

- [ ] **Step 1: Create `Session` with a `load` that reads `SessionJson`**

Create `gait-rs/src/session/session_obj.rs`:

```rust
use std::path::PathBuf;
use crate::git::Repos;
use crate::session::session_json::read_session_json;

/// The durable runtime instance: identity + bindings + injected repos.
/// Not the live provider/tools — those are `ResolvedProfile`.
pub struct Session {
    pub id: String,
    pub workspace: PathBuf,
    pub codebase_path: PathBuf,
    pub model: String,
    pub assigned_by: Option<String>,
    pub repos: Repos,
}

impl Session {
    /// Load identity/bindings from `sessions/<id>/session.json` (defaults if absent).
    /// `codebase_path` falls back to the provided default when the json has none.
    pub fn load(
        workspace: PathBuf,
        id: String,
        model: String,
        default_codebase: PathBuf,
        repos: Repos,
    ) -> Self {
        let sj = read_session_json(&workspace, &id);
        let codebase_path = sj.codebase_path
            .map(PathBuf::from)
            .unwrap_or(default_codebase);
        Session { id, workspace, codebase_path, model, assigned_by: sj.assigned_by, repos }
    }
}
```

Add to `gait-rs/src/session/mod.rs`: `pub mod session_obj;` and `pub use session_obj::Session;`.

- [ ] **Step 2: Add `TurnInput` to `turn.rs`**

Add near `TurnResult` in `gait-rs/src/turn.rs`:
```rust
/// Per-call inputs for one turn (the things that vary every invocation).
pub struct TurnInput {
    pub task: Option<String>,   // None on resume
    pub auto: bool,
    pub max_iter: Option<usize>,
}
```

- [ ] **Step 3: Extend `ResolvedProfile` with `hooks` + a test constructor**

In `gait-rs/src/profile.rs`, add `pub hooks: Vec<Box<dyn crate::types::Hook>>,` to `ResolvedProfile` (and set `hooks: vec![]` in `resolve()`). Add:
```rust
impl ResolvedProfile {
    /// Minimal constructor for tests: caller supplies provider + tools + allow/deny;
    /// everything else is empty/default.
    pub fn for_test(
        provider: std::sync::Arc<dyn crate::provider::Provider>,
        tools: Vec<Box<dyn crate::types::Tool>>,
        allow: Vec<String>,
        deny: Vec<String>,
    ) -> Self {
        ResolvedProfile {
            provider, tools, allow, deny,
            model: "test/model".into(),
            instructions: String::new(),
            mcp_registry: vec![],
            mcp_clients: std::collections::HashMap::new(),
            skill_registry: vec![],
            config_enabled: std::collections::HashSet::new(),
            output_schema_retries: 3,
            hooks: vec![],
        }
    }
}
```

- [ ] **Step 4: Write a test for `Session::load`**

Add to `session_obj.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn load_defaults_when_no_session_json() {
        let dir = TempDir::new().unwrap();
        let s = Session::load(
            dir.path().to_path_buf(), "s1".into(), "anthropic/x".into(),
            std::path::PathBuf::from("/code"), Repos::production());
        assert_eq!(s.id, "s1");
        assert_eq!(s.codebase_path, std::path::PathBuf::from("/code"));
        assert!(s.assigned_by.is_none());
    }
}
```

- [ ] **Step 5: Build + test (TurnOpts still exists — both compile side by side)**

Run (from `gait-rs/`):
```bash
cargo test --lib session_obj
cargo build
```
Expected: PASS / builds (nothing uses the new types yet; `TurnOpts` still present).

- [ ] **Step 6: Commit**

```bash
git add gait-rs/src/session/session_obj.rs gait-rs/src/session/mod.rs gait-rs/src/turn.rs gait-rs/src/profile.rs
git commit -m "feat: add Session + TurnInput + ResolvedProfile::for_test (TurnOpts still present)"
```

---

### Task 2: Re-sign `run_turn` and rewrite its body to use Session/caps/input

**Files:**
- Modify: `gait-rs/src/turn.rs` (`run_turn` signature + body field references)

- [ ] **Step 1: Change the signature**

Replace:
```rust
pub async fn run_turn(
    opts: TurnOpts,
    provider: Arc<dyn Provider>,
    tools: Vec<Box<dyn Tool>>,
) -> anyhow::Result<TurnResult>
```
with:
```rust
pub async fn run_turn(
    session: &crate::session::Session,
    caps: crate::profile::ResolvedProfile,
    input: TurnInput,
) -> anyhow::Result<TurnResult>
```

At the top of the body, destructure into the names the existing body already uses, so the rest of the function needs minimal edits:
```rust
let provider = caps.provider.clone();
let mut tools = caps.tools;          // owned + mutable (MCP reload loop pushes into it)
let hooks = caps.hooks;
```

- [ ] **Step 2: Apply the field-reference mapping throughout the body**

Replace every `opts.<field>` reference in `run_turn`'s body using this table (these are the only references that change):

| Old reference | New reference |
|---|---|
| `opts.session_id` | `session.id` |
| `opts.workspace` | `session.workspace` |
| `opts.codebase_path` | `session.codebase_path` |
| `opts.model` | `session.model` |
| `opts.repos` | `session.repos` |
| `opts.task` | `input.task` |
| `opts.auto` | `input.auto` |
| `opts.max_iter` | `input.max_iter` |
| `opts.allow` | `caps.allow` |
| `opts.deny` | `caps.deny` |
| `opts.system_prompt` | `caps.instructions` |
| `opts.mcp_registry` | `caps.mcp_registry` |
| `opts.mcp_clients` | `caps.mcp_clients` |
| `opts.skill_registry` | `caps.skill_registry` |
| `opts.config_enabled` | `caps.config_enabled` |
| `opts.output_schema_retries` | `caps.output_schema_retries` |
| `opts.hooks` | `hooks` (destructured above) |

`provider` and `tools` are already in scope from Step 1, so existing `provider`/`tools` usages are unchanged. Note: `session.repos` is `Repos` (Clone); where the old code moved `opts.repos`, use `session.repos.clone()`.

- [ ] **Step 3: Fix the `apply_curate_filter` recursive call (still inside turn.rs)**

`apply_curate_filter` (turn.rs ~349) currently builds a `TurnOpts` and calls `run_turn(opts, provider, vec![AnnotateTool...])`. Rewrite it to the new shape:
```rust
let session = crate::session::Session {
    id: session_id,                       // the generated curate session id
    workspace: workspace.to_path_buf(),
    codebase_path: codebase_path.to_path_buf(),
    model: model.to_string(),
    assigned_by: None,
    repos: repos.clone(),
};
let caps = crate::profile::ResolvedProfile {
    provider: provider.clone(),
    tools: vec![AnnotateTool::new().boxed()],
    model: model.to_string(),
    instructions: CURATE_SYSTEM_PROMPT.to_string(),
    allow: vec![], deny: vec![],
    mcp_registry: vec![], mcp_clients: std::collections::HashMap::new(),
    skill_registry: vec![], config_enabled: std::collections::HashSet::new(),
    output_schema_retries: 3, hooks: vec![],
};
run_turn(&session, caps, TurnInput { task: Some(task), auto: true, max_iter: Some(1) }).await
```

- [ ] **Step 4: Update `lib.rs` exports**

In `gait-rs/src/lib.rs`, change `pub use turn::{TurnOpts, run_turn, TurnResult};` → `pub use turn::{TurnInput, run_turn, TurnResult};` and add `pub use session::Session;` if not already present.

- [ ] **Step 5: Do NOT delete `TurnOpts` yet**

Leave the `TurnOpts` struct + `Default` in place for now so callers still compile incrementally — Task 5 deletes it once all call sites are migrated. The crate will not fully build until Tasks 3–4 are done; that is expected mid-refactor.

- [ ] **Step 6: Commit (compile may be red until Task 4)**

```bash
git add gait-rs/src/turn.rs gait-rs/src/lib.rs
git commit -m "refactor: run_turn(&Session, ResolvedProfile, TurnInput); body reads session/caps/input"
```

---

### Task 3: Migrate the two production call sites

**Files:**
- Modify: `gait-rs/src/bin/gait.rs` (cmd_run), `gait-rs/src/workflow/exec_prompt.rs`

- [ ] **Step 1: `bin/gait.rs` — build Session + TurnInput, call new run_turn**

Replace the `run_turn(TurnOpts { ... }, resolved.provider.clone(), resolved.tools)` block (from Plan 3 Task 4) with:
```rust
let session = gait_ai::session::Session::load(
    workspace.clone(), session_id.clone(), resolved.model.clone(),
    codebase_path.clone(), repos.clone());
let turn_result = match run_turn(
    &session,
    resolved,                                   // moves the ResolvedProfile (provider+tools inside)
    gait_ai::turn::TurnInput { task, auto, max_iter: None },
).await {
    Ok(r) => r,
    Err(e) => { eprintln!("[gait] error: {}", e); std::process::exit(1); }
};
```
(`resolved` already carries `allow`/`deny`/`instructions`/`mcp_*`/`config_enabled`/`output_schema_retries`/`provider`/`tools`, so the old per-field `TurnOpts` mapping disappears entirely.)

- [ ] **Step 2: `workflow/exec_prompt.rs` — same shape**

Replace its `run_turn(TurnOpts { ... }, provider, resolved.tools)` with a `Session::load` + `TurnInput { task: Some(full_task), auto, max_iter: None }`. If the workflow must reuse its injected provider (Plan 3 Task 5 note), set `resolved.provider = provider;` before the call so the moved `resolved` carries it.

- [ ] **Step 3: Build**

Run (from `gait-rs/`):
```bash
cargo build
```
Expected: the two binaries/lib compile (test code may still reference `TurnOpts` — Task 4).

- [ ] **Step 4: Commit**

```bash
git add gait-rs/src/bin/gait.rs gait-rs/src/workflow/exec_prompt.rs
git commit -m "refactor: production call sites use Session + ResolvedProfile + TurnInput"
```

---

### Task 4: Migrate the 17 test sites in `turn.rs`

**Files:**
- Modify: `gait-rs/src/turn.rs` (`#[cfg(test)]` module)

- [ ] **Step 1: Add a test helper at the top of the test module**

```rust
fn run_turn_test_call(
    workspace: std::path::PathBuf,
    codebase_path: std::path::PathBuf,
    session_id: String,
    provider: std::sync::Arc<dyn Provider>,
    tools: Vec<Box<dyn Tool>>,
    allow: Vec<String>,
    deny: Vec<String>,
    input: TurnInput,
    repos: crate::git::Repos,
) -> impl std::future::Future<Output = anyhow::Result<TurnResult>> {
    let session = crate::session::Session {
        id: session_id, workspace, codebase_path,
        model: "test/model".into(), assigned_by: None, repos,
    };
    let caps = crate::profile::ResolvedProfile::for_test(provider, tools, allow, deny);
    run_turn_owned(session, caps, input)
}

async fn run_turn_owned(s: crate::session::Session, c: crate::profile::ResolvedProfile, i: TurnInput)
    -> anyhow::Result<TurnResult> { run_turn(&s, c, i).await }
```

- [ ] **Step 2: Convert each of the 17 test sites**

Each old site is `run_turn(TurnOpts { <fields>, ..Default::default() }, provider, tools).await`. Convert to `run_turn_test_call(workspace, codebase_path, session_id, provider, tools, allow, deny, TurnInput { task, auto, max_iter }, repos).await`, carrying that site's distinguishing fields. The 17 sites and their distinguishing fields (verbatim from the field audit):

| turn.rs line | test | allow / deny | task | auto | repos |
|---|---|---|---|---|---|
| 1246 | `run_turn_writes_transcript_and_git_commit` | allow=`["read"]` | Some | — | prod |
| 1317 | `output_tool_result_used_in_turn_complete` | allow=`[]` | Some | true | prod |
| 1374 | `output_tool_exempt_from_permission_gating` | — | Some | false | prod |
| 1425 | `permission_gating_pauses_turn_in_non_auto_mode` | allow=`["read"]` | Some | false | prod |
| 1518 | `run_turn_returns_done_result_on_completion` | — | Some | true | prod |
| 1567 | `run_turn_returns_permissions_when_paused` | — | Some | false | prod |
| 1610 | `streaming_text_does_not_produce_extra_transcript_events` | — | Some | true | prod |
| 1679 | `denied_tool_gets_blocked_result_without_prompting` | deny=`["shell:rm *"]` | Some | false | prod |
| 1749 | `deny_wins_when_tool_in_both_allow_and_deny` | allow & deny=`["shell:rm *"]` | Some | — | prod |
| 1880 | `resume_executes_approved_tool_directly` | allow=`["shell:echo *"]` | None (resume) | — | prod |
| 1961 | `resume_emits_denied_tool_result_for_rejected_call` | — | None (resume) | — | prod |
| 2078 | `resume_with_scope_agent_shows_whitelist_prefix` | — | None (resume) | — | prod |
| 2216 | `run_turn_records_code_checkpoint_for_git_codebase` | — | Some | — | prod (git codebase dir) |
| 2284 | `run_turn_persists_codebase_path_in_session_json` | — | Some | — | prod (git codebase dir) |
| 2359 | `run_turn_calls_rewind_init_and_checkpoint_via_injected_repo` | — | Some | — | **custom `Repos { checkpoints: mock }`** |

(The 1996 site is a `reconstruct`/`retrace` call, not a `run_turn`, already handled in Plan 1.) For site 2359, pass the custom `Repos` (with `MockCheckpointRepo`) as the `repos` arg to `run_turn_test_call` — the helper threads it into `Session.repos`, preserving exactly what that test asserts (injected checkpoint repo gets `init`+`checkpoint` called). For sites that previously relied on `system_prompt`/`model`/`config_enabled` defaults, `for_test` supplies empty/`"test/model"` — assert behavior, not those fields (none of these tests assert on model/system_prompt).

- [ ] **Step 3: Run the full library test suite**

Run (from `gait-rs/`):
```bash
cargo test --lib
```
Expected: PASS — all 17 migrated `run_turn` tests plus everything else. If `run_turn_persists_codebase_path_in_session_json` (2284) fails, confirm `Session::load`/`run_turn` still writes `codebase_path` to `session.json` (behavior must be preserved).

- [ ] **Step 4: Commit**

```bash
git add gait-rs/src/turn.rs
git commit -m "refactor: migrate run_turn tests to Session + ResolvedProfile::for_test + TurnInput"
```

---

### Task 5: Delete `TurnOpts`

**Files:**
- Modify: `gait-rs/src/turn.rs` (delete `TurnOpts` struct + `Default` impl)

- [ ] **Step 1: Confirm no references remain**

Run (from repo root):
```bash
grep -rn "TurnOpts" gait-rs/src --include="*.rs"
```
Expected: **no output**. If any remain, migrate them (same pattern).

- [ ] **Step 2: Delete the struct + Default impl**

Delete `pub struct TurnOpts { ... }` (turn.rs:22-48) and `impl Default for TurnOpts { ... }` (turn.rs:50-72).

- [ ] **Step 3: Full build + test**

Run (from `gait-rs/`):
```bash
cargo build && cargo test
```
Expected: builds clean; full suite PASS.

- [ ] **Step 4: Commit**

```bash
git add gait-rs/src/turn.rs
git commit -m "refactor: delete TurnOpts (dissolved into Session + ResolvedProfile + TurnInput)"
```

---

## Self-Review

**Spec coverage** (against the `Session` / `ResolvedProfile` / migration sections):
- *"split the durable token from the resolved capabilities"* → `Session` (durable) + `ResolvedProfile` (live) + `TurnInput` (per-call); `TurnOpts` deleted. ✓
- *"run_turn(session, caps, input)"* → new signature (by-value `caps`, `&Session` — deviation documented above). ✓
- *"validated by the curate filter which already runs caps without a durable identity"* → Task 2 Step 3 rebuilds curate as a `Session` + minimal `ResolvedProfile` + `TurnInput`. ✓
- *field buckets* → the Task 2 mapping table assigns every one of the 18 `TurnOpts` fields to `session.*` / `caps.*` / `input.*`. ✓

**Placeholder scan:** No TBD/TODO. The 17-site migration is a table of real sites + distinguishing fields + a uniform transformation via `run_turn_test_call` — precise, not a placeholder. `grep` gate in Task 5 guarantees completeness.

**Type/name consistency:** `Session { id, workspace, codebase_path, model, assigned_by, repos }`, `TurnInput { task, auto, max_iter }`, `ResolvedProfile::for_test(provider, tools, allow, deny)` used identically in the helper, the production sites, and the curate rebuild. The field-mapping table is the single source of truth for the body rewrite.

**Risk:** Highest-churn plan. Mitigations: (1) the mapping table makes the body rewrite mechanical; (2) `for_test` + `run_turn_test_call` make the 17 test conversions uniform; (3) the `grep "TurnOpts"` gate before deletion guarantees no straggler; (4) the mock-repo test (2359) is explicitly preserved by threading a custom `Repos` through the helper. The by-value `caps` deviation is documented and is the only signature difference from the spec.

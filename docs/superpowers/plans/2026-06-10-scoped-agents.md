---
status: draft
---

# Scoped Agents

**Date:** 2026-06-10  
**Status:** Draft v3

## Problem

`spawn_agent` today passes the same `codebase_path` to every child. N concurrent agents
editing N disjoint file sets is safe in practice, but nothing enforces disjointness, the
checkpoint snapshots the whole tree (absorbing sibling edits), and scoped undo doesn't exist.
The result: parallel subagents are possible but the audit trail can lie.

## Goal

N agents, N declared file scopes, running simultaneously with no coordination at finish.
Each agent's checkpoint, undo, and audit history reflects only its own scope.

---

## Core Concept: Agent Scope

A scope is an array of subtree paths declared at spawn time:

```json
{ "task": "refactor parser", "scope": ["src/parser/", "tests/parser/"] }
```

**Subtree-shaped, not globs.** `src/parser/` maps cleanly to Landlock rules, git pathspecs,
and path-component checks. Globs like `**/*_test.rs` don't map to Landlock rules — constrain
the syntax now. Canonicalize at spawn and reject scopes containing symlinks (git's pathspec
view and Landlock's resolved-path view disagree across symlinks).

**Two sets derived from scope:**
- **Enforcement scope** — declared paths + standard writable infra (see below). What the
  process is allowed to write.
- **Snapshot scope** — declared paths only. What gets committed to checkpoint history.

**Unscoped agents** have implicit scope `["."]` for disjointness purposes — they conflict
with everything and cannot run concurrently with any scoped sibling. This includes the
parent: a parent spawning scoped children while continuing to edit is itself an unscoped
concurrent writer; its write tools go behind the same conflict check until children finish,
or it declares its own disjoint scope.

**Grandchild containment:** a child's declared scope must be a subtree of its parent's
scope. Landlock enforces this for free — stacked rulesets only ever tighten.

---

## Shared helper: `path_within(scope, target)`

Used in both disjointness checks and the tier-0 gate — factor once, use everywhere so they
can't drift.

```rust
/// Returns true if `target` is within `scope` (or equal to it).
/// Both paths must be canonicalized. Comparison is component-wise to avoid
/// prefix bugs (`src/parser` must not match `src/parserutil/x.rs`) and
/// traversal bugs (`src/parser/../../outside.rs` must not match `src/parser/`).
fn path_within(scope: &Path, target: &Path) -> bool {
    target.starts_with(scope)  // Path::starts_with is component-wise in std
}
```

`Path::starts_with` in Rust's std is already component-wise (not string-prefix), so
`Path::new("src/parser").starts_with("src/parserutil")` is false. The requirement is that
both sides are canonicalized before the call so `..` components are resolved.

---

## Changes Required

### 1. `spawn_agent` — scope parameter + disjointness check + pre-create dirs

```rust
struct SpawnAgentArgs {
    task: String,
    scope: Option<Vec<String>>,   // subtree paths, e.g. ["src/parser/"]
    model: Option<String>,
    context_files: Option<Vec<String>>,
}
```

At spawn time, under a workspace-level lock file (prevents TOCTOU when two sessions
spawn concurrently):
1. Canonicalize all scope paths; reject any containing symlinks.
2. **Pre-create declared scope directories** (empty, if they don't exist). `PathFd::new`
   in the Landlock setup requires the path to exist when the ruleset is built — an agent
   whose task is to create `src/newmodule/` would fail at spawn otherwise. Empty dirs are
   invisible to git (not committed), so no checkpoint pollution.
3. Load active sessions from trace repo (`list_active_sessions`).
4. For each active sibling, read its `session.json` scope (unscoped = implicit `["."]`).
5. Check disjointness using `path_within` — if any scope path overlaps a sibling's → error.
6. Write new `session.json` with scope before releasing lock.

### 2. `session.json` — store scope

```rust
pub struct SessionJson {
    pub id: String,
    pub assigned_by: Option<String>,
    pub created_at: u64,
    pub token_totals: TokenTotals,
    pub codebase_path: Option<String>,
    pub agent_name: Option<String>,
    pub system_prompt: Option<String>,
    pub scope: Option<Vec<String>>,   // ← new
}
```

**Scope reservation release:** a scope is held until `finish`, `gait abort <session>`, or
staleness detection. Without a release story, a crashed child holds its scope forever —
the parent's write tools stay locked, no future agent can spawn into that subtree. Required:
- `gait abort <session>` — explicit release; removes session from active list, releases scope.
- Staleness detection — session lock file stores PID; at disjointness check time, if the
  lock file's PID is dead, treat the session as released. Define "dead" precisely: process
  gone OR lock file older than a configurable TTL.

### 3. Scope gate — tier 0, before deny/allow

**Do not reuse the deny list for scope enforcement.** Deny is tier 1, a hard floor that
fires even in `--auto`. A deny catch-all would block all writes including in-scope ones.

Add a dedicated scope gate using `path_within`:

```rust
// Tier 0: scope gate — only applies when session has a declared scope
if let Some(scope) = &session_scope {
    // Canonicalize the requested path before checking — resolves any .. traversal
    let requested = PathBuf::from(&req.display);
    let canonical = codebase.join(&requested).canonicalize()
        .unwrap_or_else(|_| codebase.join(&requested)); // pre-create ensures this works
    let in_scope = scope.iter()
        .any(|s| path_within(&codebase.join(s), &canonical));
    if !in_scope {
        denied_ids.insert(tc.tool_call_id.clone());
        // tool result: "outside session scope: <path>"
        continue;
    }
}
// Tier 1: deny list (unchanged)
// Tier 2: allow list (unchanged)
// Tier 3: prompt (unchanged)
```

Applies to `write`, `edit`, `multiedit`. Shell covered by kernel enforcement.

### 4. Checkpoint — pathspec-limited snapshot

```rust
// Unscoped (unchanged):
index.add_all(["."].iter(), IndexAddOption::DEFAULT, callback)?;

// Scoped:
index.add_all(scope_paths.iter(), IndexAddOption::DEFAULT, callback)?;
```

### 5. Scoped restore — checkout_tree + post-restore checkpoint

The current `restore_to` uses `reset --hard` — correct for unscoped. Keep it.

For scoped sessions, `reset --hard` would touch files outside scope. Use `checkout_tree`
with `FORCE` instead:

```rust
// Scoped restore:
let mut checkout_opts = CheckoutBuilder::new();
for p in &scope_paths { checkout_opts.path(p); }
checkout_opts.force();
// FORCE diffs target vs HEAD baseline: removes tracked files present in baseline
// but absent in target (the tracked ghost file case).
shadow.checkout_tree(&obj, Some(&mut checkout_opts))?;

// Clean genuinely untracked files within scope (crash case: written mid-turn
// before checkpoint ran, so not in HEAD baseline).
remove_untracked_in_scope(&shadow, codebase, &scope_paths)?;

// CRITICAL: advance HEAD to match restored state.
// Without this, consecutive undos use stale HEAD as baseline, resurrecting ghosts.
// The post-restore checkpoint also creates an honest audit event for the undo.
let post_restore_sha = checkpoint(workspace, session_id, codebase, turn)?;
// caller emits this SHA to transcript
```

**Why the post-restore checkpoint is required:** `checkout_tree` does not move HEAD. On
a consecutive undo (e.g. undo to turn 4, then undo to turn 3), FORCE diffs target-3 vs
baseline-HEAD. If HEAD is still at turn 7, a file present in turn 4 but absent from both
turns 3 and 7 is untracked relative to baseline — FORCE leaves it, ghost survives. Moving
HEAD via a post-restore commit makes baseline match reality and turns the undo into an
honest append-only event rather than an invisible mutation.

### 6. Kernel enforcement

**Sandbox trait — inject like CheckpointRepo:**

```rust
pub trait Sandbox: Send + Sync {
    /// Apply enforcement before the turn loop. Called once at process start.
    fn apply(&self, enforcement_scope: &[PathBuf]) -> anyhow::Result<SandboxStatus>;
}

pub struct LandlockSandbox;   // Linux — landlock crate
pub struct SeatbeltSandbox;   // macOS — sandbox-exec subprocess
pub struct NoopSandbox;       // Windows / fallback / tests
```

**Linux — `landlock` crate (kernel 5.19+ recommended, 5.13 minimum):**

Use the `landlock` crate (maintained by Mickaël Salaün, the Landlock kernel author).
Same reasoning as using `git2` instead of shelling out to git — type safety and automatic
ABI version negotiation.

```rust
fn apply_landlock(enforcement_scope: &[PathBuf]) -> anyhow::Result<SandboxStatus> {
    // Enumerate ONLY write-type access rights in handle_access.
    // handle_access governs everything listed — reads must NOT be listed or they
    // become governed and are denied everywhere outside scope.
    let write_rights = AccessFs::WriteFile | AccessFs::RemoveFile
        | AccessFs::RemoveDir | AccessFs::MakeDir | AccessFs::MakeReg
        | AccessFs::MakeSymlink | AccessFs::MakeSock | AccessFs::MakeFifo
        | AccessFs::MakeBlock | AccessFs::MakeChar;
    // Truncate is ABI 4+ (kernel 6.2+); landlock crate negotiates automatically.
    // On kernels 5.13-6.1, truncate operations escape enforcement — document this gap.

    // Minimum recommended ABI: 2 (kernel 5.19) for REFER (cross-directory moves).
    // On ABI 1 (5.13-5.18), moving files between directories fails even inside scope.
    // Fall back to NoopSandbox below ABI 2 and emit KernelEnforcementUnavailable.

    let status = Ruleset::default()
        .handle_access(write_rights)?   // only write rights — reads ungoverned
        .create()?
        .add_rules(enforcement_scope.iter().map(|p| {
            Ok(PathBeneath::new(PathFd::new(p)?, write_rights))
        }))?
        .restrict_self()?;
    Ok(SandboxStatus::from(status))
}
```

**macOS — shell out to `sandbox-exec`:**

`sandbox-exec` ships at `/usr/bin/sandbox-exec`. No maintained Rust crate binds the
underlying Seatbelt C API (`sandbox_init`) — unlike libgit2, the C API is deprecated with
no stable library interface. Shelling out to the binary is the more stable surface.

```rust
// Instead of: Command::new("gait").args([...]).spawn()
// Wrap with:
Command::new("/usr/bin/sandbox-exec")
    .arg("-p").arg(generate_seatbelt_profile(&enforcement_scope))
    .arg("gait").args(["run", "--session", &id, "--workspace", ws])
    .spawn()?;
```

`sandbox-exec` has been deprecated since macOS Sierra (2016) but remains in use by
Chrome, Firefox, and others. Apple's official replacement (App Sandbox) requires code
signing — not viable for a CLI tool. Accept the deprecation; revisit if Apple removes it.

**Windows:** No equivalent exists. Job Objects don't provide per-path write restriction
without ACL changes that affect all processes. Out of scope.

**Enforcement scope** (what the process may write):
- Declared scope paths
- `sessions/<id>/` — transcript, lock, tmp
- `checkpoints/<id>/codebase.git/` — shadow db (checkpoint runs inside child process)
- Codebase `.git/` — `git status`/`git diff` via shell require `index.lock` write access.
  Make writable. **Note:** Landlock covers *worktree* writes regardless — `git checkout`
  or `git stash` invoked by a scoped agent gets its out-of-scope worktree writes blocked at
  the syscall level even though `.git/` itself is open. This is the correct behavior and
  is not a gap; don't "fix" it by making `.git/` read-only or `git status` breaks.
- `/tmp`
- `~/.cargo`, `~/.npm`, etc. — language toolchain caches

**Fallback (kernel enforcement unavailable):**
- Emit `KernelEnforcementUnavailable` transcript event (not just stderr). Audit honesty:
  a run without kernel enforcement is one a user may later want to trust or distrust.
- In `--auto` mode: require `--no-kernel-enforcement` flag. Silent downgrade in headless
  mode is how guarantees rot.
- Tier 0 scope gate and pathspec checkpoint still apply regardless.

**MCP servers:** spawned MCP servers (`--spawn`) inherit the Landlock domain; `--connect`
servers run outside it entirely. This is a known gap — document in "What this is NOT."

### 7. `gait run` — scope propagation

When `gait run --session <id>` starts:
1. Read `session.json` for scope.
2. If scope present: apply sandbox (Landlock/Seatbelt/Noop via injected `Sandbox` trait).
3. Pass scope to `TurnOpts` → tier 0 gate, checkpoint, and restore use it.

`--no-kernel-enforcement` flag: acknowledged here; required in `--auto` when kernel
enforcement is unavailable and scope is declared.

---

## Interaction with existing features

**`gait undo --code`** — calls `restore_to`. Scoped sessions get scoped restore +
post-restore checkpoint. Unscoped unchanged.

**`gait finish`** — unchanged. Scoped agents edited their subtree directly.

**`gait abort <session>`** — new command. Releases scope reservation, marks session
dead in trace repo. Required for the scope release story.

**`--auto` mode** — scoped + kernel enforcement is the correctness story for headless
parallel agents. Requires kernel enforcement or `--no-kernel-enforcement`.

**Workflows** — `scope:` on a prompt node passes through to `spawn_agent`. Disjoint
parallel nodes supported. Node scope must be ⊆ workflow scope.

---

## What this is NOT

- Not per-session codebase working trees. Agents edit the shared directory, constrained
  to scope. No merge step.
- Not a security sandbox for untrusted code. `--container` remains the recommendation
  for adversarial inputs.
- Not required for single-agent use. Unscoped sessions work exactly as today.
- `--connect` MCP servers run outside the Landlock domain — their filesystem writes
  are not scope-enforced.

---

## Implementation order

1. `session.json` scope field + `spawn_agent` scope parameter + workspace spawn lock +
   pre-create scope dirs + `gait abort` command
2. Pathspec checkpoint + scoped restore + post-restore checkpoint  
   *(tests first — this is the one failure mode that lies silently)*
3. Disjointness check + staleness detection (unscoped = implicit `["."]`)
4. **Scope gate semantics decision on paper** — confirm tier 0 + `path_within` helper,
   verify it doesn't affect existing deny/allow behavior before touching permission code
5. Tier 0 scope gate in `turn.rs`
6. Landlock (Linux) — `landlock` crate, minimum ABI 2
7. Seatbelt (macOS) — `sandbox-exec` subprocess wrapper

Steps 1–5 are the correctness story. Steps 6–7 are the shell-enforcement story.
Each step is independently shippable.

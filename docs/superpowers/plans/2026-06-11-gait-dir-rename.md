---
status: done
---

# `workspace` → `gait_dir` Rename + Resolution Logic

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Rename every internal use of `workspace` to `gait_dir` so the code matches the CLI flag `--gait-dir`, then tighten resolution: `--gait-dir` > `$GAIT_DIR` env var > error (no silent default to cwd), while `--codebase` > pwd.

**Architecture:** Pure mechanical rename of ~320 occurrences across 25 files, then a small logic change to resolution sites in `bin/gait.rs`. Struct fields first (compiler guides the rest), then help strings, then resolution logic. Several user-facing string tokens must NOT be renamed — see the preserved tokens section below.

**Tech Stack:** Rust, `cargo test` for verification.

---

## Preserved tokens — do NOT rename these

These look like `workspace` but are user-facing or env-facing strings that external callers depend on:

| Token | Location | Why preserved |
|-------|----------|---------------|
| `$WORKSPACE` | `workflow/vars.rs:69` | Template token substituted in workflow YAML — user-authored scripts reference it |
| `"workspace"` | `profile.rs:199`, `exec_shell.rs:16` | Config field value (`cwd: "workspace"`) — user-facing config option |
| `"WORKSPACE"` | `workflow/runner.rs` (env key) | Shell env var exported to workflow node processes |

Only Rust identifiers (variable names, field names, parameter names) are renamed. String literals in the above table stay as-is.

---

## Files changed

| File | Change |
|------|--------|
| `gait-rs/src/session/session_obj.rs` | `Session.workspace` → `Session.gait_dir` |
| `gait-rs/src/harness.rs` | `NativeHarnessBackend.workspace` → `.gait_dir` |
| `gait-rs/src/tools/shell.rs` | `ShellTool.workspace` → `.gait_dir` |
| `gait-rs/src/tools/spawn_agent.rs` | `SpawnAgentTool.workspace` → `.gait_dir` |
| `gait-rs/src/tools/list_agents.rs` | `ListAgentsTool.workspace` → `.gait_dir` |
| `gait-rs/src/workflow/vars.rs` | `WorkflowVars.workspace` → `.gait_dir` (keep `$WORKSPACE` token) |
| `gait-rs/src/workflow/runner.rs` | `WorkflowRunner.workspace` → `.gait_dir` (keep `"WORKSPACE"` env key) |
| `gait-rs/src/workflow/exec_shell.rs` | struct field + `"workspace"` cwd-check string stays |
| `gait-rs/src/workflow/exec_prompt.rs` | `ctx.workspace` → `ctx.gait_dir`, function params |
| `gait-rs/src/workflow/exec_loop.rs` | `workspace: ctx.workspace` → `gait_dir: ctx.gait_dir` |
| `gait-rs/src/tools/mod.rs` | `builtin_tools()` param rename |
| `gait-rs/src/profile.rs` | `resolve()` param rename (keep `"workspace"` cwd string) |
| `gait-rs/src/turn.rs` | local vars + params |
| `gait-rs/src/chat.rs` | local vars |
| `gait-rs/src/config.rs` | function params |
| `gait-rs/src/permission.rs` | function params |
| `gait-rs/src/agent_frontmatter.rs` | function params |
| `gait-rs/src/docker.rs` | function params |
| `gait-rs/src/git/trace.rs` | method params (8 methods) |
| `gait-rs/src/git/checkpoint.rs` | method params |
| `gait-rs/src/git/mod.rs` | helper function param |
| `gait-rs/src/session/session_json.rs` | function params |
| `gait-rs/src/workflow/progress.rs` | function params |
| `gait-rs/src/bin/gait.rs` | local vars + help strings + error messages |

---

## Task 1: Rename struct fields

Start with struct fields — the compiler will flag every construction and field access site, guiding Task 2.

**Files:**
- Modify: `gait-rs/src/session/session_obj.rs`
- Modify: `gait-rs/src/harness.rs`
- Modify: `gait-rs/src/tools/shell.rs`
- Modify: `gait-rs/src/tools/spawn_agent.rs`
- Modify: `gait-rs/src/tools/list_agents.rs`
- Modify: `gait-rs/src/workflow/vars.rs`
- Modify: `gait-rs/src/workflow/runner.rs`
- Modify: `gait-rs/src/workflow/exec_shell.rs`

- [ ] **Step 1: Rename `Session.workspace`**

In `gait-rs/src/session/session_obj.rs`, change:
```rust
pub struct Session {
    pub id: String,
    pub workspace: PathBuf,
    pub codebase_path: PathBuf,
    pub assigned_by: Option<String>,
    pub repos: Repos,
}
```
to:
```rust
pub struct Session {
    pub id: String,
    pub gait_dir: PathBuf,
    pub codebase_path: PathBuf,
    pub assigned_by: Option<String>,
    pub repos: Repos,
}
```

Also update the `load()` constructor:
```rust
pub fn load(
    gait_dir: PathBuf,
    id: String,
    default_codebase: PathBuf,
    repos: Repos,
) -> Self {
    let sj = read_session_json(&gait_dir, &id);
    let codebase_path = sj.codebase_path
        .map(PathBuf::from)
        .unwrap_or(default_codebase);
    Session { id, gait_dir, codebase_path, assigned_by: sj.assigned_by, repos }
}
```

- [ ] **Step 2: Rename `NativeHarnessBackend.workspace`**

In `gait-rs/src/harness.rs`:
```rust
pub struct NativeHarnessBackend {
    gait_dir: PathBuf,
}

impl NativeHarnessBackend {
    pub fn new(gait_dir: PathBuf) -> Self {
        Self { gait_dir }
    }
}
```

Update every `self.workspace` inside the impl to `self.gait_dir`.

- [ ] **Step 3: Rename tool struct fields**

In `gait-rs/src/tools/shell.rs`:
```rust
pub struct ShellTool {
    codebase_path: Arc<PathBuf>,
    gait_dir: Arc<PathBuf>,
}

impl ShellTool {
    pub fn new(codebase_path: PathBuf, gait_dir: PathBuf) -> Self {
        Self {
            codebase_path: Arc::new(codebase_path),
            gait_dir: Arc::new(gait_dir),
        }
    }
}
```

Update all `self.workspace` → `self.gait_dir` inside the impl.

In `gait-rs/src/tools/spawn_agent.rs`:
```rust
pub struct SpawnAgentTool {
    current_session_id: String,
    gait_dir: PathBuf,
    codebase_path: PathBuf,
    repos: crate::git::Repos,
}

impl SpawnAgentTool {
    pub fn new(current_session_id: String, gait_dir: PathBuf, codebase_path: PathBuf, repos: crate::git::Repos) -> Self {
        Self { current_session_id, gait_dir, codebase_path, repos }
    }
}
```

In `gait-rs/src/tools/list_agents.rs`:
```rust
pub struct ListAgentsTool {
    gait_dir: PathBuf,
}

impl ListAgentsTool {
    pub fn new(gait_dir: PathBuf) -> Self { Self { gait_dir } }
}
```

- [ ] **Step 4: Rename workflow struct fields**

In `gait-rs/src/workflow/vars.rs`, rename the Rust field only — the `$WORKSPACE` template token on line 69 stays unchanged:
```rust
pub struct WorkflowVars {   // or VarContext — use the actual struct name in the file
    pub session_id: String,
    pub gait_dir: PathBuf,    // was: workspace
    // ... other fields
}
```

The substitution line becomes:
```rust
("$WORKSPACE", ctx.gait_dir.to_string_lossy().to_string()),
//  ^^^^^^^^^^^ string key preserved — only ctx.workspace → ctx.gait_dir
```

In `gait-rs/src/workflow/runner.rs`, rename the field and all `self.workspace` usages to `self.gait_dir`. The env-key string `"WORKSPACE"` that runner exports to shell processes must remain unchanged:
```rust
env.insert("WORKSPACE".into(), self.gait_dir.to_string_lossy().to_string());
//          ^^^^^^^^^^^ string key preserved
```

In `gait-rs/src/workflow/exec_shell.rs`, rename the `workspace` Rust field/param to `gait_dir`. The `cwd == "workspace"` string literal check (which matches a user config value) must NOT be renamed:
```rust
if shell.cwd() == "workspace" {   // ← string literal preserved
    cmd.current_dir(&ctx.gait_dir);  // ← Rust field renamed
}
```

- [ ] **Step 5: Verify compile errors (don't fix yet)**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo build 2>&1 | grep '^error' | head -30
```

Expected: many errors about `workspace` field/variable not found. That's correct — they guide Task 2.

---

## Task 2: Fix all call sites (follow the compiler)

**Files:** All remaining files with `workspace` variable/param names.

- [ ] **Step 1: Rename `builtin_tools()` param in `tools/mod.rs`**

In `gait-rs/src/tools/mod.rs`, the function is `builtin_tools` (not `build_tools`). Find it around line 106 and rename the `workspace` param:
```rust
pub fn builtin_tools(
    session_id: String,
    codebase_path: PathBuf,
    gait_dir: PathBuf,        // was: workspace
    repos: crate::git::Repos,
) -> Vec<Box<dyn Tool>> {
    // ...
    ShellTool::new(codebase_path.clone(), gait_dir.clone()).boxed(),
    SpawnAgentTool::new(session_id.clone(), gait_dir.clone(), codebase_path.clone(), repos.clone()).boxed(),
    ListAgentsTool::new(gait_dir.clone()).boxed(),
```

- [ ] **Step 2: Rename `profile.rs` resolve() param**

In `gait-rs/src/profile.rs`, find `pub async fn resolve(` (around line 99). Change `workspace: std::path::PathBuf` → `gait_dir: std::path::PathBuf` and update all uses of the `workspace` local inside the function to `gait_dir`.

**Important:** `profile.rs` around line 199 contains `if ct.cwd == "workspace"` — this is a config field value check, NOT a Rust identifier. Do not rename it.

- [ ] **Step 3: Rename git trait method params**

In `gait-rs/src/git/trace.rs`, rename every `workspace: &Path` param to `gait_dir: &Path` in the trait declaration and the `GitTraceRepo` impl. There are 8 methods:
```rust
pub trait TraceRepo: Send + Sync {
    fn init_workspace(&self, gait_dir: &Path) -> anyhow::Result<()>;
    fn create_session_worktree(&self, gait_dir: &Path, session_id: &str) -> anyhow::Result<()>;
    fn commit_session(&self, gait_dir: &Path, session_id: &str, message: &str) -> anyhow::Result<()>;
    fn list_active_sessions(&self, gait_dir: &Path) -> anyhow::Result<Vec<String>>;
    fn finish_session(&self, gait_dir: &Path, session_id: &str) -> anyhow::Result<()>;
    fn create_branch_worktree(&self, gait_dir: &Path, base_branch: &str, new_branch: &str, worktree_path: &Path) -> anyhow::Result<()>;
    fn remove_branch_worktree(&self, gait_dir: &Path, worktree_path: &Path) -> anyhow::Result<()>;
    fn merge_branch_ff(&self, gait_dir: &Path, target_branch: &str, source_branch: &str) -> anyhow::Result<()>;
}
```

Apply the same param rename inside every `GitTraceRepo` impl method body.

In `gait-rs/src/git/checkpoint.rs`, rename `workspace: &Path` → `gait_dir: &Path` in all `CheckpointRepo` trait methods and impl.

In `gait-rs/src/git/mod.rs`, rename the `workspace: &Path` param in `checkpoint_git_dir()`.

- [ ] **Step 4: Rename function params in config, permission, agent_frontmatter, docker, session_json, progress**

In `gait-rs/src/config.rs`:
```rust
pub fn load_config(gait_dir: &Path) -> GaitConfig { ... }
pub fn append_allow_pattern(gait_dir: &Path, pattern: &str) -> anyhow::Result<()> { ... }
pub fn append_deny_pattern(gait_dir: &Path, pattern: &str) -> anyhow::Result<()> { ... }
```

In `gait-rs/src/permission.rs`: rename `workspace: &Path` → `gait_dir: &Path` in all functions.

In `gait-rs/src/agent_frontmatter.rs`:
```rust
pub fn resolve_agent_path(gait_dir: &std::path::Path, name: &str) -> PathBuf { ... }
```

In `gait-rs/src/docker.rs`:
```rust
pub fn build_mount_map(gait_dir: &Path, codebase: &Path) -> MountMap { ... }
pub fn write_mount_map(gait_dir: &Path, map: &MountMap) -> anyhow::Result<()> { ... }
```

In `gait-rs/src/session/session_json.rs`:
```rust
pub fn read_session_json(gait_dir: &Path, session_id: &str) -> SessionJson { ... }
pub fn write_session_json(gait_dir: &Path, session_json: &SessionJson) -> anyhow::Result<()> { ... }
```

In `gait-rs/src/workflow/progress.rs`:
```rust
pub fn write_progress(gait_dir: &Path, session_id: &str, progress: &ProgressFile) -> anyhow::Result<()> { ... }
pub fn read_progress(gait_dir: &Path, session_id: &str) -> Option<ProgressFile> { ... }
```

- [ ] **Step 5: Rename params in `exec_prompt.rs` and `exec_loop.rs`**

In `gait-rs/src/workflow/exec_prompt.rs`, rename all `workspace` params and `ctx.workspace` field accesses to `gait_dir`/`ctx.gait_dir`. There are ~7 occurrences.

In `gait-rs/src/workflow/exec_loop.rs`, rename the one `workspace: ctx.workspace` assignment to `gait_dir: ctx.gait_dir`.

- [ ] **Step 6: Rename local variables in `turn.rs`**

In `gait-rs/src/turn.rs`, rename every local variable `workspace` → `gait_dir`. Includes the `once_approval_path()` helper:
```rust
pub fn once_approval_path(gait_dir: &std::path::Path, session_id: &str) -> std::path::PathBuf {
    gait_dir.join("tmp").join(format!("gait-approve-{}", session_id))
}
```

- [ ] **Step 7: Rename local variables in `chat.rs`**

In `gait-rs/src/chat.rs`, rename the `workspace: PathBuf` param of `run_chat()` (around line 422) and all local uses of `workspace` → `gait_dir`.

- [ ] **Step 8: Rename local variables in `bin/gait.rs`**

In `gait-rs/src/bin/gait.rs`, rename every local variable named `workspace` to `gait_dir` across all subcommand functions.

**Note on `cmd_workflow_*`:** These functions (`cmd_workflow_run`, `cmd_workflow_list`, etc.) receive `gait_dir: PathBuf` as a parameter — they do NOT parse `--gait-dir` themselves. The resolution happens at the dispatch site around line 611. Rename the parameter and usages, but do not add resolution logic to these functions.

- [ ] **Step 9: Build to verify no remaining errors**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo build 2>&1 | grep '^error'
```

Expected: no errors. If any remain, they point to exact lines — fix them.

- [ ] **Step 10: Commit struct + call site rename**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && git add -p
git commit -m "refactor: rename workspace → gait_dir throughout (struct fields + call sites)"
```

---

## Task 3: Fix resolution logic and help strings

**Files:**
- Modify: `gait-rs/src/bin/gait.rs`

- [ ] **Step 1: Add `serial_test` dev-dependency**

In `gait-rs/Cargo.toml`, add to `[dev-dependencies]`:
```toml
serial_test = "3"
```

This is needed because the `GAIT_DIR` env-var tests mutate process-global state and will race when run in parallel by default.

- [ ] **Step 2: Write failing tests for gait_dir resolution**

In `gait-rs/src/bin/gait.rs`, add to the `#[cfg(test)]` block at the bottom:

```rust
#[cfg(test)]
mod resolution_tests {
    use super::*;
    use serial_test::serial;

    #[test]
    #[serial]
    fn resolve_gait_dir_uses_env_var_when_no_arg() {
        std::env::set_var("GAIT_DIR", "/tmp/test-gait");
        let result = resolve_gait_dir(&[]);
        std::env::remove_var("GAIT_DIR");
        assert_eq!(result.unwrap(), std::path::PathBuf::from("/tmp/test-gait"));
    }

    #[test]
    #[serial]
    fn resolve_gait_dir_arg_overrides_env_var() {
        std::env::set_var("GAIT_DIR", "/tmp/env-gait");
        let args = vec!["--gait-dir".to_string(), "/tmp/arg-gait".to_string()];
        let result = resolve_gait_dir(&args);
        std::env::remove_var("GAIT_DIR");
        assert_eq!(result.unwrap(), std::path::PathBuf::from("/tmp/arg-gait"));
    }

    #[test]
    #[serial]
    fn resolve_gait_dir_errors_when_nothing_set() {
        std::env::remove_var("GAIT_DIR");
        let result = resolve_gait_dir(&[]);
        assert!(result.is_err(), "should error when no --gait-dir and no $GAIT_DIR");
    }

    #[test]
    fn resolve_codebase_defaults_to_pwd() {
        let result = resolve_codebase(&[]);
        assert_eq!(result, std::env::current_dir().unwrap());
    }

    #[test]
    fn resolve_codebase_uses_arg() {
        let args = vec!["--codebase".to_string(), "/tmp/mycode".to_string()];
        let result = resolve_codebase(&args);
        assert_eq!(result, std::path::PathBuf::from("/tmp/mycode"));
    }
}
```

- [ ] **Step 3: Run tests to verify they fail**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test resolution_tests 2>&1 | tail -15
```

Expected: compile error — `resolve_gait_dir` and `resolve_codebase` don't exist yet.

- [ ] **Step 4: Extract resolution helpers**

In `gait-rs/src/bin/gait.rs`, add these two functions near `extract_path_arg`:

```rust
/// Resolve gait dir: --gait-dir arg > $GAIT_DIR env var > error.
fn resolve_gait_dir(args: &[String]) -> anyhow::Result<std::path::PathBuf> {
    if let Some(p) = extract_path_arg(args, "--gait-dir") {
        return Ok(p);
    }
    if let Ok(v) = std::env::var("GAIT_DIR") {
        if !v.is_empty() {
            return Ok(std::path::PathBuf::from(v));
        }
    }
    anyhow::bail!("no gait dir: pass --gait-dir <path> or set $GAIT_DIR")
}

/// Resolve codebase: --codebase arg > current working directory.
fn resolve_codebase(args: &[String]) -> std::path::PathBuf {
    extract_path_arg(args, "--codebase")
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")))
}
```

- [ ] **Step 5: Run tests to verify they pass**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test resolution_tests 2>&1 | tail -15
```

Expected: all 5 tests pass.

- [ ] **Step 6: Wire helpers into subcommands**

Replace ad-hoc `gait_dir` resolution in each subcommand with `resolve_gait_dir`. The pattern is:

```rust
// Before (typical subcommand):
let mut gait_dir = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
// ... loop setting gait_dir from --gait-dir arg ...

// After:
let gait_dir = resolve_gait_dir(&argv[2..]).unwrap_or_else(|e| {
    eprintln!("[gait] {e}");
    std::process::exit(1);
});
```

Apply to these subcommands: `cmd_run`, `cmd_finish`, `cmd_undo`, `cmd_sessions`, `cmd_approve`, `cmd_reject`, `cmd_status`.

For `cmd_run`, also replace the codebase resolution:
```rust
let codebase_path = resolve_codebase(&argv[2..]);
```

**`cmd_workflow` dispatch site (around line 611):** The five `cmd_workflow_*` functions are called from a single dispatch block that resolves `gait_dir` once and passes it by value. Apply `resolve_gait_dir` at that single dispatch site only — do not add resolution inside the individual `cmd_workflow_*` functions.

**`cmd_init` exception:** `gait init` creates a new gait dir — it must not require one to already exist. Keep `cmd_init` using `current_dir()` as its default. `cmd_init` does not accept `--gait-dir` today and that is intentional (out of scope for this plan).

**Early pre-resolution block (lines 102-104):** This block resolves `gait_dir` before the main match for `load`/`unload`/`enable`/`disable`. Keep it using `extract_path_arg` with a `current_dir()` fallback (not `resolve_gait_dir`) — changing it to error-on-missing would cause unrelated commands (`auth`, `models`, etc.) routed through this block to error even though they never use the gait_dir. The per-subcommand wiring above is sufficient.

**`cmd_chat` and `--codebase "."` special case:** `cmd_chat` has special handling where `--codebase "."` re-resolves to `current_dir()`. Do not route `cmd_chat`'s codebase through `resolve_codebase` — leave its existing resolution logic unchanged to preserve this behaviour.

- [ ] **Step 7: Fix all help text strings**

In `gait-rs/src/bin/gait.rs`, update the `usage()` function (lines 74-85) to replace every `--workspace <path>` with `--gait-dir <path>`:

```rust
eprintln!("  run      [--session <id>] [--task <text>] [--gait-dir <path>] [--codebase <path>] [--auto] [--model <provider/model>] [--container]");
eprintln!("  finish   <session-id> [--gait-dir <path>]");
eprintln!("  undo    <session-id> [--turn N | --list] [--code] [--transcript] [--gait-dir <path>]");
eprintln!("  sessions [--gait-dir <path>]");
eprintln!("  chat     [--gait-dir <path>] [--codebase <path>] [--agent <name>]");
eprintln!("  approve  <session-id> [--pattern <pat>] [--scope <global|agent|once>] [--gait-dir <path>] [--agent <name>]");
eprintln!("  reject   <session-id> [--gait-dir <path>]");
eprintln!("  status   <session-id> [--gait-dir <path>] [--json]");
```

Also fix inline `usage:` error strings (e.g. `"usage: gait finish <session-id> [--workspace <path>]"`). Find all remaining hits:
```bash
grep -n "workspace" /Users/aicoder/Documents/ideation/gait/gait-rs/src/bin/gait.rs
```
Fix every hit.

- [ ] **Step 8: Run all tests**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test 2>&1 | tail -20
```

Expected: all tests pass.

- [ ] **Step 9: Commit**

```bash
git add gait-rs/src/bin/gait.rs gait-rs/Cargo.toml
git commit -m "feat: resolve gait_dir from --gait-dir > \$GAIT_DIR > error; fix help strings"
```

---

## Task 4: Final verification

- [ ] **Step 1: Full test run**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test 2>&1 | tail -30
```

Expected: all tests pass, zero failures.

- [ ] **Step 2: Verify no `workspace` identifiers remain**

```bash
grep -rn "\bworkspace\b" /Users/aicoder/Documents/ideation/gait/gait-rs/src --include="*.rs" \
  | grep -v '\$WORKSPACE\|"workspace"\|"WORKSPACE"\|// '
```

Expected: zero hits. Any remaining hits are bugs — fix them.

- [ ] **Step 3: Verify no stale `--workspace` help strings remain**

```bash
grep -rn '"--workspace\|--workspace <' /Users/aicoder/Documents/ideation/gait/gait-rs/src --include="*.rs"
```

Expected: zero hits.

- [ ] **Step 4: Smoke test binary**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo build 2>&1 | grep '^error'
# With no GAIT_DIR set, should error:
GAIT_DIR="" ./target/debug/gait run --task "hello" 2>&1 | head -3
# Expected: [gait] no gait dir: pass --gait-dir <path> or set $GAIT_DIR
```

- [ ] **Step 5: Final commit**

```bash
git add -p
git commit -m "chore: verify gait_dir rename complete — no workspace remnants"
```

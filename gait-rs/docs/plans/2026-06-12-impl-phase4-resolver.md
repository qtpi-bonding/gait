# Phase 4: Unified Markdown-Context Resolver + Agents → Directory Form

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the scattered, inconsistent `agents/<name>.md` flat-file lookups with a single `resolve_markdown_context_path(gait_dir, kind, name)` resolver that uses directory form (`agents/<name>/AGENT.md`, etc.), and kill the duplicate resolver and the `AGENTS.md` relic in `exec_prompt.rs`.

**Architecture:** A new `MarkdownKind` enum and `resolve_markdown_context_path` function live in `agent_frontmatter.rs`, replacing `resolve_agent_path`. Every call site (`chat.rs`, `bin/gait.rs` ×3, `exec_prompt.rs`) is updated. The old flat `agents/<name>.md` path is dropped entirely — no fallback, clean switch. `USER.md` remains a singleton resolved at the call site (as today), not through the enum.

**Tech Stack:** Rust stable, `std::path::PathBuf`, `tempfile` (already a dev-dep) for tests. No new dependencies.

---

## Files

| File | Change |
|---|---|
| `src/agent_frontmatter.rs` | Add `MarkdownKind` enum + `resolve_markdown_context_path`; delete `resolve_agent_path`; update tests |
| `src/chat.rs` | Replace `resolve_agent_path` call (line 40) + fallback `agent.md` (line 41) with new resolver |
| `src/bin/gait.rs` | Replace `resolve_agent_path` at lines 213, 215, 926; update `scaffold_gait_dir` (line 328); update `cmd_init` default agent path (line 375); update test at line 1368 |
| `src/workflow/exec_prompt.rs` | Replace `resolve_system_prompt` (lines 87–95) with call to the unified resolver; retire `AGENTS.md` fallback |
| `src/prompt.rs` | Comment on line 5: update `<agent>` doc comment to reflect new path |

---

## Task 1: Add `MarkdownKind` enum + `resolve_markdown_context_path` to `agent_frontmatter.rs`

**Files:**
- Modify: `src/agent_frontmatter.rs:54-63` (replace `resolve_agent_path`)

- [ ] **Step 1: Write the failing test**

Add this test block at the bottom of the `#[cfg(test)]` mod in `src/agent_frontmatter.rs` (after the existing `resolve_agent_path_appends_md` test and `append_agent_allow_creates_missing_file` test, before the closing `}`):

```rust
    #[test]
    fn resolve_markdown_context_path_agent_uses_dir_form() {
        let dir = TempDir::new().unwrap();
        let path = resolve_markdown_context_path(dir.path(), MarkdownKind::Agent, "coder");
        assert_eq!(path, dir.path().join("agents/coder/AGENT.md"));
    }

    #[test]
    fn resolve_markdown_context_path_project_uses_dir_form() {
        let dir = TempDir::new().unwrap();
        let path = resolve_markdown_context_path(dir.path(), MarkdownKind::Project, "myrepo-abc1234");
        assert_eq!(path, dir.path().join("codebases/myrepo-abc1234/PROJECT.md"));
    }

    #[test]
    fn resolve_markdown_context_path_skill_uses_dir_form() {
        let dir = TempDir::new().unwrap();
        let path = resolve_markdown_context_path(dir.path(), MarkdownKind::Skill, "tdd");
        assert_eq!(path, dir.path().join("skills/tdd/SKILL.md"));
    }

    #[test]
    fn resolve_markdown_context_path_agent_bare_name_no_slash_passthrough() {
        // Names with slashes should not be allowed — resolver always composes the path.
        // Verify no double-slash is emitted for a simple name.
        let dir = TempDir::new().unwrap();
        let path = resolve_markdown_context_path(dir.path(), MarkdownKind::Agent, "default");
        assert_eq!(path, dir.path().join("agents/default/AGENT.md"));
    }
```

- [ ] **Step 2: Run test to verify it fails**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test resolve_markdown_context_path 2>&1 > /tmp/gait-test-output.txt; echo "exit: $?"
grep "FAILED\|test result\|^error" /tmp/gait-test-output.txt
```

Expected: compile error — `resolve_markdown_context_path` and `MarkdownKind` are not yet defined.

- [ ] **Step 3: Add the enum and function**

In `src/agent_frontmatter.rs`, replace the existing `resolve_agent_path` function (lines 54–63) with the following. Keep `resolve_agent_path` deleted — no legacy alias:

```rust
/// Which kind of named markdown context file to resolve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkdownKind {
    /// `agents/<name>/AGENT.md` — the role/identity file for a named agent.
    Agent,
    /// `codebases/<key>/PROJECT.md` — per-codebase project instructions.
    Project,
    /// `skills/<name>/SKILL.md` — a loadable skill body.
    Skill,
}

/// Resolve a named markdown context file to its canonical path under `gait_dir`.
///
/// - `Agent`   → `<gait_dir>/agents/<name>/AGENT.md`
/// - `Project` → `<gait_dir>/codebases/<name>/PROJECT.md`
/// - `Skill`   → `<gait_dir>/skills/<name>/SKILL.md`
///
/// `USER.md` is a singleton (`<gait_dir>/USER.md`) — resolve it directly, not here.
pub fn resolve_markdown_context_path(
    gait_dir: &std::path::Path,
    kind: MarkdownKind,
    name: &str,
) -> PathBuf {
    match kind {
        MarkdownKind::Agent   => gait_dir.join("agents").join(name).join("AGENT.md"),
        MarkdownKind::Project => gait_dir.join("codebases").join(name).join("PROJECT.md"),
        MarkdownKind::Skill   => gait_dir.join("skills").join(name).join("SKILL.md"),
    }
}
```

- [ ] **Step 4: Run the new tests**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test resolve_markdown_context_path 2>&1 > /tmp/gait-test-output.txt; echo "exit: $?"
grep "FAILED\|test result\|^error" /tmp/gait-test-output.txt
```

Expected output:
```
test result: ok. 4 passed; 0 failed; 0 ignored
exit: 0
```

- [ ] **Step 5: Update the `resolve_agent_path_appends_md` test to use the new API**

The existing test at `src/agent_frontmatter.rs:226-229` tests the old function. Replace it:

```rust
    #[test]
    fn resolve_agent_path_uses_dir_form() {
        let dir = TempDir::new().unwrap();
        let path = resolve_markdown_context_path(dir.path(), MarkdownKind::Agent, "coder");
        assert_eq!(path, dir.path().join("agents/coder/AGENT.md"));
    }
```

- [ ] **Step 6: Full test suite passes (compile check)**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test 2>&1 > /tmp/gait-test-output.txt; echo "exit: $?"
grep "^error" /tmp/gait-test-output.txt | head -20
grep "FAILED\|test result" /tmp/gait-test-output.txt
```

Expected: compile errors from `resolve_agent_path` no longer existing in call sites. We will fix those in Tasks 2–4. For now just confirm the **agent_frontmatter** tests themselves pass, the errors are only from other files importing `resolve_agent_path`.

- [ ] **Step 7: Commit**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && git add src/agent_frontmatter.rs
git commit -m "$(cat <<'EOF'
feat(resolver): add MarkdownKind enum + resolve_markdown_context_path

Agents, projects, and skills now resolve to directory-form paths
(agents/<name>/AGENT.md etc.) via a single unified function.
resolve_agent_path removed — call sites updated in follow-up commits.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: Update `src/chat.rs` call site

**Files:**
- Modify: `src/chat.rs:39-42`

- [ ] **Step 1: Write the failing compile check (no new test needed — existing behavior test suffices)**

The call site at `src/chat.rs:39-42` currently reads:

```rust
    let agent_path: PathBuf = match agent_name.as_deref() {
        Some(n) => crate::agent_frontmatter::resolve_agent_path(&gait_dir, n),
        None => gait_dir.join("agent.md"),
    };
```

The `None` branch also references the old flat `agent.md` root file (a pre-`agents/` era relic). After the change, `None` should resolve to `agents/default/AGENT.md` — matching the new default used in `bin/gait.rs`.

- [ ] **Step 2: Apply the change**

In `src/chat.rs`, replace lines 39–42:

```rust
    let agent_path: Option<PathBuf> = match agent_name.as_deref() {
        Some(n) => Some(crate::agent_frontmatter::resolve_markdown_context_path(
            &gait_dir,
            crate::agent_frontmatter::MarkdownKind::Agent,
            n,
        )),
        None => {
            let default = crate::agent_frontmatter::resolve_markdown_context_path(
                &gait_dir,
                crate::agent_frontmatter::MarkdownKind::Agent,
                "default",
            );
            if default.exists() { Some(default) } else { None }
        }
    };
```

Note: `agent_path` type changes from `PathBuf` to `Option<PathBuf>`. Check that the downstream use in `chat.rs` (passed to `compose_system_prompt` or `Profile::from_config_and_agent`) accepts `Option<&Path>`. Looking at `compose_system_prompt(gait_dir, codebase, agent_path: Option<&Path>)` — it does. You will also need to search for how `agent_path` is used further down in `run_chat` and pass `agent_path.as_deref()` wherever `&agent_path` was used.

- [ ] **Step 3: Find downstream uses of `agent_path` in `run_chat` and fix them**

Search `src/chat.rs` for all references to `agent_path` after line 42 and ensure each one:
- passes `agent_path.as_deref()` where `Option<&Path>` is expected
- passes `agent_path.as_ref().map(|p| p.as_path())` or equivalent if a method takes `Option<&Path>`

The key call sites in `run_chat` (read the file to find the exact lines) are:
1. `compose_system_prompt` call — signature is `(gait_dir: &Path, codebase: Option<&Path>, agent_path: Option<&Path>)` — pass `agent_path.as_deref()`
2. `Profile::from_config_and_agent` — signature is `(&config, gait_dir, codebase, agent_path: Option<&Path>, model)` — pass `agent_path.as_deref()`

- [ ] **Step 4: Build check**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo build 2>&1 > /tmp/gait-build-output.txt; echo "exit: $?"
grep "^error" /tmp/gait-build-output.txt | head -20
```

Expected: chat.rs errors resolved; remaining errors are in `bin/gait.rs` and `exec_prompt.rs`.

- [ ] **Step 5: Commit**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && git add src/chat.rs
git commit -m "$(cat <<'EOF'
feat(resolver): update chat.rs to use resolve_markdown_context_path

Default agent now resolves to agents/default/AGENT.md (dir form).
Flat agent.md root relic removed.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: Update `src/bin/gait.rs` call sites (three locations + scaffold + init)

**Files:**
- Modify: `src/bin/gait.rs` lines 213–216, 328, 375–378, 926, 1368

There are five locations in this file that reference the old flat path form.

- [ ] **Step 1: Fix `cmd_run` — agent resolution (lines 211–217)**

Current code:
```rust
    let agent_path: Option<PathBuf> = if let Some(ref name) = agent_name {
        Some(gait_ai::agent_frontmatter::resolve_agent_path(&gait_dir, name))
    } else {
        let default = gait_dir.join("agents").join("default.md");
        if default.exists() { Some(default) } else { None }
    };
```

Replace with:
```rust
    let agent_path: Option<PathBuf> = if let Some(ref name) = agent_name {
        Some(gait_ai::agent_frontmatter::resolve_markdown_context_path(
            &gait_dir,
            gait_ai::agent_frontmatter::MarkdownKind::Agent,
            name,
        ))
    } else {
        let default = gait_ai::agent_frontmatter::resolve_markdown_context_path(
            &gait_dir,
            gait_ai::agent_frontmatter::MarkdownKind::Agent,
            "default",
        );
        if default.exists() { Some(default) } else { None }
    };
```

- [ ] **Step 2: Fix `scaffold_gait_dir` (line 328)**

Current code:
```rust
    for p in [agents.join("default.md"), gait_dir.join("USER.md")] {
```

Replace with (scaffold now creates the directory form):
```rust
    let default_agent_dir = agents.join("default");
    std::fs::create_dir_all(&default_agent_dir)?;
    for p in [default_agent_dir.join("AGENT.md"), gait_dir.join("USER.md")] {
```

- [ ] **Step 3: Fix `cmd_init` default agent scaffold (lines 374–378)**

Current code:
```rust
    let default_agent = agents_dir.join("default.md");
    if !default_agent.exists() {
        let _ = std::fs::write(&default_agent, "# Default agent\n\nAdd project-specific instructions here. Appended to the built-in gait harness description.\n\nFrontmatter fields (optional):\n  model: anthropic/claude-sonnet-4-6\n  allow: [read, shell:git *]\n  deny: [shell:rm *]\n");
    }
```

Replace with:
```rust
    let default_agent_dir = agents_dir.join("default");
    std::fs::create_dir_all(&default_agent_dir).unwrap_or_default();
    let default_agent = default_agent_dir.join("AGENT.md");
    if !default_agent.exists() {
        let _ = std::fs::write(&default_agent, "# Default agent\n\nAdd project-specific instructions here. Appended to the built-in gait harness description.\n\nFrontmatter fields (optional):\n  model: anthropic/claude-sonnet-4-6\n  allow: [read, shell:git *]\n  deny: [shell:rm *]\n");
    }
```

- [ ] **Step 4: Fix `cmd_approve` — agent resolution (lines 877 and 926)**

Current code:
```rust
    use gait_ai::agent_frontmatter::resolve_agent_path;
    // ...
    let agent_path = agent_name.as_deref().map(|n| resolve_agent_path(&gait_dir, n));
```

Replace with:
```rust
    use gait_ai::agent_frontmatter::{resolve_markdown_context_path, MarkdownKind};
    // ...
    let agent_path = agent_name.as_deref().map(|n| {
        resolve_markdown_context_path(&gait_dir, MarkdownKind::Agent, n)
    });
```

- [ ] **Step 5: Fix the scaffold test (line 1368)**

Current test assertion:
```rust
        assert!(dir.path().join("agents").join("default.md").exists());
```

Replace with:
```rust
        assert!(dir.path().join("agents").join("default").join("AGENT.md").exists());
```

- [ ] **Step 6: Build check**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo build 2>&1 > /tmp/gait-build-output.txt; echo "exit: $?"
grep "^error" /tmp/gait-build-output.txt | head -20
```

Expected: `bin/gait.rs` errors resolved. Only `exec_prompt.rs` may remain.

- [ ] **Step 7: Run tests**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test 2>&1 > /tmp/gait-test-output.txt; echo "exit: $?"
grep "FAILED\|test result" /tmp/gait-test-output.txt
```

Expected: all tests pass.

- [ ] **Step 8: Commit**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && git add src/bin/gait.rs
git commit -m "$(cat <<'EOF'
feat(resolver): update bin/gait.rs to dir-form agent paths

cmd_run, cmd_approve, scaffold_gait_dir, cmd_init all use
agents/<name>/AGENT.md now. Flat default.md relic removed.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: Kill the duplicate resolver in `exec_prompt.rs` + retire `AGENTS.md` relic

**Files:**
- Modify: `src/workflow/exec_prompt.rs:87-95`

The `resolve_system_prompt` function at lines 87–95 contains a hand-rolled inline resolver identical to the old `resolve_agent_path`, plus an `AGENTS.md` fallback that dates to before the `agents/` directory was introduced.

- [ ] **Step 1: Write a test for the new behavior**

The function is private (`fn resolve_system_prompt`). It is only called from `exec_prompt_node` in the same file. Add a test in `src/workflow/exec_prompt.rs` (or in its `#[cfg(test)]` block if one exists — check first).

If there is no test block, add one at the bottom of the file. The test exercises the three branches: explicit `system_prompt`, named `agent`, and no agent:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_system_prompt_returns_explicit_when_set() {
        let dir = tempfile::TempDir::new().unwrap();
        let node = crate::workflow::schema::NodeDef {
            system_prompt: Some("explicit prompt".to_string()),
            agent: None,
            ..Default::default()
        };
        let result = resolve_system_prompt(&node, dir.path());
        assert_eq!(result, "explicit prompt");
    }

    #[test]
    fn resolve_system_prompt_reads_agent_body_in_dir_form() {
        let dir = tempfile::TempDir::new().unwrap();
        let agent_dir = dir.path().join("agents").join("reviewer");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(agent_dir.join("AGENT.md"), "---\nmodel: x\n---\nYou are a reviewer.\n").unwrap();
        let node = crate::workflow::schema::NodeDef {
            system_prompt: None,
            agent: Some("reviewer".to_string()),
            ..Default::default()
        };
        let result = resolve_system_prompt(&node, dir.path());
        assert_eq!(result, "You are a reviewer.");
    }

    #[test]
    fn resolve_system_prompt_returns_empty_when_no_agent_and_no_agents_md() {
        let dir = tempfile::TempDir::new().unwrap();
        let node = crate::workflow::schema::NodeDef {
            system_prompt: None,
            agent: None,
            ..Default::default()
        };
        let result = resolve_system_prompt(&node, dir.path());
        assert_eq!(result, "");
    }
}
```

Note: `NodeDef` must implement `Default`. Check `src/workflow/schema.rs` — if it does not, you will need to construct it field by field. Read `src/workflow/schema.rs` before writing this test to confirm the struct shape and which fields have defaults.

- [ ] **Step 2: Run the test to verify it fails (or verify the current behavior is wrong)**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test workflow::exec_prompt 2>&1 > /tmp/gait-test-output.txt; echo "exit: $?"
grep "FAILED\|test result\|^error" /tmp/gait-test-output.txt
```

The `resolve_system_prompt_reads_agent_body_in_dir_form` test will fail because the current implementation looks for `agents/reviewer.md` (flat) not `agents/reviewer/AGENT.md` (dir form). The other two tests may pass already.

- [ ] **Step 3: Replace `resolve_system_prompt`**

In `src/workflow/exec_prompt.rs`, replace the entire `resolve_system_prompt` function (lines 87–95):

```rust
fn resolve_system_prompt(node: &NodeDef, gait_dir: &Path) -> String {
    if let Some(sp) = &node.system_prompt { return sp.clone(); }
    if let Some(name) = &node.agent {
        let p = crate::agent_frontmatter::resolve_markdown_context_path(
            gait_dir,
            crate::agent_frontmatter::MarkdownKind::Agent,
            name,
        );
        return crate::agent_frontmatter::read_agent_body(&p);
    }
    String::new()
}
```

Key changes:
1. Uses `resolve_markdown_context_path` (dir form) instead of inline `gait_dir.join("agents").join(format!("{}.md", name))`.
2. Uses `read_agent_body` to strip frontmatter (the old code used raw `read_to_string` which would have included the frontmatter in the prompt — this is a correctness fix as well).
3. Drops the `AGENTS.md` relic fallback (returns `String::new()` instead).

- [ ] **Step 4: Run the tests**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test workflow::exec_prompt 2>&1 > /tmp/gait-test-output.txt; echo "exit: $?"
grep "FAILED\|test result" /tmp/gait-test-output.txt
```

Expected:
```
test result: ok. 3 passed; 0 failed; 0 ignored
exit: 0
```

- [ ] **Step 5: Full test suite**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test 2>&1 > /tmp/gait-test-output.txt; echo "exit: $?"
grep "FAILED\|test result" /tmp/gait-test-output.txt
```

Expected: all tests pass, exit 0.

- [ ] **Step 6: Commit**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && git add src/workflow/exec_prompt.rs
git commit -m "$(cat <<'EOF'
feat(resolver): kill duplicate resolver in exec_prompt; retire AGENTS.md relic

resolve_system_prompt now uses resolve_markdown_context_path (dir form)
and read_agent_body (strips frontmatter). AGENTS.md fallback removed.
Three tests added covering all three resolution branches.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: Update `prompt.rs` doc comment + ensure `Project` kind aligns with `ensure_codebase_dir`

**Files:**
- Modify: `src/prompt.rs:5` (doc comment)
- Verify: `src/prompt.rs:86-92` (`ensure_codebase_dir`) still matches the `Project` kind path

The `Project` variant in `resolve_markdown_context_path` produces `codebases/<key>/PROJECT.md`. The existing `ensure_codebase_dir` in `prompt.rs` already creates `<gait_dir>/codebases/<key>/PROJECT.md` — these are consistent. No code change needed there.

The doc comment on line 5 still says `agents/<name>.md`:

```rust
//!   `<agent>`    — `agents/<name>.md`, the role
```

- [ ] **Step 1: Update the doc comment**

In `src/prompt.rs`, replace line 5:

```rust
//!   `<agent>`    — `agents/<name>/AGENT.md`, the role
```

- [ ] **Step 2: Verify `ensure_codebase_dir` path is consistent with `MarkdownKind::Project`**

`ensure_codebase_dir` (lines 86–92) builds: `gait_dir.join("codebases").join(codebase_key(codebase))` — the `PROJECT.md` is then `dir.join("PROJECT.md")`. The `MarkdownKind::Project` resolver takes a pre-computed key as `name` and builds `gait_dir.join("codebases").join(name).join("PROJECT.md")`. These are the same path when `name == codebase_key(codebase)`. No change needed, but add a comment in `prompt.rs` near `ensure_codebase_dir` to make this explicit:

In `src/prompt.rs` at line 84, add the comment line before `fn ensure_codebase_dir`:

```rust
/// Ensure `<gait-dir>/codebases/<key>/` exists with an empty `PROJECT.md`
/// affordance, and return the dir. The key is `codebase_key(codebase)`.
/// This path matches `resolve_markdown_context_path(gait_dir, MarkdownKind::Project, key)`.
fn ensure_codebase_dir(gait_dir: &Path, codebase: &Path) -> PathBuf {
```

- [ ] **Step 3: Build and test**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test 2>&1 > /tmp/gait-test-output.txt; echo "exit: $?"
grep "FAILED\|test result" /tmp/gait-test-output.txt
```

Expected: all tests pass, exit 0.

- [ ] **Step 4: Commit**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && git add src/prompt.rs
git commit -m "$(cat <<'EOF'
docs(resolver): update prompt.rs doc comment and ensure_codebase_dir note

Reflect the new agents/<name>/AGENT.md path in the module-level doc.
Add explicit note that ensure_codebase_dir matches MarkdownKind::Project.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
EOF
)"
```

---

## Task 6: Final verification — full build + test + dogfood smoke test

- [ ] **Step 1: Full build**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo build 2>&1 > /tmp/gait-build-output.txt; echo "exit: $?"
grep "^error\|^warning\[" /tmp/gait-build-output.txt | grep -v "^warning\[dead_code\]" | head -20
```

Expected: exit 0, no errors. Unused-variable warnings for `resolve_agent_path` callers that were removed are gone.

- [ ] **Step 2: Full test suite**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test 2>&1 > /tmp/gait-test-output.txt; echo "exit: $?"
grep "FAILED\|test result" /tmp/gait-test-output.txt
```

Expected:
```
test result: ok. N passed; 0 failed; 0 ignored
exit: 0
```

- [ ] **Step 3: Install the binary**

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && make install 2>&1 | tail -5
```

Expected: `Finished ... [optimized]` and binary copied to `/Users/aicoder/develop/gait`.

- [ ] **Step 4: Smoke test — confirm gait resolves the new path**

In the dogfood test workspace (`/Users/aicoder/gait-test-ws`), create the new-form agent directory and verify gait resolves it:

```bash
mkdir -p /Users/aicoder/gait-test-ws/agents/default
echo "# Smoke test agent\nYou are a helpful assistant." > /Users/aicoder/gait-test-ws/agents/default/AGENT.md
/Users/aicoder/develop/gait run \
  --gait-dir /Users/aicoder/gait-test-ws \
  --model minimax/MiniMax-M3 \
  --task "reply with exactly: phase4-ok" \
  --auto 2>&1 | tail -10
```

Expected: the model responds with `phase4-ok` (or similar short confirmation). If it errors with "file not found" or produces an empty agent body, the resolver has a bug.

- [ ] **Step 5: Confirm no `agents/<name>.md` flat-file references remain in src/**

```bash
grep -rn 'agents/.*\.md\|"agents".*"\.md"\|format!.*\.md.*agents' \
  /Users/aicoder/Documents/ideation/gait/gait-rs/src/ \
  --include="*.rs" | grep -v "target" | grep -v "#\[doc\]" | grep -v "//.*old"
```

Expected: zero results (or only inside comments). Any remaining match is a missed call site that must be fixed before this phase is complete.

- [ ] **Step 6: Final commit if any loose ends found**

If step 5 reveals any remaining flat-file paths, fix them, run tests again, then commit:

```bash
cd /Users/aicoder/Documents/ideation/gait/gait-rs && git add -p
git commit -m "$(cat <<'EOF'
fix(resolver): remove remaining flat agents/<name>.md references

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
EOF
)"
```

---

## Self-Review

### Spec coverage

| Spec requirement (§5 of `2026-06-12-agent-checkpoint-model.md`) | Covered by |
|---|---|
| `resolve_markdown_context_path(gait_dir, kind, name)` with `Agent/Project/Skill` kinds | Task 1 |
| `Agent → agents/<name>/AGENT.md` | Task 1 step 3 |
| `Project → codebases/<key>/PROJECT.md` | Task 1 step 3 |
| `Skill → skills/<name>/SKILL.md` | Task 1 step 3 |
| `USER.md` singleton — leave as-is | Task 1 step 3 doc comment; Task 5 verification |
| Clean switch — no legacy `agents/<name>.md` fallback | Tasks 2–4 (every call site updated) |
| Kill duplicate resolver in `exec_prompt.rs` | Task 4 |
| Retire `AGENTS.md` relic fallback | Task 4 step 3 |
| Update tests hardcoding `agents/<name>.md` | Tasks 1 step 5, 3 step 5 |

### Type consistency

- `MarkdownKind` enum defined in Task 1, used by name `crate::agent_frontmatter::MarkdownKind` at every call site in Tasks 2–4.
- `resolve_markdown_context_path(gait_dir: &std::path::Path, kind: MarkdownKind, name: &str) -> PathBuf` — signature is the same across all uses.
- `read_agent_body(path: &std::path::Path) -> String` — called in Task 4 step 3, signature unchanged from `agent_frontmatter.rs:37`.

### Placeholder scan

No TBD or TODO items remain. Every step includes exact code, exact commands, and exact expected output.

### Notes for the implementer

- **`NodeDef::Default`**: Before writing the `exec_prompt.rs` tests in Task 4, read `src/workflow/schema.rs` to check if `NodeDef` derives `Default`. If it does not, construct the struct manually with all required fields set to appropriate zero/empty values rather than using `..Default::default()`.
- **`chat.rs` downstream uses**: After changing `agent_path` from `PathBuf` to `Option<PathBuf>` in `run_chat`, use `grep -n "agent_path" src/chat.rs` to find every reference and update each one to call `.as_deref()` before passing to functions expecting `Option<&Path>`.
- **`push_file_layer` `src` attribute**: `push_file_layer` in `prompt.rs` uses `path.file_name()` for the `src` attribute — so the `<agent src="..." ...>` tag will now show `AGENT.md` instead of `coder.md`. This is correct and more informative. No test change needed, but be aware that prompt-caching hash stability for existing sessions is unaffected (hash is over the body content, not the filename).

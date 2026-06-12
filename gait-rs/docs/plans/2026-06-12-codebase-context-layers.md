# Codebase Context Layers Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give the agent real per-codebase and per-user context by composing the system prompt from four explicit, delimited layers — **harness** (built-in), **agent/role**, **project**, **user** — all stored in the gait-dir (never the target repo), plus a dynamic **status** block.

**Architecture:** `compose_system_prompt` becomes a layered builder. Each layer is wrapped in a named XML-style tag (`<harness>`, `<agent>`, `<project>`, `<user>`) carrying `src`/`hash` provenance, and is skipped when empty. The four prompt layers are stable → they form the cached system prompt (the Manifest's `instructions`). The dynamic `<status>` block (time, commit, dirty files) stays a separate per-turn user message, out of the cached prompt. Storage lives entirely in the gait-dir: `agents/<name>.md` (role, shared), `codebases/<basename-hash>/PROJECT.md` (per-codebase, shared), `USER.md` (gait-dir root, **gitignored**, personal).

**Observability (no new mechanism):** layer files are re-read every turn (the profile is rebuilt per turn), and `instructions` is part of the Manifest, which is re-emitted *in full* on drift (`turn.rs` drift guard). So a mid-conversation edit to any layer lands in `log.jsonl` the next turn, replay-complete. The per-layer `hash=` attribute gives cheap per-layer change detection inside that record — no simhash needed.

**Tech Stack:** Rust. Tests: `cargo test -p gait-ai` (per `gait-rs/CLAUDE.md`: redirect to a file, then grep — never re-run cargo just to see output).

**Design principles (carry into every task):**
- **Storage is gait-dir only.** Never write config/context files into the target codebase. The codebase is read/edited as *work*, never to configure the agent.
- **Tags, not bare separators.** Markdown layers contain `---`/`##`; only named, paired tags disambiguate section boundaries. Every block (including the first) is wrapped, so nothing floats outside a tag.
- **Skip empty.** Empty layer files exist as discoverable affordances; they are NOT emitted as empty tags.
- **Stable vs dynamic.** harness/agent/project/user are stable (cached system prompt). `<status>` is dynamic and stays a separate user message.
- **Naming rule.** lowercase = shipped source asset (`prompts/gait-harness.md`); UPPERCASE = user-editable config (`PROJECT.md`, `USER.md`).

---

## File Structure

- `gait-rs/prompts/gait-agent.md` → **rename** `gait-rs/prompts/gait-harness.md` — the built-in harness preamble.
- `gait-rs/src/lib.rs` — const `GAIT_AGENT_PROMPT` → `GAIT_HARNESS_PROMPT`; `include_str!` path; doc comment.
- `gait-rs/src/prompt.rs` — rewrite `compose_system_prompt` to the layered tag builder; add `codebase_key`, `read_nonempty`, `wrap_layer`, `push_file_layer`, `ensure_codebase_dir` helpers.
- `gait-rs/src/profile.rs` — thread `gait_dir: &Path` + `codebase: Option<&Path>` through `from_config_and_agent`.
- `gait-rs/src/chat.rs`, `gait-rs/src/bin/gait.rs`, `gait-rs/src/workflow/exec_prompt.rs` — update the three `from_config_and_agent` callsites.
- `gait-rs/src/git/trace.rs` or a new `scaffold` fn called at the init site (`bin/gait.rs:319`) — scaffold empty `agents/default.md`, `USER.md`, and ensure `.gitignore` contains `USER.md`.
- `gait-rs/src/turn.rs` — wrap `build_turn_context` output in `<status>…</status>`.

Two phases. **Phase 1 (Tasks 1–2)**: rename + pure helpers (independently shippable). **Phase 2 (Tasks 3–5)**: the layered `compose_system_prompt` + callsite threading, init scaffolding, and the `<status>` tag.

---

## Phase 1 — Rename + helpers

### Task 1: Rename the harness preamble `gait-agent.md` → `gait-harness.md`

**Why:** "agent" now names the *role* layer; leaving the harness file `gait-agent.md` collides with `agents/`. Pure rename, no behavior change.

**Files:**
- Rename: `gait-rs/prompts/gait-agent.md` → `gait-rs/prompts/gait-harness.md`
- Modify: `gait-rs/src/lib.rs` (const + include path + doc)
- Sweep: any `GAIT_AGENT_PROMPT` reference

- [ ] **Step 1: Rename the file**

```bash
cd /Users/aicoder/Documents/ideation/gait
git mv gait-rs/prompts/gait-agent.md gait-rs/prompts/gait-harness.md
```

- [ ] **Step 2: Update the const + include in `lib.rs`**

Find:
```rust
/// Default system prompt embedded from `prompts/gait-agent.md`.
/// Always prepended to the composed system prompt; additive with workspace AGENTS.md.
pub const GAIT_AGENT_PROMPT: &str = include_str!("../prompts/gait-agent.md");
```
Replace with:
```rust
/// The built-in harness preamble, embedded from `prompts/gait-harness.md`.
/// Wrapped in the `<harness>` layer of the composed system prompt.
pub const GAIT_HARNESS_PROMPT: &str = include_str!("../prompts/gait-harness.md");
```

- [ ] **Step 3: Sweep remaining references**

Run: `grep -rn "GAIT_AGENT_PROMPT\|gait-agent.md" gait-rs/src` → replace each `GAIT_AGENT_PROMPT` with `GAIT_HARNESS_PROMPT`. (The `compose_system_prompt` reference in `prompt.rs` is rewritten in Task 3, but make it compile now by updating the symbol.)

- [ ] **Step 4: Build + commit**

Run: `cd gait-rs && cargo build -p gait-ai 2>&1 > /tmp/t.txt; echo "exit: $?"; grep -E "^error" /tmp/t.txt | head`
Expected: exit 0. `grep -rn "GAIT_AGENT_PROMPT\|gait-agent.md" gait-rs/src` returns nothing.

```bash
git add gait-rs/prompts gait-rs/src && git commit -m "refactor: rename harness preamble gait-agent.md -> gait-harness.md"
```

---

### Task 2: Add the layer helpers (`codebase_key`, `read_nonempty`, `wrap_layer`)

**Files:**
- Modify: `gait-rs/src/prompt.rs` (add helpers near `content_hash_short`)
- Test: same file `#[cfg(test)] mod tests`

`content_hash_short(&str) -> String` (7 hex chars) already exists in `prompt.rs` and is reused for layer hashes and the codebase key.

- [ ] **Step 1: Write the failing tests**

```rust
    #[test]
    fn codebase_key_is_basename_plus_hash_and_stable() {
        use std::path::Path;
        let k1 = codebase_key(Path::new("/tmp/does-not-exist/myrepo"));
        let k2 = codebase_key(Path::new("/tmp/does-not-exist/myrepo"));
        assert!(k1.starts_with("myrepo-"), "key keeps a readable basename: {k1}");
        assert_eq!(k1, k2, "same path -> same key (stable)");
        assert_ne!(k1, codebase_key(Path::new("/other/myrepo")), "different path -> different key");
    }

    #[test]
    fn read_nonempty_skips_blank_files() {
        let dir = tempfile::TempDir::new().unwrap();
        let p = dir.path().join("X.md");
        assert_eq!(read_nonempty(&p), None, "missing file -> None");
        std::fs::write(&p, "   \n\t\n").unwrap();
        assert_eq!(read_nonempty(&p), None, "whitespace-only -> None");
        std::fs::write(&p, "hello").unwrap();
        assert_eq!(read_nonempty(&p).as_deref(), Some("hello"));
    }

    #[test]
    fn wrap_layer_emits_named_paired_tag_with_provenance() {
        let s = wrap_layer("project", "PROJECT.md", "abc1234", "body text");
        assert!(s.starts_with("<project src=\"PROJECT.md\" hash=\"abc1234\">\n"), "got: {s}");
        assert!(s.trim_end().ends_with("</project>"), "got: {s}");
        assert!(s.contains("body text"));
    }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cd gait-rs && cargo test -p gait-ai codebase_key_is_basename 2>&1 > /tmp/t.txt; grep -E "cannot find|error" /tmp/t.txt | head`
Expected: FAIL — `cannot find function codebase_key` / `read_nonempty` / `wrap_layer`.

- [ ] **Step 3: Add the helpers**

```rust
use std::path::{Path, PathBuf};

/// Stable, human-findable key for a codebase: `<basename>-<short-hash-of-abspath>`.
/// Two repos with the same basename in different paths never collide.
pub fn codebase_key(codebase: &Path) -> String {
    let abs = std::fs::canonicalize(codebase).unwrap_or_else(|_| codebase.to_path_buf());
    let base = abs.file_name().and_then(|n| n.to_str()).unwrap_or("codebase");
    format!("{base}-{}", content_hash_short(&abs.to_string_lossy()))
}

/// Read a file, returning `None` if it is missing or only whitespace.
pub fn read_nonempty(path: &Path) -> Option<String> {
    let s = std::fs::read_to_string(path).ok()?;
    let t = s.trim();
    if t.is_empty() { None } else { Some(t.to_string()) }
}

/// Wrap a layer body in a named, paired tag with `src`/`hash` provenance.
fn wrap_layer(tag: &str, src: &str, hash: &str, body: &str) -> String {
    format!("<{tag} src=\"{src}\" hash=\"{hash}\">\n{body}\n</{tag}>\n")
}
```

- [ ] **Step 4: Run to verify they pass**

Run: `cd gait-rs && cargo test -p gait-ai codebase_key_is_basename read_nonempty wrap_layer 2>&1 > /tmp/t.txt; grep -E "test result|error" /tmp/t.txt | head`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add gait-rs/src/prompt.rs && git commit -m "feat(prompt): codebase_key + read_nonempty + wrap_layer helpers"
```

---

## Phase 2 — Layered compose + scaffolding + status

### Task 3: Rewrite `compose_system_prompt` as the layered builder + thread gait_dir/codebase

**Files:**
- Modify: `gait-rs/src/prompt.rs` (`compose_system_prompt`, add `push_file_layer` + `ensure_codebase_dir`)
- Modify: `gait-rs/src/profile.rs` (`from_config_and_agent` signature)
- Modify callsites: `gait-rs/src/chat.rs:178`, `gait-rs/src/bin/gait.rs:221`, `gait-rs/src/workflow/exec_prompt.rs:36`, and the two `profile.rs` tests
- Test: `gait-rs/src/prompt.rs`

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn compose_layers_wraps_present_skips_empty() {
        let gait = tempfile::TempDir::new().unwrap();
        let cb = tempfile::TempDir::new().unwrap();
        // USER.md present; PROJECT.md will be auto-created empty (skipped); no agent.
        std::fs::write(gait.path().join("USER.md"), "I prefer terse output.").unwrap();

        let prompt = compose_system_prompt(gait.path(), Some(cb.path()), None);

        assert!(prompt.contains("<harness src=\"gait-harness.md\""), "harness always present: {prompt}");
        assert!(prompt.contains("</harness>"));
        assert!(prompt.contains("<user src=\"USER.md\""), "non-empty USER.md emitted");
        assert!(prompt.contains("I prefer terse output."));
        assert!(!prompt.contains("<project"), "empty PROJECT.md is skipped, not an empty tag");
        assert!(!prompt.contains("<agent"), "no agent path -> no agent block");
        // The per-codebase dir + empty affordance file were created:
        let key = codebase_key(cb.path());
        assert!(gait.path().join("codebases").join(&key).join("PROJECT.md").exists());
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cd gait-rs && cargo test -p gait-ai compose_layers_wraps_present 2>&1 > /tmp/t.txt; grep -E "this function takes|cannot find|error\[" /tmp/t.txt | head`
Expected: FAIL — arity mismatch on `compose_system_prompt` (now 3 args).

- [ ] **Step 3: Rewrite `compose_system_prompt` + helpers**

```rust
/// Compose the layered, stable (cacheable) system prompt. The dynamic per-turn
/// status is a SEPARATE user message (see `build_turn_context`), not part of this.
///
/// Layers, in order, each a named paired tag, skipped when empty:
///   <harness> built-in gait-harness.md (always present)
///   <agent>   agents/<name>.md — the role (agent_path)
///   <project> codebases/<key>/PROJECT.md — per-codebase
///   <user>    USER.md — gait-dir root, gitignored
pub fn compose_system_prompt(
    gait_dir: &Path,
    codebase: Option<&Path>,
    agent_path: Option<&Path>,
) -> String {
    let mut out = String::new();

    // <harness> — always present; "version" is the binary build hash.
    let h_hash = &crate::GAIT_BUILD_GIT_HASH[..crate::GAIT_BUILD_GIT_HASH.len().min(7)];
    out.push_str(&wrap_layer("harness", "gait-harness.md", h_hash,
        crate::GAIT_HARNESS_PROMPT.trim_end()));

    // <agent> — the role.
    if let Some(path) = agent_path {
        let body = crate::agent_frontmatter::read_agent_body(path);
        push_file_layer(&mut out, "agent", path, &body);
    }

    // <project> — per-codebase; ensure the dir + empty affordance file exist.
    if let Some(cb) = codebase {
        let project = ensure_codebase_dir(gait_dir, cb).join("PROJECT.md");
        if let Some(body) = read_nonempty(&project) {
            push_file_layer(&mut out, "project", &project, &body);
        }
    }

    // <user> — gait-dir root, gitignored.
    let user = gait_dir.join("USER.md");
    if let Some(body) = read_nonempty(&user) {
        push_file_layer(&mut out, "user", &user, &body);
    }

    out
}

/// Push a file-backed layer, skipping entirely if the (trimmed) body is empty.
fn push_file_layer(out: &mut String, tag: &str, path: &Path, body: &str) {
    let body = body.trim();
    if body.is_empty() { return; }
    let src = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let hash = content_hash_short(body);
    out.push_str(&wrap_layer(tag, src, &hash, body));
}

/// Ensure `<gait-dir>/codebases/<key>/` exists with an empty `PROJECT.md`
/// affordance, and return the dir.
fn ensure_codebase_dir(gait_dir: &Path, codebase: &Path) -> PathBuf {
    let dir = gait_dir.join("codebases").join(codebase_key(codebase));
    let _ = std::fs::create_dir_all(&dir);
    let project = dir.join("PROJECT.md");
    if !project.exists() { let _ = std::fs::write(&project, ""); }
    dir
}
```

- [ ] **Step 4: Thread `gait_dir` + `codebase` through `from_config_and_agent`**

In `profile.rs`, change the signature and the compose call:
```rust
    pub fn from_config_and_agent(
        config: &GaitConfig,
        gait_dir: &Path,
        codebase: Option<&Path>,
        agent_path: Option<&Path>,
        model_override: Option<String>,
    ) -> Self {
        // ...model/allow/deny unchanged...
        let instructions = crate::prompt::compose_system_prompt(gait_dir, codebase, agent_path);
        // ...rest unchanged...
    }
```
Add `use std::path::Path;` to `profile.rs` if not present.

- [ ] **Step 5: Update the three real callsites + two tests**

- `chat.rs:178` — pass `&gait_dir` and `Some(codebase.as_path())` (both in scope in `run_chat`):
```rust
            let profile = crate::profile::Profile::from_config_and_agent(
                &config, &gait_dir, Some(codebase.as_path()), Some(agent_path.as_path()), None,
            );
```
- `bin/gait.rs:221` — pass `&gait_dir` and `Some(codebase_path.as_path())` (the run handler has both; confirm the local variable names at that callsite):
```rust
    let profile = gait_ai::profile::Profile::from_config_and_agent(
        &config, &gait_dir, Some(codebase_path.as_path()), agent_path.as_deref(), model_override.clone());
```
- `workflow/exec_prompt.rs:36` — workflows are not bound to a single codebase; pass `gait_dir` and `None`:
```rust
    let mut profile = crate::profile::Profile::from_config_and_agent(
        &config, gait_dir, None, None, Some(model.clone()));
```
- `profile.rs` tests (the two `from_config_and_agent(&config, None, None)` calls) — add a tempdir + `None`:
```rust
        let dir = tempfile::TempDir::new().unwrap();
        let p = Profile::from_config_and_agent(&config, dir.path(), None, None, None);
```
(and the `Some("openai/y".into())` test analogously).

- [ ] **Step 6: Run the new test + full suite**

Run: `cd gait-rs && cargo test -p gait-ai compose_layers_wraps_present 2>&1 > /tmp/t.txt; grep -E "test result|error\[" /tmp/t.txt | head`
Then: `cd gait-rs && cargo test -p gait-ai 2>&1 > /tmp/tfull.txt; grep -E "test result: ok|FAILED|^error\[" /tmp/tfull.txt | head`
Expected: all PASS.

- [ ] **Step 7: Commit**

```bash
git add gait-rs/src/prompt.rs gait-rs/src/profile.rs gait-rs/src/chat.rs gait-rs/src/bin/gait.rs gait-rs/src/workflow/exec_prompt.rs && git commit -m "feat(prompt): layered system prompt (harness/agent/project/user) with tag delimiters"
```

---

### Task 4: Scaffold empty layer files + gitignore USER.md on init

**Files:**
- Modify: `gait-rs/src/bin/gait.rs` (the init path around `:319`, after `init_workspace`)
- Test: a focused unit test for the scaffold helper

The init command calls `init_workspace(&gait_dir)` (`bin/gait.rs:319`). Add a `scaffold_gait_dir(&gait_dir)` step that creates the empty editable affordances and ensures `USER.md` is gitignored. Keep it idempotent (never overwrite existing files).

- [ ] **Step 1: Write the failing test** (place in `bin/gait.rs`'s test module, or wherever `scaffold_gait_dir` lives)

```rust
    #[test]
    fn scaffold_creates_empty_affordances_and_gitignores_user() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join(".gitignore"), "sessions/\n").unwrap();
        scaffold_gait_dir(dir.path()).unwrap();
        assert!(dir.path().join("agents").join("default.md").exists());
        assert!(dir.path().join("USER.md").exists());
        let gi = std::fs::read_to_string(dir.path().join(".gitignore")).unwrap();
        assert!(gi.lines().any(|l| l.trim() == "USER.md"), "USER.md gitignored: {gi}");
        // Idempotent + non-clobbering:
        std::fs::write(dir.path().join("USER.md"), "mine").unwrap();
        scaffold_gait_dir(dir.path()).unwrap();
        assert_eq!(std::fs::read_to_string(dir.path().join("USER.md")).unwrap(), "mine");
        assert_eq!(gi.matches("USER.md").count(), 1, "no duplicate gitignore entry");
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cd gait-rs && cargo test -p gait-ai scaffold_creates_empty 2>&1 > /tmp/t.txt; grep -E "cannot find|error" /tmp/t.txt | head`
Expected: FAIL — `cannot find function scaffold_gait_dir`.

- [ ] **Step 3: Implement `scaffold_gait_dir`**

```rust
/// Create the empty, editable layer affordances in the gait-dir (idempotent,
/// never clobbers) and ensure `USER.md` is gitignored.
fn scaffold_gait_dir(gait_dir: &std::path::Path) -> std::io::Result<()> {
    let agents = gait_dir.join("agents");
    std::fs::create_dir_all(&agents)?;
    for p in [agents.join("default.md"), gait_dir.join("USER.md")] {
        if !p.exists() { std::fs::write(&p, "")?; }
    }
    let gi_path = gait_dir.join(".gitignore");
    let mut gi = std::fs::read_to_string(&gi_path).unwrap_or_default();
    if !gi.lines().any(|l| l.trim() == "USER.md") {
        if !gi.is_empty() && !gi.ends_with('\n') { gi.push('\n'); }
        gi.push_str("USER.md\n");
        std::fs::write(&gi_path, gi)?;
    }
    Ok(())
}
```

- [ ] **Step 4: Call it at the init site**

In `bin/gait.rs`, right after the `init_workspace(&gait_dir)` call (`:319`), add:
```rust
    let _ = scaffold_gait_dir(&gait_dir);
```

- [ ] **Step 5: Run the test + full suite, commit**

Run: `cd gait-rs && cargo test -p gait-ai scaffold_creates_empty 2>&1 > /tmp/t.txt; grep -E "test result|error" /tmp/t.txt | head`
Then full suite green.

```bash
git add gait-rs/src/bin/gait.rs && git commit -m "feat(init): scaffold empty agents/default.md + USER.md; gitignore USER.md"
```

---

### Task 5: Wrap the dynamic turn context in `<status>`

**Files:**
- Modify: `gait-rs/src/turn.rs` (`build_turn_context`, `:108`)
- Test: same file

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn turn_context_is_wrapped_in_status_tag() {
        let ctx = build_turn_context("deadbeef", &["src/a.rs".to_string()]);
        assert!(ctx.starts_with("<status>"), "got: {ctx}");
        assert!(ctx.trim_end().ends_with("</status>"), "got: {ctx}");
        assert!(ctx.contains("Codebase commit: deadbeef"));
        assert!(ctx.contains("src/a.rs"));
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cd gait-rs && cargo test -p gait-ai turn_context_is_wrapped 2>&1 > /tmp/t.txt; grep -E "assert|test result" /tmp/t.txt | head`
Expected: FAIL — no `<status>` wrapper.

- [ ] **Step 3: Wrap the output**

In `build_turn_context`, open with `<status>` and close with `</status>`:
```rust
fn build_turn_context(codebase_hash: &str, dirty_files: &[String]) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    let mut ctx = format!("<status>\nCurrent time (unix): {}\nCodebase commit: {}\n", secs, codebase_hash);
    if !dirty_files.is_empty() {
        ctx.push_str("Dirty files:\n");
        for f in dirty_files { ctx.push_str(&format!("  {}\n", f)); }
    }
    ctx.push_str("</status>\n");
    ctx
}
```

- [ ] **Step 4: Run the test + full suite**

Run: `cd gait-rs && cargo test -p gait-ai 2>&1 > /tmp/t.txt; grep -E "test result: ok|FAILED|^error\[" /tmp/t.txt | head`
Expected: all PASS.

- [ ] **Step 5: Commit**

```bash
git add gait-rs/src/turn.rs && git commit -m "feat(turn): wrap dynamic turn context in <status> tag"
```

---

## Self-Review

**Spec coverage:**
- Harness rename (`gait-agent.md` → `gait-harness.md`, const, refs) → Task 1. ✓
- Helpers (`codebase_key` basename-hash, `read_nonempty` skip-empty, `wrap_layer` tags) → Task 2. ✓
- Layered `compose_system_prompt` (harness/agent/project/user, tags, skip-empty, ensure per-codebase dir + empty PROJECT.md) → Task 3. ✓
- gait-dir-only storage; codebase keyed `basename-hash` → Tasks 2–3. ✓
- Thread gait_dir + codebase through profile + 3 callsites + 2 tests → Task 3. ✓
- Init scaffolds empty `agents/default.md` + `USER.md`; `USER.md` gitignored → Task 4. ✓
- Dynamic status kept separate + tagged `<status>` → Task 5. ✓
- Provider *content* layer dropped (delimiters were the real need) → not built (intentional). ✓
- Observability: layer edits land in `log.jsonl` via Manifest drift; per-layer `hash=` for cheap change detection; no simhash → satisfied by existing Manifest mechanism + the `hash=` attribute (Task 2/3), no new code. ✓

**Type consistency:** `compose_system_prompt(gait_dir: &Path, codebase: Option<&Path>, agent_path: Option<&Path>) -> String` is used identically in `profile.rs` and the test. `from_config_and_agent` gains `gait_dir: &Path, codebase: Option<&Path>` before `agent_path` at all 5 callsites (3 real + 2 tests). `codebase_key`/`read_nonempty` are `pub`; `wrap_layer`/`push_file_layer`/`ensure_codebase_dir` are private to `prompt.rs`.

**Open verification for the implementer (confirm against live code):**
1. The exact local variable names at `bin/gait.rs:221` for the gait-dir and codebase path (the plan assumes `gait_dir` and `codebase_path` — confirm and match).
2. That `run_chat` exposes `gait_dir` and `codebase` as owned `PathBuf`s in scope at `chat.rs:178` (it does — they're `run_chat` params); use `.as_path()`.
3. That `workflow/exec_prompt.rs` has a `gait_dir` in scope (it reads `gait_dir.join("AGENTS.md")` nearby — yes); pass it + `None`.
4. `tempfile` is a dev-dependency (used throughout existing tests) — available for the new tests.

---

## Execution Handoff

Two phases, five tasks. Phase 1 (rename + helpers) is independently shippable. Per the dogfood complexity-split: Tasks 1, 4, 5 are mechanical (delegate to the gait agent with intent + this plan); Task 2 is small (either); **Task 3 is the keystone** (signature threading across 5 callsites + the side-effecting compose) — do it directly.

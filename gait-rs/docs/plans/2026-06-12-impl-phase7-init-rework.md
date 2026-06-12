# Phase 7 — `gait init` Rework: Implementation Plan

**Status:** READY TO IMPLEMENT  
**Prerequisite:** Phase 4 complete (agents live at `agents/<name>/AGENT.md`, directory form).  
**Source of truth:** `docs/plans/2026-06-12-agent-checkpoint-model.md` §5 (init bullet) and §6 O2.

---

## Goal

Modernize `gait init` from a submodule-era relic into a clean, `git init`-style workspace
bootstrapper that reflects the Phase 4 directory layout, prints an actionable hint table,
and is safe to run with `--help`.

Current bugs to kill:
1. **`--help` runs the command** — `cmd_init` at line 341 ignores `--help`/`-h`; any `argv`
   that is empty or starts with a non-`-` token drops straight into the git+scaffold path.
   A user running `gait init --help` inside their codebase repo has silently committed to
   `HEAD`. (Real incident that prompted this plan.)
2. **Submodule/codebase relic** — lines 405-420 in `cmd_init`: the `repo_url` branch that
   runs `git submodule add <url> codebase`. This concept is gone.
3. **Stale staging list** — line 390: `["gait.jsonc", ".gitignore", "AGENTS.md"]`. `AGENTS.md`
   does not exist; `agents/default/AGENT.md` is never staged.
4. **Old flat agent path** — line 375: `agents/default.md`. Phase 4 moved to
   `agents/default/AGENT.md`.
5. **No `skills/` scaffold** — `skills/` directory is never created.
6. **No path argument** — `gait init [path]` does not exist; only cwd is supported.

---

## Architecture

```
cmd_init(argv: &[String])
    │
    ├─ parse_init_args(argv) → InitArgs { path: Option<PathBuf>, help: bool }
    │       — pure fn, no I/O, no side effects; testable in isolation
    │
    ├─ if help → init_usage() → exit(0)   // safe, no side effects
    │
    ├─ gait_dir = path.unwrap_or(cwd)
    │
    ├─ std::fs::create_dir_all(&gait_dir)
    │
    ├─ Repos::production().traces.init_workspace(&gait_dir)  // git init
    │
    ├─ scaffold_gait_dir(&gait_dir)       // idempotent file/dir scaffold
    │
    └─ stage_and_commit(&gait_dir)        // git2 staging + initial commit
           then: print_init_summary(&gait_dir)
```

### `scaffold_gait_dir` (refactored)

Creates the new layout. All operations are idempotent (never clobber existing files):

| path | notes |
|---|---|
| `agents/default/AGENT.md` | empty; dir form per Phase 4 |
| `USER.md` | empty; gitignored |
| `skills/` | empty directory |
| `sessions/` | empty directory |
| `gait.jsonc` | default config (model + minimal allow/tools) |
| `.gitignore` | `sessions/\nUSER.md\n.gait-ws.lock\n` |

The `.gitignore` writing is in `scaffold_gait_dir`, not duplicated in `cmd_init` (fix the
current split: `scaffold_gait_dir` adds `USER.md`, `cmd_init` adds `sessions/`).

### Staged files (the initial commit)

```rust
let to_stage = ["gait.jsonc", ".gitignore", "agents/default/AGENT.md"];
```

`AGENTS.md` is dropped. `skills/` is an empty dir — git does not track empty dirs, so it
is not staged (`.gitkeep` is not added; skills presence on disk drives availability, not
git tracking). `sessions/` is gitignored — not staged.

### `init_usage()` vs `usage()`

`init_usage()` is a focused helper that prints only `gait init` usage and exits 0 (help is
not an error):

```rust
fn init_usage() -> ! {
    eprintln!("usage: gait init [<path>]");
    eprintln!();
    eprintln!("Initialize a gait-dir at <path> (default: current directory).");
    eprintln!("Like 'git init': creates the path if needed, safe to re-run.");
    eprintln!();
    eprintln!("Options:");
    eprintln!("  -h, --help    Print this help");
    std::process::exit(0);
}
```

The top-level `usage()` (line 93) also needs `init` line updated to remove `[<repo-url>]`.

### Hint output shape

Printed to stderr after the commit succeeds:

```
[gait] initialized gait-dir at <abs-path>

  file                          purpose                                        git
  gait.jsonc                    model + tool-permission config                 committed
  agents/default/AGENT.md       the agent's role and instructions              committed
  USER.md                       your personal instructions (not shared)        ignored
  skills/                       installed skills  (skills/<n>/SKILL.md)        committed*
  sessions/                     turn history                                   ignored

  * empty dir; git-tracked when you add skills

  next: gait run --task "..."
```

Column widths are achieved with `format!("{:<30} {:<46} {}", ...)` — no external deps.

---

## Tech Stack

- Language: Rust (edition 2021, same as the rest of gait-rs)
- Git integration: `git2` crate (already a dependency; used in `cmd_init` lines 381-403)
- Testing: `tempfile` crate (already used in tests at line 1283+); `#[test]` unit tests only
  for pure helpers; no integration test that actually calls `cmd_init` end-to-end (git init
  inside a tempdir in tests is fragile — skip it, test helpers instead)

---

## Files

- `src/bin/gait.rs` — all changes live here
  - `parse_init_args` (new, pure)
  - `init_usage` (new)
  - `scaffold_gait_dir` (refactor: new layout + unified .gitignore)
  - `cmd_init` (refactor: drop submodule, add path arg, call helpers)
  - `usage` (update init line at ~96)
  - `#[cfg(test)]` block (add tests for new helpers)

---

## Tasks

---

### Task 1 — Write tests for `parse_init_args` and `scaffold_gait_dir` (new layout)

**Files:** `src/bin/gait.rs`

Write the tests first. They will fail to compile until Task 2 introduces the functions they
test — that is expected. These anchor the behavior contract before we touch any logic.

**Steps:**

- [ ] In the `#[cfg(test)] mod tests` block (line 1254), add a new `mod init_tests` submodule.

- [ ] Write `parse_init_args_no_args_returns_cwd_and_no_help`:
  ```rust
  #[test]
  fn parse_init_args_no_args_returns_cwd_and_no_help() {
      let args: Vec<String> = vec![];
      let a = parse_init_args(&args);
      assert!(!a.help, "no --help flag");
      assert!(a.path.is_none(), "no path → None (caller resolves to cwd)");
  }
  ```

- [ ] Write `parse_init_args_with_path`:
  ```rust
  #[test]
  fn parse_init_args_with_path() {
      let args = vec!["/tmp/myws".to_string()];
      let a = parse_init_args(&args);
      assert_eq!(a.path, Some(std::path::PathBuf::from("/tmp/myws")));
      assert!(!a.help);
  }
  ```

- [ ] Write `parse_init_args_help_flag_long` and `parse_init_args_help_flag_short`:
  ```rust
  #[test]
  fn parse_init_args_help_flag_long() {
      let args = vec!["--help".to_string()];
      let a = parse_init_args(&args);
      assert!(a.help);
  }

  #[test]
  fn parse_init_args_help_flag_short() {
      let args = vec!["-h".to_string()];
      let a = parse_init_args(&args);
      assert!(a.help);
  }
  ```

- [ ] Write `parse_init_args_help_with_path_help_wins` (help beats positional):
  ```rust
  #[test]
  fn parse_init_args_help_with_path_help_wins() {
      let args = vec!["/tmp/x".to_string(), "--help".to_string()];
      let a = parse_init_args(&args);
      assert!(a.help, "--help anywhere in argv sets help=true");
  }
  ```

- [ ] Write `scaffold_creates_new_dir_layout`:
  ```rust
  #[test]
  fn scaffold_creates_new_dir_layout() {
      let dir = tempfile::TempDir::new().unwrap();
      scaffold_gait_dir(dir.path()).unwrap();

      // New Phase 4 dir form
      assert!(dir.path().join("agents").join("default").join("AGENT.md").exists(),
          "agents/default/AGENT.md must exist");
      // Old flat form must NOT be created
      assert!(!dir.path().join("agents").join("default.md").exists(),
          "agents/default.md (flat, old) must not be created");
      // skills/ dir created
      assert!(dir.path().join("skills").is_dir(), "skills/ dir must exist");
      // USER.md created and gitignored
      assert!(dir.path().join("USER.md").exists());
      let gi = std::fs::read_to_string(dir.path().join(".gitignore")).unwrap_or_default();
      assert!(gi.lines().any(|l| l.trim() == "USER.md"), "USER.md in .gitignore");
      assert!(gi.lines().any(|l| l.trim() == "sessions/"), "sessions/ in .gitignore");
  }
  ```

- [ ] Write `scaffold_is_idempotent_and_does_not_clobber`:
  ```rust
  #[test]
  fn scaffold_is_idempotent_and_does_not_clobber() {
      let dir = tempfile::TempDir::new().unwrap();
      // Pre-populate with user content
      std::fs::create_dir_all(dir.path().join("agents").join("default")).unwrap();
      std::fs::write(dir.path().join("agents").join("default").join("AGENT.md"),
          "my instructions").unwrap();
      std::fs::write(dir.path().join("USER.md"), "personal").unwrap();

      scaffold_gait_dir(dir.path()).unwrap();

      assert_eq!(
          std::fs::read_to_string(dir.path().join("agents/default/AGENT.md")).unwrap(),
          "my instructions",
          "must not clobber existing AGENT.md"
      );
      assert_eq!(
          std::fs::read_to_string(dir.path().join("USER.md")).unwrap(),
          "personal",
          "must not clobber existing USER.md"
      );
  }
  ```

- [ ] Write `scaffold_gitignore_no_duplicate_entries`:
  ```rust
  #[test]
  fn scaffold_gitignore_no_duplicate_entries() {
      let dir = tempfile::TempDir::new().unwrap();
      // Run twice
      scaffold_gait_dir(dir.path()).unwrap();
      scaffold_gait_dir(dir.path()).unwrap();
      let gi = std::fs::read_to_string(dir.path().join(".gitignore")).unwrap();
      assert_eq!(gi.matches("USER.md").count(), 1, "USER.md appears once");
      assert_eq!(gi.matches("sessions/").count(), 1, "sessions/ appears once");
  }
  ```

- [ ] Confirm tests fail to compile as expected:
  ```bash
  cd /Users/aicoder/Documents/ideation/gait/gait-rs && \
    cargo test --bin gait 2>&1 > /tmp/gait-test-output.txt; echo "exit: $?"
  grep "^error" /tmp/gait-test-output.txt | head -20
  ```
  Expected: compile errors for `parse_init_args` not found.

---

### Task 2 — Implement `parse_init_args`, `InitArgs`, and `init_usage`

**Files:** `src/bin/gait.rs`

Add the pure arg-parsing struct and function just above `cmd_init` (currently at line 340).

**Steps:**

- [ ] Add `InitArgs` struct and `parse_init_args` function. Insert immediately before
  `fn cmd_init` (line 340):

  ```rust
  struct InitArgs {
      /// Target gait-dir. None = use cwd.
      path: Option<std::path::PathBuf>,
      /// User passed -h or --help.
      help: bool,
  }

  /// Pure: parse `gait init [<path>] [-h|--help]`. No I/O, no side effects.
  fn parse_init_args(argv: &[String]) -> InitArgs {
      let help = argv.iter().any(|a| a == "--help" || a == "-h");
      let path = if help {
          None
      } else {
          argv.iter()
              .find(|a| !a.starts_with('-'))
              .map(std::path::PathBuf::from)
      };
      InitArgs { path, help }
  }
  ```

- [ ] Add `init_usage` function immediately after `parse_init_args`:

  ```rust
  fn init_usage() -> ! {
      eprintln!("usage: gait init [<path>]");
      eprintln!();
      eprintln!("Initialize a gait-dir at <path> (default: current directory).");
      eprintln!("Like 'git init': creates the path if needed, safe to re-run.");
      eprintln!();
      eprintln!("Options:");
      eprintln!("  -h, --help    Print this help");
      std::process::exit(0);
  }
  ```

- [ ] Run tests; `parse_init_args` tests should now pass:
  ```bash
  cd /Users/aicoder/Documents/ideation/gait/gait-rs && \
    cargo test --bin gait init_tests 2>&1 > /tmp/gait-test-output.txt; echo "exit: $?"
  grep "FAILED\|test result\|^error" /tmp/gait-test-output.txt
  ```
  Expected: `parse_init_args_*` tests pass. `scaffold_*` tests still fail (old layout).

- [ ] Commit:
  ```
  git add src/bin/gait.rs
  git commit -m "$(cat <<'EOF'
  feat(init): add InitArgs/parse_init_args/init_usage — pure helpers for Phase 7

  Introduces the pure, side-effect-free arg-parsing layer for gait init: InitArgs
  struct, parse_init_args (detects -h/--help before any I/O), and init_usage
  (exits 0, so --help is safe). Tests added first, all passing.

  Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
  EOF
  )"
  ```

---

### Task 3 — Refactor `scaffold_gait_dir` to the Phase 4 layout

**Files:** `src/bin/gait.rs`

Replace the current `scaffold_gait_dir` body (lines 325-339) with the new layout.

**Steps:**

- [ ] Read the existing function body (lines 325-339) one more time to confirm the exact
  old text before editing:

  ```rust
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

- [ ] Replace the body with the new implementation:

  ```rust
  /// Create the empty, editable layer affordances in the gait-dir (idempotent,
  /// never clobbers) and ensure USER.md and sessions/ are gitignored.
  fn scaffold_gait_dir(gait_dir: &std::path::Path) -> std::io::Result<()> {
      // agents/default/ — Phase 4 directory form
      let default_agent_dir = gait_dir.join("agents").join("default");
      std::fs::create_dir_all(&default_agent_dir)?;
      let default_agent = default_agent_dir.join("AGENT.md");
      if !default_agent.exists() {
          std::fs::write(&default_agent, "")?;
      }

      // USER.md — personal instructions, gitignored
      let user_md = gait_dir.join("USER.md");
      if !user_md.exists() {
          std::fs::write(&user_md, "")?;
      }

      // skills/ — installed skills (disk presence = available)
      std::fs::create_dir_all(gait_dir.join("skills"))?;

      // sessions/ — turn history
      std::fs::create_dir_all(gait_dir.join("sessions"))?;

      // gait.jsonc — default config (never clobber)
      let config_path = gait_dir.join("gait.jsonc");
      if !config_path.exists() {
          std::fs::write(&config_path,
              "{\n  // gait configuration\n  \"model\": \"anthropic/claude-sonnet-4-6\",\n  // Allow list: tools that run without approval. Default empty = everything gated.\n  \"allow\": [],\n  // Tier 1 config-defined tools: shell commands the LLM can call by name\n  \"tools\": []\n}\n")?;
      }

      // .gitignore — merge required entries, no duplicates
      let gi_path = gait_dir.join(".gitignore");
      let mut gi = std::fs::read_to_string(&gi_path).unwrap_or_default();
      let required = ["sessions/", "USER.md", ".gait-ws.lock"];
      let mut changed = false;
      for entry in &required {
          if !gi.lines().any(|l| l.trim() == *entry) {
              if !gi.is_empty() && !gi.ends_with('\n') { gi.push('\n'); }
              gi.push_str(entry);
              gi.push('\n');
              changed = true;
          }
      }
      if changed || !gi_path.exists() {
          std::fs::write(&gi_path, &gi)?;
      }

      Ok(())
  }
  ```

- [ ] Run scaffold tests:
  ```bash
  cd /Users/aicoder/Documents/ideation/gait/gait-rs && \
    cargo test --bin gait init_tests 2>&1 > /tmp/gait-test-output.txt; echo "exit: $?"
  grep "FAILED\|test result\|^error" /tmp/gait-test-output.txt
  ```
  Expected: all `init_tests` pass.

- [ ] Also run the existing `scaffold_creates_empty_affordances_and_gitignores_user` test
  (line 1364). This test asserts `agents/default.md` exists — it will now **fail** because
  the new scaffold creates `agents/default/AGENT.md`. Update the test to match the new layout:
  ```rust
  // In the existing test (line 1364), replace:
  //   assert!(dir.path().join("agents").join("default.md").exists());
  // with:
  assert!(dir.path().join("agents").join("default").join("AGENT.md").exists());
  // and drop the old flat path check if present.
  ```

- [ ] Run all tests to confirm no regressions:
  ```bash
  cd /Users/aicoder/Documents/ideation/gait/gait-rs && \
    cargo test --bin gait 2>&1 > /tmp/gait-test-output.txt; echo "exit: $?"
  grep "FAILED\|test result\|^error" /tmp/gait-test-output.txt
  ```

- [ ] Commit:
  ```
  git add src/bin/gait.rs
  git commit -m "$(cat <<'EOF'
  refactor(init): scaffold Phase 4 layout — agents/default/AGENT.md, skills/, unified .gitignore

  scaffold_gait_dir now creates the directory-form agent path (agents/default/AGENT.md),
  a skills/ directory, and merges all gitignore entries (sessions/, USER.md,
  .gait-ws.lock) in one pass with duplicate-safe logic. Drops flat agents/default.md.
  Existing scaffold test updated to match new paths.

  Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
  EOF
  )"
  ```

---

### Task 4 — Rewrite `cmd_init`: drop submodule relic, add path arg, fix staging, add hint

**Files:** `src/bin/gait.rs`

Replace the entire `cmd_init` body (lines 340-421).

**Steps:**

- [ ] Write `print_init_summary` helper (insert just before `cmd_init`):

  ```rust
  fn print_init_summary(gait_dir: &std::path::Path) {
      let display = gait_dir.display();
      eprintln!("[gait] initialized gait-dir at {display}");
      eprintln!();
      eprintln!("  {:<32} {:<46} {}", "file", "purpose", "git");
      eprintln!("  {:<32} {:<46} {}", "gait.jsonc", "model + tool-permission config", "committed");
      eprintln!("  {:<32} {:<46} {}", "agents/default/AGENT.md", "the agent's role and instructions", "committed");
      eprintln!("  {:<32} {:<46} {}", "USER.md", "your personal instructions (not shared)", "ignored");
      eprintln!("  {:<32} {:<46} {}", "skills/", "installed skills  (skills/<n>/SKILL.md)", "committed*");
      eprintln!("  {:<32} {:<46} {}", "sessions/", "turn history", "ignored");
      eprintln!();
      eprintln!("  * empty dir; git-tracked when you add skills");
      eprintln!();
      eprintln!("  next: gait run --task \"...\"");
  }
  ```

- [ ] Replace `cmd_init` (lines 340-421) with the new implementation:

  ```rust
  fn cmd_init(argv: &[String]) {
      let args = parse_init_args(argv);
      if args.help { init_usage(); }

      let gait_dir = match args.path {
          Some(p) => p,
          None => std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
      };

      if let Err(e) = std::fs::create_dir_all(&gait_dir) {
          eprintln!("[gait] cannot create directory {}: {}", gait_dir.display(), e);
          std::process::exit(1);
      }

      if let Err(e) = gait_ai::git::Repos::production().traces.init_workspace(&gait_dir) {
          eprintln!("[gait] git init failed: {}", e);
          std::process::exit(1);
      }

      if let Err(e) = scaffold_gait_dir(&gait_dir) {
          eprintln!("[gait] scaffold failed: {}", e);
          std::process::exit(1);
      }

      // Stage and initial commit via git2
      let repo = match git2::Repository::open(&gait_dir) {
          Ok(r) => r,
          Err(e) => { eprintln!("[gait] cannot open repo: {e}"); std::process::exit(1); }
      };
      let sig = repo.signature()
          .or_else(|_| git2::Signature::now("gait", "gait@localhost"))
          .unwrap();
      let mut index = repo.index().unwrap();

      // Stage exactly what scaffold created (relative paths from gait_dir root)
      let to_stage = [
          "gait.jsonc",
          ".gitignore",
          "agents/default/AGENT.md",
      ];
      for f in &to_stage {
          if gait_dir.join(f).exists() {
              let _ = index.add_path(std::path::Path::new(f));
          }
      }
      index.write().unwrap();
      let tree_id = index.write_tree().unwrap();
      let tree = repo.find_tree(tree_id).unwrap();

      let parent_commits: Vec<git2::Commit> = repo.head()
          .ok().and_then(|h| h.peel_to_commit().ok()).into_iter().collect();
      let parents: Vec<&git2::Commit> = parent_commits.iter().collect();
      repo.commit(Some("HEAD"), &sig, &sig, "gait: init gait_dir", &tree, &parents).unwrap();

      print_init_summary(&gait_dir);
  }
  ```

  Key changes from old code:
  - Calls `parse_init_args` → `init_usage` before any I/O (fix the `--help` bug)
  - Resolves `gait_dir` from parsed path or cwd
  - Calls `create_dir_all` so a path arg creates the directory
  - Drops all `repo_url` / submodule code
  - Drops the duplicate `.gitignore` and `gait.jsonc` writes (now in `scaffold_gait_dir`)
  - Drops the duplicate `sessions/` `create_dir_all` (now in `scaffold_gait_dir`)
  - Stages `agents/default/AGENT.md` instead of `AGENTS.md`
  - Calls `print_init_summary` instead of the ad-hoc eprintln chain

- [ ] Build to catch any compile errors:
  ```bash
  cd /Users/aicoder/Documents/ideation/gait/gait-rs && \
    cargo build --bin gait 2>&1 > /tmp/gait-build-output.txt; echo "exit: $?"
  grep "^error" /tmp/gait-build-output.txt | head -20
  ```

- [ ] Run all tests:
  ```bash
  cd /Users/aicoder/Documents/ideation/gait/gait-rs && \
    cargo test --bin gait 2>&1 > /tmp/gait-test-output.txt; echo "exit: $?"
  grep "FAILED\|test result\|^error" /tmp/gait-test-output.txt
  ```
  Expected: all pass.

- [ ] Commit:
  ```
  git add src/bin/gait.rs
  git commit -m "$(cat <<'EOF'
  feat(init): rewrite cmd_init — path arg, --help safety, Phase 4 staging, hint table

  - gait init [<path>]: no arg = cwd; path arg = create_dir_all then init there (like git init)
  - --help/-h detected by parse_init_args before any I/O; exits 0 with no side effects
    (fixes the bug that silently committed to the caller's repo when --help was passed)
  - scaffold_gait_dir now owns all dir/file creation; cmd_init no longer duplicates writes
  - Staged files: gait.jsonc, .gitignore, agents/default/AGENT.md — AGENTS.md dropped
  - print_init_summary: tab-aligned table of scaffolded files with committed/ignored status
  - Submodule/codebase relic (repo_url branch) deleted entirely

  Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
  EOF
  )"
  ```

---

### Task 5 — Update top-level `usage()` for the new `init` signature

**Files:** `src/bin/gait.rs`

**Steps:**

- [ ] Locate line 96 (the `init` line in `usage()`):
  ```rust
  eprintln!("  init     [<repo-url>]");
  ```
  Replace with:
  ```rust
  eprintln!("  init     [<path>]");
  ```

- [ ] Build and test:
  ```bash
  cd /Users/aicoder/Documents/ideation/gait/gait-rs && \
    cargo build --bin gait 2>&1 > /tmp/gait-build-output.txt; echo "exit: $?"
  grep "^error" /tmp/gait-build-output.txt | head -10
  ```

- [ ] Commit:
  ```
  git add src/bin/gait.rs
  git commit -m "$(cat <<'EOF'
  chore(usage): update gait init usage line — drop repo-url, show [<path>]

  Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
  EOF
  )"
  ```

---

### Task 6 — Install, smoke-test, and verify the full flow

**Steps:**

- [ ] Install the binary:
  ```bash
  cd /Users/aicoder/Documents/ideation/gait/gait-rs && make install
  ```

- [ ] Verify `--help` exits 0 with no side effects (run from your codebase to confirm it
  does NOT touch it):
  ```bash
  cd /Users/aicoder/Documents/ideation/gait && \
    /Users/aicoder/develop/gait init --help
  echo "exit: $?"
  git status   # must show no changes
  ```
  Expected: usage printed, exit 0, git status clean.

- [ ] Verify `gait init` in a fresh tmp dir creates the correct layout:
  ```bash
  rm -rf /tmp/gait-init-test && \
  /Users/aicoder/develop/gait init /tmp/gait-init-test && \
  find /tmp/gait-init-test -not -path '*/\.git*' | sort
  ```
  Expected output (order may vary):
  ```
  /tmp/gait-init-test
  /tmp/gait-init-test/.gitignore
  /tmp/gait-init-test/USER.md
  /tmp/gait-init-test/agents
  /tmp/gait-init-test/agents/default
  /tmp/gait-init-test/agents/default/AGENT.md
  /tmp/gait-init-test/gait.jsonc
  /tmp/gait-init-test/sessions
  /tmp/gait-init-test/skills
  ```

- [ ] Verify hint table is printed:
  Expected (stderr):
  ```
  [gait] initialized gait-dir at /tmp/gait-init-test

    file                             purpose                                        git
    gait.jsonc                       model + tool-permission config                 committed
    agents/default/AGENT.md          the agent's role and instructions              committed
    USER.md                          your personal instructions (not shared)        ignored
    skills/                          installed skills  (skills/<n>/SKILL.md)        committed*
    sessions/                        turn history                                   ignored

    * empty dir; git-tracked when you add skills

    next: gait run --task "..."
  ```

- [ ] Verify the initial git commit staged the correct files:
  ```bash
  cd /tmp/gait-init-test && git show --stat HEAD
  ```
  Expected: `gait.jsonc`, `.gitignore`, `agents/default/AGENT.md` present; `AGENTS.md` absent.

- [ ] Verify `.gitignore` content:
  ```bash
  cat /tmp/gait-init-test/.gitignore
  ```
  Expected (order may vary, all three lines present):
  ```
  sessions/
  USER.md
  .gait-ws.lock
  ```

- [ ] Verify `gait init` with no args (cwd) works — run in a fresh empty dir:
  ```bash
  mkdir /tmp/gait-cwd-test && cd /tmp/gait-cwd-test && \
    /Users/aicoder/develop/gait init && \
    ls -la
  ```
  Expected: same layout in `/tmp/gait-cwd-test`.

- [ ] Verify idempotency — re-running in the same dir does not error or clobber:
  ```bash
  /Users/aicoder/develop/gait init /tmp/gait-init-test
  echo "exit: $?"
  ```
  Expected: exits 0 (may print summary again; USER.md must retain its content if edited).

---

## Edge cases and guard rails

| Scenario | Expected behavior |
|---|---|
| `gait init --help` | prints usage, exit 0, zero side effects |
| `gait init -h` | same |
| `gait init /new/nested/path` | `create_dir_all` creates all intermediate dirs, then inits |
| `gait init .` | resolves `.` to absolute cwd, inits there |
| `gait init` in an existing git repo | `init_workspace` is idempotent (git init in an existing repo is a no-op); scaffold skips existing files |
| `gait init` when agents/default/AGENT.md already has content | not clobbered |
| `gait init` when USER.md already has content | not clobbered |
| `.gitignore` already has all entries | no duplicate lines added |
| No git user config | `Signature::now("gait", "gait@localhost")` fallback used (line 386 pattern reused) |

---

## What is NOT in scope

- Migrating existing workspaces from `agents/default.md` (flat) to `agents/default/AGENT.md`
  (directory form) — that is Phase 4's migration path, not `gait init`'s job.
- `gait.jsonc` schema changes — the default template written by `scaffold_gait_dir` matches
  the current schema; any schema evolution is a separate task.
- `--reinit` / force-reinit flag — out of scope; idempotency handles the re-run case.
- `skills/<n>/SKILL.md` skeleton generation — `skills/` is an empty dir; content is up to
  the user or future `gait skill new` command.

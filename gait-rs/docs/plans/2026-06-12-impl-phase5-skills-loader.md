# Phase 5 — Skills Disk-Loader + Progressive Disclosure

**Status:** READY TO IMPLEMENT  
**Depends on:** Phase 4 complete (`resolve_markdown_context_path(gait_dir, Skill, name) → skills/<name>/SKILL.md` exists).

---

## Goal

Replace inline `gait.jsonc` skill bodies with a disk scanner that reads
`<gait_dir>/skills/<name>/SKILL.md`. Disk presence = *available* (installed).
Enabled = config agent-state (the `AgentState.skills` set from Phase 3). No
inline `content` field needed in `gait.jsonc`.

The progressive-disclosure mechanism (name+description in `<loadable-index>`;
body injected on `load(name, true)`) is already live — this phase wires the
scanner into it so skills come from disk instead of from JSON.

---

## Architecture

```
skills/
  tdd/
    SKILL.md          ← frontmatter (name, description) + markdown body
  review/
    SKILL.md
```

**SKILL.md frontmatter format** (reuses `agent_frontmatter.rs` `split_frontmatter`):

```markdown
---
name: tdd
description: Test-driven development workflow
---
# TDD

1. Write a failing test first.
2. Make it pass with the minimum code.
3. Refactor.
```

**Data flow (after Phase 5):**

```
disk scan  →  Vec<SkillDef>  →  Profile.skills  →  ResolvedProfile.skill_registry
                                                  →  build_loadable_index entries
                                                  →  LoadTool.execute(name, true)
                                                       returns def.content (body)
```

`SkillDef` (`config.rs:44`) already has `{ name, description, content, enabled }`.
The scanner produces these from disk; `Profile::from_config_and_agent` merges them
with the `AgentState.skills` enabled-set (Phase 3) rather than pulling from
`config.skills`.

---

## Tech Stack / Key Constraints

- **Rust stable** — `std::fs::read_dir` for the scan; no `walkdir` dep needed (one
  level deep: `skills/<name>/SKILL.md`).
- **Reuse `agent_frontmatter::split_frontmatter`** — it is `pub(crate)` (or
  internal); expose if needed. The SKILL.md parser calls `split_frontmatter` then
  parses YAML with `serde_yaml` (already a dep via `agent_frontmatter.rs`).
- **`SkillDef.enabled`** — the scanner sets `enabled: true` for all discovered
  skills (disk presence = available). The `enabled` field's runtime semantics stay
  with `config_enabled` / `AgentState`.
- **`GaitConfig.skills` stays in the struct** (backward compat: existing `gait.jsonc`
  files with inline `skills:[]` keep parsing). Phase 5 changes who *populates*
  `Profile.skills`, not the struct layout.
- **No new crate deps.** `serde_yaml` already used in `agent_frontmatter.rs`.
- **Test commands (from `gait-rs/CLAUDE.md`):**
  ```bash
  cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test 2>&1 > /tmp/gait-test-output.txt; echo "exit: $?"
  grep "^error" /tmp/gait-test-output.txt | head -30
  grep "FAILED\|test result" /tmp/gait-test-output.txt
  ```

---

## Files

| File | Role |
|---|---|
| `src/agent_frontmatter.rs` | Expose `split_frontmatter` (currently `fn`, make `pub(crate)`) |
| `src/skills.rs` | **NEW** — disk scanner `scan_skills(gait_dir) → Vec<SkillDef>` |
| `src/lib.rs` | Add `pub(crate) mod skills;` |
| `src/profile.rs` | `Profile::from_config_and_agent` — merge disk skills into `self.skills` |
| `src/config.rs` | Remove `skills` field from `GaitConfig` **OR** keep it as legacy fallback (see Task 4) |
| `src/tools/load.rs` | Tests: update `make_skill_registry()` to use a tempdir scanner |

---

## Tasks

---

### Task 1 — Expose `split_frontmatter` from `agent_frontmatter.rs`

The scanner in `src/skills.rs` will call `split_frontmatter`. Today it is a
private `fn` in `agent_frontmatter.rs` (`agent_frontmatter.rs:100`).

**Files:** `src/agent_frontmatter.rs`

**Steps:**

- [ ] Read `src/agent_frontmatter.rs` lines 100–115.
- [ ] Change `fn split_frontmatter` → `pub(crate) fn split_frontmatter`.
- [ ] Run tests — confirm no regression:
  ```bash
  cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test 2>&1 > /tmp/gait-test-output.txt; echo "exit: $?"
  grep "FAILED\|test result" /tmp/gait-test-output.txt
  ```
  Expected: `test result: ok. N passed; 0 failed`.
- [ ] Commit:
  ```
  refactor(agent_frontmatter): expose split_frontmatter as pub(crate)

  Skills disk-loader in src/skills.rs will reuse it to parse SKILL.md
  frontmatter without duplicating the fence-detection logic.

  Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
  ```

---

### Task 2 — Add `src/skills.rs` with the disk scanner

**Files:** `src/skills.rs` (new), `src/lib.rs`

#### `scan_skills` contract

```rust
/// Scan `<gait_dir>/skills/*/SKILL.md` and return one `SkillDef` per valid file.
///
/// A file is valid if it has a non-empty `name` key in its YAML frontmatter and
/// a non-empty markdown body.  Files that fail to parse are silently skipped with
/// an `eprintln!("[gait] warning: …")` diagnostic (matching the style used in
/// `config.rs:load_config`).
///
/// `SkillDef.enabled` is set to `true` for all discovered skills (disk presence
/// = available).  Caller is responsible for applying the enabled-set filter.
pub fn scan_skills(gait_dir: &std::path::Path) -> Vec<crate::config::SkillDef>
```

#### `parse_skill_file` contract

```rust
/// Parse one SKILL.md file into a SkillDef.
/// Returns `None` if frontmatter is missing, `name` key is absent/empty, or body
/// is empty.
fn parse_skill_file(path: &std::path::Path) -> Option<crate::config::SkillDef>
```

#### Full implementation sketch

```rust
use std::path::Path;
use crate::config::SkillDef;
use crate::agent_frontmatter::split_frontmatter;

pub fn scan_skills(gait_dir: &Path) -> Vec<SkillDef> {
    let skills_dir = gait_dir.join("skills");
    if !skills_dir.is_dir() { return vec![]; }

    let mut out = Vec::new();
    let entries = match std::fs::read_dir(&skills_dir) {
        Ok(e) => e,
        Err(err) => {
            eprintln!("[gait] warning: could not read skills dir {}: {err}", skills_dir.display());
            return vec![];
        }
    };
    for entry in entries.flatten() {
        let skill_md = entry.path().join("SKILL.md");
        if !skill_md.is_file() { continue; }
        match parse_skill_file(&skill_md) {
            Some(def) => out.push(def),
            None => eprintln!("[gait] warning: skipping invalid skill file {}", skill_md.display()),
        }
    }
    out
}

fn parse_skill_file(path: &Path) -> Option<SkillDef> {
    let content = std::fs::read_to_string(path).ok()?;
    let (fm, body) = split_frontmatter(&content);
    if fm.is_empty() { return None; }
    let yaml: serde_yaml::Value = serde_yaml::from_str(fm).ok()?;
    let name = yaml.get("name").and_then(|v| v.as_str()).filter(|s| !s.is_empty())?.to_string();
    let description = yaml.get("description").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let body = body.trim_end().to_string();
    if body.is_empty() { return None; }
    Some(SkillDef { name, description, content: body, enabled: true })
}
```

**Steps:**

- [ ] Write `src/skills.rs` with the two functions above plus a `#[cfg(test)]` module
  (see tests below).
- [ ] Add `pub(crate) mod skills;` to `src/lib.rs` (find the `mod` block).
- [ ] Run tests:
  ```bash
  cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test 2>&1 > /tmp/gait-test-output.txt; echo "exit: $?"
  grep "^error" /tmp/gait-test-output.txt | head -30
  grep "FAILED\|test result" /tmp/gait-test-output.txt
  ```
  Expected: all pass, zero errors.

#### Tests for `src/skills.rs`

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_skill(dir: &TempDir, name: &str, fm: &str, body: &str) {
        let skill_dir = dir.path().join("skills").join(name);
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            format!("---\n{}---\n{}", fm, body),
        ).unwrap();
    }

    #[test]
    fn scanner_returns_empty_when_no_skills_dir() {
        let dir = TempDir::new().unwrap();
        assert!(scan_skills(dir.path()).is_empty());
    }

    #[test]
    fn scanner_reads_one_skill() {
        let dir = TempDir::new().unwrap();
        write_skill(&dir, "tdd",
            "name: tdd\ndescription: Test-driven development workflow\n",
            "# TDD\n\n1. Write failing test.\n");
        let skills = scan_skills(dir.path());
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "tdd");
        assert_eq!(skills[0].description, "Test-driven development workflow");
        assert!(skills[0].content.contains("TDD"));
        assert!(skills[0].enabled);
    }

    #[test]
    fn scanner_reads_multiple_skills() {
        let dir = TempDir::new().unwrap();
        write_skill(&dir, "tdd",   "name: tdd\ndescription: TDD workflow\n",    "# TDD\nbody\n");
        write_skill(&dir, "review","name: review\ndescription: Code review\n", "# Review\nbody\n");
        let mut skills = scan_skills(dir.path());
        skills.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(skills.len(), 2);
        assert_eq!(skills[0].name, "review");
        assert_eq!(skills[1].name, "tdd");
    }

    #[test]
    fn scanner_skips_skill_with_no_name_in_frontmatter() {
        let dir = TempDir::new().unwrap();
        write_skill(&dir, "bad", "description: No name here\n", "# body\n");
        assert!(scan_skills(dir.path()).is_empty());
    }

    #[test]
    fn scanner_skips_skill_with_empty_body() {
        let dir = TempDir::new().unwrap();
        let skill_dir = dir.path().join("skills").join("empty");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: empty\ndescription: x\n---\n   \n",
        ).unwrap();
        assert!(scan_skills(dir.path()).is_empty());
    }

    #[test]
    fn scanner_skips_dir_with_no_skill_md() {
        let dir = TempDir::new().unwrap();
        let skill_dir = dir.path().join("skills").join("orphan");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(skill_dir.join("README.md"), "hi").unwrap();
        assert!(scan_skills(dir.path()).is_empty());
    }

    #[test]
    fn description_defaults_to_empty_when_absent() {
        let dir = TempDir::new().unwrap();
        write_skill(&dir, "nodesc", "name: nodesc\n", "# Body\ncontent\n");
        let skills = scan_skills(dir.path());
        assert_eq!(skills[0].description, "");
    }
}
```

- [ ] Commit:
  ```
  feat(skills): add disk scanner — scan_skills(gait_dir) → Vec<SkillDef>

  Reads <gait_dir>/skills/*/SKILL.md, parses YAML frontmatter (name,
  description) + markdown body into SkillDef.  Reuses split_frontmatter
  from agent_frontmatter.rs.  Invalid or empty files are silently skipped
  with a [gait] warning diagnostic.

  Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
  ```

---

### Task 3 — Wire disk scanner into `Profile::from_config_and_agent`

Today `Profile.skills` is populated from `config.skills` (`profile.rs:86`).
After Phase 5, skills come from the disk scanner. The scanner result merges with
`config.skills` as a **fallback / legacy** path (see Task 4 for the removal plan).

**Files:** `src/profile.rs`

**Steps:**

- [ ] Read `src/profile.rs` lines 49–92 (the `from_config_and_agent` body).

- [ ] After building `instructions` (line 64), add a disk scan and merge:

  ```rust
  // Phase 5: skills come from disk first; fall back to config.skills for
  // backward compat with gait.jsonc inline bodies.
  let mut disk_skills = crate::skills::scan_skills(gait_dir);
  // Merge: disk wins over config for same name; config-only entries kept.
  let config_skill_names: std::collections::HashSet<&str> =
      disk_skills.iter().map(|s| s.name.as_str()).collect();
  for s in &config.skills {
      if !config_skill_names.contains(s.name.as_str()) {
          disk_skills.push(s.clone());
      }
  }
  let merged_skills = disk_skills;
  ```

- [ ] Replace `skills: config.skills.clone()` (line 86) with `skills: merged_skills`.

- [ ] Update `config_enabled` computation (lines 75–78) to iterate `merged_skills`
  instead of `config.skills`:

  ```rust
  let config_enabled: HashSet<String> = config.mcp_servers.iter()
      .filter(|s| s.enabled).map(|s| s.name.clone())
      .chain(merged_skills.iter().filter(|s| s.enabled).map(|s| s.name.clone()))
      .collect();
  ```

  Note: after Phase 3 the enabled-set comes from `AgentState.skills`. This
  `config_enabled` path is the pre-Phase-3 seed. Keep the iteration consistent.

- [ ] Run tests:
  ```bash
  cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test 2>&1 > /tmp/gait-test-output.txt; echo "exit: $?"
  grep "^error" /tmp/gait-test-output.txt | head -30
  grep "FAILED\|test result" /tmp/gait-test-output.txt
  ```
  Expected: all pass.

- [ ] Add a test to `profile.rs` that verifies disk skills surface in `profile.skills`:

  ```rust
  #[test]
  fn disk_skills_populate_profile_skills() {
      use tempfile::TempDir;
      let dir = TempDir::new().unwrap();
      // Write a SKILL.md on disk
      let skill_dir = dir.path().join("skills").join("tdd");
      std::fs::create_dir_all(&skill_dir).unwrap();
      std::fs::write(
          skill_dir.join("SKILL.md"),
          "---\nname: tdd\ndescription: TDD workflow\n---\n# TDD\nbody\n",
      ).unwrap();
      let config = GaitConfig::default();   // no inline skills
      let profile = Profile::from_config_and_agent(&config, dir.path(), None, None, None);
      assert_eq!(profile.skills.len(), 1);
      assert_eq!(profile.skills[0].name, "tdd");
      assert!(profile.skills[0].content.contains("TDD"));
  }

  #[test]
  fn config_skills_kept_when_no_disk_skill() {
      use tempfile::TempDir;
      let dir = TempDir::new().unwrap();
      // skills/ dir absent — config inline skill must survive
      let config = GaitConfig {
          skills: vec![crate::config::SkillDef {
              name: "inline".into(),
              description: "inline desc".into(),
              content: "# Inline body".into(),
              enabled: true,
          }],
          ..GaitConfig::default()
      };
      let profile = Profile::from_config_and_agent(&config, dir.path(), None, None, None);
      assert_eq!(profile.skills.len(), 1);
      assert_eq!(profile.skills[0].name, "inline");
  }

  #[test]
  fn disk_skill_wins_over_config_skill_same_name() {
      use tempfile::TempDir;
      let dir = TempDir::new().unwrap();
      let skill_dir = dir.path().join("skills").join("tdd");
      std::fs::create_dir_all(&skill_dir).unwrap();
      std::fs::write(
          skill_dir.join("SKILL.md"),
          "---\nname: tdd\ndescription: Disk version\n---\n# Disk TDD\n",
      ).unwrap();
      let config = GaitConfig {
          skills: vec![crate::config::SkillDef {
              name: "tdd".into(),
              description: "Config version".into(),
              content: "# Config TDD".into(),
              enabled: true,
          }],
          ..GaitConfig::default()
      };
      let profile = Profile::from_config_and_agent(&config, dir.path(), None, None, None);
      assert_eq!(profile.skills.len(), 1, "should deduplicate same-named skill");
      assert_eq!(profile.skills[0].description, "Disk version", "disk wins");
  }
  ```

- [ ] Run tests again after adding the new tests:
  ```bash
  cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test 2>&1 > /tmp/gait-test-output.txt; echo "exit: $?"
  grep "FAILED\|test result" /tmp/gait-test-output.txt
  ```
  Expected: all pass.

- [ ] Commit:
  ```
  feat(profile): wire disk skill scanner into Profile::from_config_and_agent

  scan_skills(gait_dir) populates Profile.skills; config.skills kept as
  legacy fallback for inline gait.jsonc bodies.  Disk wins on name
  collision.  Three new profile tests: disk-wins, config-fallback,
  dedup.

  Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
  ```

---

### Task 4 — Deprecate inline `skills:` in `gait.jsonc` (soft removal)

The spec (O4: disk-only) says no inline skill bodies in `gait.jsonc`. However,
existing tests (e.g. `config.rs:382-395`, `load.rs:95-102`) construct skills
inline. Hard-remove the field now would break those tests and backward compat.

**Decision:** soft deprecation — keep `GaitConfig.skills` for now; add a
`#[deprecated]` doc comment; emit a `[gait] warning` at load time if the field
is non-empty. Full removal is a follow-up cleanup after all call sites migrate.

**Files:** `src/config.rs`

**Steps:**

- [ ] Read `src/config.rs` lines 78–103 (`GaitConfig` struct and `load_config`).

- [ ] Add a deprecation warning in `load_config` after deserialization, after the
  `GaitConfig` is produced (line ~155):

  ```rust
  // O4: inline skill bodies in gait.jsonc are deprecated; use skills/<name>/SKILL.md.
  if !config.skills.is_empty() {
      eprintln!("[gait] warning: gait.jsonc `skills:` field is deprecated (O4). \
                 Move skill bodies to skills/<name>/SKILL.md");
  }
  ```

- [ ] Add a `/// Deprecated (O4): skill bodies should live in skills/<name>/SKILL.md.` doc
  comment above the `skills` field in `GaitConfig`.

- [ ] Run tests:
  ```bash
  cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test 2>&1 > /tmp/gait-test-output.txt; echo "exit: $?"
  grep "FAILED\|test result" /tmp/gait-test-output.txt
  ```
  Expected: all pass (tests that use inline skills still work; the warning is
  `eprintln!` only, not a hard error).

- [ ] Commit:
  ```
  chore(config): deprecate inline skills field in gait.jsonc (O4)

  Emits a [gait] warning at load_config time when gait.jsonc contains
  a non-empty skills: array.  The field is kept for backward compat;
  hard removal is a follow-up after call sites migrate to disk layout.

  Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
  ```

---

### Task 5 — End-to-end integration test: scan → load → index

Verify the full path: disk skill → `Profile` → `ResolvedProfile.skill_registry`
→ `build_loadable_index` entry → `LoadTool.execute` returns body.

**Files:** `src/tools/load.rs` (add an integration-style test using tempdir)

**Steps:**

- [ ] Read `src/tools/load.rs` lines 79–154 (existing tests + helpers).

- [ ] Add the following test to the `mod tests` block:

  ```rust
  #[tokio::test]
  async fn disk_skill_loads_via_load_tool() {
      use tempfile::TempDir;
      let dir = TempDir::new().unwrap();
      let skill_dir = dir.path().join("skills").join("retro");
      std::fs::create_dir_all(&skill_dir).unwrap();
      std::fs::write(
          skill_dir.join("SKILL.md"),
          "---\nname: retro\ndescription: Retrospective interview workflow\n---\n# Retro\nDo a retro.\n",
      ).unwrap();

      // Scanner builds the registry directly from disk (mimics profile.rs path)
      let skill_registry = crate::skills::scan_skills(dir.path());
      assert_eq!(skill_registry.len(), 1, "scanner must find the disk skill");
      assert_eq!(skill_registry[0].name, "retro");
      assert_eq!(skill_registry[0].description, "Retrospective interview workflow");

      // LoadTool returns the markdown body
      let tool = LoadTool::new(vec![], skill_registry);
      let result = tool.execute(serde_json::json!({"name":"retro","enabled":true})).await.unwrap();
      assert!(result.as_str().unwrap().contains("Retro"));
  }

  #[test]
  fn disk_skill_appears_in_loadable_index() {
      use tempfile::TempDir;
      use crate::types::{LoadableEntry, LoadableKind};
      let dir = TempDir::new().unwrap();
      let skill_dir = dir.path().join("skills").join("refactor");
      std::fs::create_dir_all(&skill_dir).unwrap();
      std::fs::write(
          skill_dir.join("SKILL.md"),
          "---\nname: refactor\ndescription: Refactoring patterns\n---\n# Refactor\nclean code.\n",
      ).unwrap();
      let skills = crate::skills::scan_skills(dir.path());
      let entries: Vec<LoadableEntry> = skills.iter().map(|s| LoadableEntry {
          name: s.name.clone(),
          kind: LoadableKind::Skill,
          description: s.description.clone(),
          enabled: s.enabled,
      }).collect();
      let idx = crate::turn::build_loadable_index(&entries);
      assert!(idx.contains("refactor"), "index must list skill name");
      assert!(idx.contains("Refactoring patterns"), "index must show description");
      assert!(idx.contains("enabled=true"), "disk skill defaults to enabled=true");
  }
  ```

- [ ] Run tests:
  ```bash
  cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test 2>&1 > /tmp/gait-test-output.txt; echo "exit: $?"
  grep "^error" /tmp/gait-test-output.txt | head -30
  grep "FAILED\|test result" /tmp/gait-test-output.txt
  ```
  Expected: all pass.

- [ ] Commit:
  ```
  test(skills): end-to-end — disk scan → load tool → loadable index

  Two integration tests: (1) scan_skills() → LoadTool.execute() returns
  the markdown body; (2) scan_skills() entries appear correctly in
  build_loadable_index() with name, description, and enabled=true.

  Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
  ```

---

### Task 6 — Final build + test pass, dogfood smoke test

**Steps:**

- [ ] Full clean build:
  ```bash
  cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo build 2>&1 > /tmp/gait-build-output.txt; echo "exit: $?"
  grep "^error" /tmp/gait-build-output.txt | head -20
  ```
  Expected: `exit: 0`, zero errors.

- [ ] Full test suite:
  ```bash
  cd /Users/aicoder/Documents/ideation/gait/gait-rs && cargo test 2>&1 > /tmp/gait-test-output.txt; echo "exit: $?"
  grep "FAILED\|test result" /tmp/gait-test-output.txt
  ```
  Expected: `test result: ok. N passed; 0 failed`.

- [ ] Install and smoke-test with dogfood workspace:
  ```bash
  cd /Users/aicoder/Documents/ideation/gait/gait-rs && make install
  mkdir -p /Users/aicoder/gait-test-ws/skills/hello
  cat > /Users/aicoder/gait-test-ws/skills/hello/SKILL.md <<'EOF'
  ---
  name: hello
  description: Hello world skill for smoke testing
  ---
  # Hello
  Say hello to the user.
  EOF
  gait --gait-dir /Users/aicoder/gait-test-ws --model minimax/MiniMax-M3 \
       "what skills are available? call load(hello, true) and tell me what it says"
  ```
  Expected: the agent's `<loadable-index>` includes `skill hello enabled=true —
  Hello world skill for smoke testing`; after `load("hello", true)` the result
  contains `# Hello`.

- [ ] Commit if any fixes were needed during smoke test:
  ```
  fix(skills): smoke-test fixes from dogfood run

  Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
  ```

---

## Summary of Changes

| What | Where | Before | After |
|---|---|---|---|
| `split_frontmatter` visibility | `agent_frontmatter.rs:100` | `fn` (private) | `pub(crate) fn` |
| New disk scanner | `src/skills.rs` (new) | — | `scan_skills(gait_dir)` |
| Module registration | `src/lib.rs` | — | `pub(crate) mod skills;` |
| `Profile.skills` source | `profile.rs:86` | `config.skills.clone()` | `scan_skills(gait_dir)` + config fallback |
| `GaitConfig.skills` | `config.rs:92` | silent | deprecation warning at load_config |

No changes to `src/tools/load.rs` implementation — the `LoadTool.execute` skill
path at line 69–71 already works: it looks up `def.content` from the
`skill_registry`, which now comes from disk. Only tests are added.

No changes to `turn.rs` — `build_loadable_index` and the `skill_registry` loop
(lines 674–681) already iterate `skill_registry` correctly; the entries will now
contain disk-discovered skills automatically.

---

## SKILL.md Reference

```
---
name: <skill-name>          # required; must be non-empty
description: <one-liner>    # optional; shown in <loadable-index>
---
<markdown body>              # required; must be non-empty after trim
                             # injected verbatim by load(name, true)
```

- Frontmatter delimiter: `---` on its own line (same as `agent_frontmatter.rs:3`).
- YAML parser: `serde_yaml` (already a dependency).
- Only `name` and `description` are read; other YAML keys are ignored.
- The markdown body is everything after the closing `---\n`.

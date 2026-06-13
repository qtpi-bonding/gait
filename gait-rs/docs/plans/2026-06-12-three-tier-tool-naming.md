---
status: done
---

# Three-Tier Tool Naming Refactor

**Goal:** Establish a consistent, enforced vocabulary for the three tiers of tool lifecycle: registered (load/unload), enabled (enable/disable), permissioned (allow/deny). Rename CLI commands and proto enum values to match, and introduce `RegisteredTools` → `EnabledTools` → `PermissionedTools` as a typed pipeline in Rust.

**Architecture:**

The three tiers are distinct and sequential:
- **Tier 1 — Registered**: tool exists in the registry (`gait.jsonc`). CLI: `register`/`unregister`.
- **Tier 2 — Enabled**: registered tool is injected into the LLM's context. CLI: `enable`/`disable`.
- **Tier 3 — Permissioned**: enabled tool's execution rules are attached (allow/deny patterns). CLI: `allow`/`deny`.

`run_turn` receives `PermissionedTools` — a single struct that encodes all three tiers by construction.

**Tech Stack:** Rust, protobuf (prost), gait CLI

---

## File Map

- Modify: `proto/log.proto` — rename `DECISION_APPROVE` → `DECISION_ALLOW`
- Modify: `src/proto/events.rs` — update string→Decision parsing, Decision→string serialization
- Modify: `src/permission.rs` — `PermissionChoice::Reject` → `Deny`, `ApproveScope` → `AllowScope`, `execute_approve` → `execute_allow`, `"approve"` literals → `"allow"`
- Modify: `src/ui/permission.rs` — `Reject` → `Deny`, `"reject"` → `"deny"` in parser/tests
- Modify: `src/ui/builder.rs` — `Reject` → `Deny`, `"approve"` literals → `"allow"`
- Create: `src/tools/tiers.rs` — `RegisteredTools`, `EnabledTools`, `PermissionedTools`
- Modify: `src/profile.rs` — build `PermissionedTools` pipeline, expose from `ResolvedProfile`
- Modify: `src/turn.rs` — receive `PermissionedTools`, remove separate `allow`/`deny` params
- Modify: `src/bin/gait.rs` — CLI renames + dispatch, `cmd_load`→`cmd_register`, `cmd_approve`→`cmd_allow`, etc.
- Modify: `src/session/retrace.rs` — `"approve"` → `"allow"` in test fixtures
- Modify: `src/tools/load.rs` — **decision required** (see Task 5)

---

## Task 1: Proto — rename `DECISION_APPROVE` → `DECISION_ALLOW`

Wire-safe: proto enums are serialized by number, not name. Renaming `DECISION_APPROVE = 1` to `DECISION_ALLOW = 1` does not break existing logs.

**Files:** `proto/log.proto`, `src/proto/events.rs`

- [ ] In `proto/log.proto` line 271, change:
  ```protobuf
  // before
  enum Decision { DECISION_UNSPECIFIED = 0; DECISION_APPROVE = 1; DECISION_DENY = 2; }
  // after
  enum Decision { DECISION_UNSPECIFIED = 0; DECISION_ALLOW = 1; DECISION_DENY = 2; }
  ```

- [ ] Regenerate proto bindings (or update the hand-maintained generated file):
  ```bash
  cd gait-rs && cargo build 2>&1 | grep "^error" | head -20
  ```
  The generated `Decision::Approve` variant will become `Decision::Allow` — the compiler will flag every usage.

- [ ] In `src/proto/events.rs` update the string→Decision match (currently line 412):
  ```rust
  // before
  "approve" => pb::Decision::Approve,
  // after
  "allow"   => pb::Decision::Allow,
  "approve" => pb::Decision::Allow,  // backward-compat alias for old logs
  ```
  Keep the alias so existing `log.jsonl` files with `"approve"` still parse correctly.

- [ ] Add a Decision→string serializer if one exists, or grep for any `Decision::Approve` arm in match statements. Change all to `Decision::Allow`.

- [ ] Run tests:
  ```bash
  cargo test 2>&1 > /tmp/gait-test-output.txt; echo "exit: $?"
  grep "FAILED\|test result" /tmp/gait-test-output.txt
  ```

- [ ] Commit: `refactor(proto): rename DECISION_APPROVE → DECISION_ALLOW; keep "approve" alias for old logs`

---

## Task 2: Rust renames — `PermissionChoice`, `ApproveScope`, decision strings

**Files:** `src/permission.rs`, `src/ui/permission.rs`, `src/ui/builder.rs`, `src/session/retrace.rs`, `src/turn.rs`

- [ ] In `src/permission.rs`:
  - `PermissionChoice::Reject` → `PermissionChoice::Deny`
  - `ApproveScope` → `AllowScope` (enum name + all variants if named generically)
  - `fn execute_approve` → `fn execute_allow`
  - `decision: "approve".to_string()` → `decision: "allow".to_string()` (line 118)

- [ ] In `src/ui/permission.rs`:
  - `PermissionChoice::Reject` → `PermissionChoice::Deny` (lines 29, 51, 90, 125)
  - `"r" || "reject"` parser → `"r" || "deny"` (keep `"reject"` as alias for existing users)
  - Update test: `parse_permission_response("reject", ...)` → `parse_permission_response("deny", ...)`

- [ ] In `src/ui/builder.rs`:
  - `PermissionChoice::Reject` → `PermissionChoice::Deny` (line 155)
  - `decision: "approve"` → `decision: "allow"` (lines 165, 167, and any others)

- [ ] In `src/session/retrace.rs` test fixtures:
  - All `decision: "approve".into()` → `decision: "allow".into()` (lines 776, 1111, 1114, 1128, 1144)

- [ ] In `src/turn.rs` test fixtures:
  - `decision: "approve".into()` → `decision: "allow".into()` (lines 1837, 1985)
  - `Decision::Approve` → `Decision::Allow` (line 574)

- [ ] Run tests, fix any remaining references:
  ```bash
  cargo test 2>&1 > /tmp/gait-test-output.txt; echo "exit: $?"
  grep "FAILED\|test result" /tmp/gait-test-output.txt
  ```

- [ ] Commit: `refactor: PermissionChoice::Reject→Deny, ApproveScope→AllowScope, "approve"→"allow" strings`

---

## Task 3: Three-tier struct types

**Files:** `src/tools/tiers.rs` (new), `src/profile.rs`, `src/turn.rs`

- [ ] Create `src/tools/tiers.rs`:
  ```rust
  use crate::types::Tool;

  /// All tools loaded into the registry for this session.
  pub struct RegisteredTools(pub Vec<Box<dyn Tool + Send + Sync>>);

  /// Registered tools that are currently enabled — visible to the LLM.
  pub struct EnabledTools(pub Vec<Box<dyn Tool + Send + Sync>>);

  /// Enabled tools with their permission rules attached — what run_turn receives.
  pub struct PermissionedTools {
      pub tools: EnabledTools,
      pub allow: Vec<String>,
      pub deny: Vec<String>,
  }

  impl RegisteredTools {
      pub fn filter_enabled(self, enabled_names: &std::collections::HashSet<String>) -> EnabledTools {
          EnabledTools(
              self.0.into_iter()
                  .filter(|t| enabled_names.contains(t.name()))
                  .collect()
          )
      }
  }

  impl EnabledTools {
      pub fn with_permissions(self, allow: Vec<String>, deny: Vec<String>) -> PermissionedTools {
          PermissionedTools { tools: self, allow, deny }
      }

      pub fn as_slice(&self) -> &[Box<dyn Tool + Send + Sync>] {
          &self.0
      }
  }

  impl PermissionedTools {
      pub fn tools(&self) -> &[Box<dyn Tool + Send + Sync>] {
          self.tools.as_slice()
      }
  }
  ```

- [ ] Add `mod tiers; pub use tiers::{RegisteredTools, EnabledTools, PermissionedTools};` to `src/tools/mod.rs`.

- [ ] In `src/profile.rs`, update `ResolvedProfile`:
  - Replace `pub tools: Vec<Box<dyn Tool + Send + Sync>>`, `pub allow: Vec<String>`, `pub deny: Vec<String>` with `pub permissioned: PermissionedTools`
  - Update `Profile::resolve()` to build the pipeline:
    ```rust
    let registered = RegisteredTools(all_built_tools);
    let enabled = registered.filter_enabled(&config_enabled_names);
    let permissioned = enabled.with_permissions(profile.allow.clone(), profile.deny.clone());
    // ResolvedProfile { permissioned, ... }
    ```

- [ ] Update `ResolvedProfile::for_test` constructor to accept `PermissionedTools` or build it inline.

- [ ] In `src/turn.rs`, update `run_turn` signature:
  - Replace separate `tools`/`allow`/`deny` fields on `TurnCaps` with `permissioned: PermissionedTools`
  - Update all call sites: `caps.tools` → `caps.permissioned.tools()`, `caps.allow` → `caps.permissioned.allow`, `caps.deny` → `caps.permissioned.deny`

- [ ] Run tests:
  ```bash
  cargo test 2>&1 > /tmp/gait-test-output.txt; echo "exit: $?"
  grep "FAILED\|test result" /tmp/gait-test-output.txt
  ```

- [ ] Commit: `refactor: introduce RegisteredTools→EnabledTools→PermissionedTools pipeline`

---

## Task 4: CLI renames — `load`/`unload` → `register`/`unregister`, `approve`/`reject` → `allow`/`deny`

**File:** `src/bin/gait.rs`

- [ ] Rename functions:
  - `cmd_load` → `cmd_register`
  - `cmd_unload` → `cmd_unregister`
  - `cmd_approve` → `cmd_allow`
  - `cmd_reject` → `cmd_deny`

- [ ] Update dispatch match arms:
  ```rust
  // before
  "load"    => { ... cmd_load ... }
  "unload"  => { ... cmd_unload ... }
  "approve" => cmd_approve(&args[2..]),
  "reject"  => cmd_reject(&args[2..]),
  // after
  "register"   | "load"    => { ... cmd_register ... }   // alias old names
  "unregister" | "unload"  => { ... cmd_unregister ... }
  "allow"      | "approve" => cmd_allow(&args[2..]),
  "deny"       | "reject"  => cmd_deny(&args[2..]),
  ```
  Keeping the old names as aliases avoids breaking existing scripts. Add a deprecation `eprintln!` on the old names if desired.

- [ ] Update `usage()` strings to show new names (list alias in parens):
  ```
  register   <mcp|skill> <name> [flags]   (was: load)
  unregister <mcp|skill> <name>           (was: unload)
  allow      <session-id> [--pattern <pat>] [--scope <global|agent|once>] ...  (was: approve)
  deny       <session-id>                 (was: reject)
  ```

- [ ] Update all internal `eprintln!("usage: gait load ...")` → `"gait register ..."` messages inside the renamed functions.

- [ ] Update test function names and any `cmd_load(...)` / `cmd_unload(...)` calls in tests.

- [ ] Run tests:
  ```bash
  cargo test 2>&1 > /tmp/gait-test-output.txt; echo "exit: $?"
  grep "FAILED\|test result" /tmp/gait-test-output.txt
  ```

- [ ] Commit: `feat: rename CLI load→register, approve→allow, reject→deny; keep old names as aliases`

---

## Task 5: In-session `load` tool — decision

The runtime `load` tool (called by the LLM during a session to toggle MCP servers/skills) performs **tier-2 enable/disable** operations within a session. Its name `load` no longer matches the tier vocabulary since `load`/`unload` now means tier-1 register/unregister.

**Options:**
- A. Rename to `toggle` — neutral, describes what it does without implying a tier
- B. Rename to `enable` — matches tier-2 vocabulary exactly; the tool takes `enabled: bool` so `enable(name, enabled: false)` = disable
- C. Keep as `load` — it's what the LLM calls; changing it updates the harness prompt and breaks existing agents

**Recommended:** Option B (`enable`) — the tool already takes `enabled: bool`, so `enable(name, true)` / `enable(name, false)` maps cleanly to the tier-2 vocabulary. Update the harness prompt (`prompts/gait-harness.md`) to reflect the rename.

- [ ] Decide: rename in-session tool or keep as `load`
- [ ] If renaming: update `src/tools/load.rs` `fn name()` → `"enable"`
- [ ] Update permission patterns: `"load"` → `"enable"` in `src/tools/load.rs` line 44
- [ ] Update `prompts/gait-harness.md` tool reference from `load` → `enable`
- [ ] Update any `gait.jsonc` allow list entries that reference `"load"` or `"load:*"`
- [ ] Commit: `refactor: rename in-session load tool → enable to match tier-2 vocabulary`

---

## Task 6: Make install and smoke test

- [ ] `make install` from `gait-rs/`
- [ ] `gait register` — verify new command works
- [ ] `gait allow` / `gait deny` — verify new commands work
- [ ] `gait load` — verify alias still works with deprecation note
- [ ] `gait approve` — verify alias still works
- [ ] Run a dogfood session to verify tool list is built correctly through the pipeline

---

## Open questions

1. **`PermissionPolicy` proto message** — fields are already named `allow` and `deny`. No change needed. ✓
2. **Backward compat for `"approve"` in existing `log.jsonl` files** — handled by the alias in `events.rs` (Task 1).
3. **`config.append_allow_pattern` / `config.append_deny_pattern`** — function names already use `allow`/`deny`, no rename needed. ✓
4. **`build_permission_status(allow, deny)`** in `turn.rs` — signature stays the same since allow/deny are the right words for the parameters; the function itself doesn't need renaming.

# Phase 3a + 3b — `ActiveAgent` projection + delta events (impl plan)

> **Prereqs (assumed done):** Phase 1 (Projection trait gained `checkpoint()` +
> `run_projection` resets baseline on a checkpoint hit) and Phase 2 (proto rename
> `Manifest → Agent`, `Compaction → Summary`, with serde aliases + fixture test).
> This plan is written against that **post-rename world**: `pb::Agent`,
> `payload::Kind::Agent`, `pb::Summary`, `events::agent_msg`, `retrace::active_agent`
> (the Phase-1/2 renames of `manifest_msg`/`active_manifest`). Where a snippet shows a
> Phase-1/2 symbol, it is the renamed form, not new work.

Authoritative spec: `docs/plans/2026-06-12-agent-checkpoint-model.md` — §0 (M3, I2,
I3, I4), §2 (delta events), §3 (`ActiveAgent`), §4 (assembly), §6 (O2).

---

## Goal

Collapse `ActiveManifest` + `ActiveLoadables` into **one** projection, `ActiveAgent`,
folded from the same delta substrate as every other projection — **behaviour-identical
in 3a**, then **switched to typed deltas** (`Toggle` / `ModelChange`) in 3b.

- **3a:** Replace both old projections with `ActiveAgent` whose `State = AgentState`.
  `seed` built FROM CONFIG (M3). `checkpoint()` returns `AgentState::from(&pb::Agent)`
  on `Kind::Agent`. `apply` keeps folding the `load` **ToolCall** as a compat path
  (exactly as `ActiveLoadables` did) so `<loadable-index>` / enabled-names output is
  **byte-identical**. `Agent.tools` stays audit-only (I2). No `PermissionGrant` (I3).
- **3b:** Add proto `Toggle` + `ModelChange` deltas. Wire `load()` to emit a `Toggle`;
  `ActiveAgent::apply` folds `Kind::Toggle` (OR-Set per kind) + `Kind::ModelChange`
  (LWW); **remove** the ToolCall compat path. Re-baseline (editing `AGENT.md`) emits a
  fresh `Agent` snapshot = `active_agent(events)` with new `instructions` patched in
  (recompose-preserving, I4). No permission delta (I3); `ActivePermissions` untouched.

## Architecture

The fold framework already routes checkpoints (Phase 1): `run_projection` calls
`p.checkpoint(kind)` first and, on `Some(base)`, resets `*state = base`; else
`p.apply(...)`. `ActiveAgent` plugs into that contract.

```
AgentState = fold( latest Agent checkpoint , deltas after it )
```

| dimension | seed (genesis) | checkpoint | deltas (3a) | deltas (3b) |
|---|---|---|---|---|
| agent | `AgentState::from_config(...)` (M3) | `Kind::Agent(a)` → `AgentState::from(a)` | `load` ToolCall (compat) | `Toggle`, `ModelChange` |

`AgentState` is the single working-state struct. Two key conversions, called out
explicitly:

- **`AgentState::from(&pb::Agent)`** — `From` impl in `retrace.rs`; the checkpoint
  reset path. Maps the proto snapshot fields onto the struct (Vec→Set for enabled
  sets).
- **`AgentState::from_config(...)`** (the config→seed conversion) — built in
  `retrace.rs` from the same inputs `Profile::from_config_and_agent` derives
  `config_enabled` from (mcp_servers+skills where `enabled`), plus model / instructions
  / allow / deny. This is the M3 seed: the first turn — before any `Agent` event lands —
  yields the correct enabled-set. `ActiveAgent { seed }` clones it in `seed()`.

`enabled-names` (the MCP-load + `<loadable-index>` driver in `turn.rs`) reads
`ActiveAgent`'s folded `skills ∪ mcp` instead of the deleted `ActiveLoadables`. The
drift-guard reads `active_agent(...)`'s `Agent` snapshot instead of `active_manifest`.

## Tech stack

Rust, prost + pbjson (proto codegen via `build.rs`, generated into
`OUT_DIR/gait.log.v1.rs`). Tests: `cargo test` per `gait-rs/CLAUDE.md` (save to file,
grep). Proto source: `proto/log.proto`.

---

## Files

- `proto/log.proto` — add `ToggleKind`, `Toggle`, `ModelChange`; wire into `Payload`
  oneof (3b).
- `src/session/retrace.rs` — **delete** `ActiveManifest`, `ActiveLoadables`,
  `enabled_loadables`; **add** `AgentState`, `From<&pb::Agent>`, `AgentState::from_config`,
  `ActiveAgent`, `active_agent` accessor; update `RetraceResult` + `fold`.
  *(Phase 2 already renamed `active_manifest`→`active_agent` returning `Option<pb::Agent>`;
  3a repurposes that name to return the folded `AgentState` — see Task 3 note.)*
- `src/turn.rs` — drift guard (~591), enabled-names derivation (~618-625), MCP-load loop
  (~626-634), `<loadable-index>` build (~649-682). Switch reads to `ActiveAgent`.
- `src/tools/load.rs` — 3b: emit `Toggle` from the model path (via `turn.rs`'s
  `ExecutionSignal::LoadContext` arm, ~989-1043).
- `src/profile.rs` — reference only (the `config_enabled` derivation at 75-78 is the
  template for `AgentState::from_config`).
- `src/proto/events.rs` — 3b: add `toggle(...)` / `model_change(...)` body helpers;
  `agent_msg` (Phase-2 rename of `manifest_msg`) stays.

---

# Phase 3a — behaviour-identical projection collapse

## Task 1 — Add `AgentState` + the two conversions

**Goal:** the working-state struct and both seed/checkpoint conversions, with no caller
wired yet (compiles, old projections still present).

**Files:** `src/session/retrace.rs`

- [ ] **Test first.** Add to `retrace.rs` `#[cfg(test)] mod tests`:
  ```rust
  #[test]
  fn agent_state_from_pb_agent_maps_enabled_sets() {
      let a = pb::Agent {
          model: "x/y".into(),
          instructions: "sys".into(),
          tools: vec!["read".into(), "edit".into()],
          mcp_servers: vec!["slack".into()],
          skills: vec!["tdd".into()],
          permissions: Some(pb::PermissionPolicy {
              allow: vec!["read".into()], deny: vec!["shell:rm *".into()] }),
          through_seq: 0,
      };
      let st = AgentState::from(&a);
      assert_eq!(st.model, "x/y");
      assert_eq!(st.instructions, "sys");
      assert!(st.skills.contains("tdd"));
      assert!(st.mcp.contains("slack"));
      assert!(st.tools.contains("read") && st.tools.contains("edit"));
      assert!(st.allow.contains("read"));
      assert!(st.deny.contains("shell:rm *"));
  }
  ```
- [ ] **Run** (expect: does-not-compile — `AgentState` missing):
  ```bash
  cargo test 2>&1 > /tmp/gait-test-output.txt; echo "exit: $?"
  grep "^error\[" /tmp/gait-test-output.txt | head -10
  ```
  Expect `error[E0433]`/`cannot find type AgentState`.
- [ ] **Implement.** Add above `ActiveLoadables` in `retrace.rs`:
  ```rust
  /// The agent-dimension working state: the folded result of the latest `Agent`
  /// checkpoint plus the deltas after it. `skills`/`mcp`/`tools` are ENABLED sets
  /// (OR-Set semantics under the fold); `tools` is AUDIT-ONLY today (I2 — built-ins
  /// are not gated by it). `allow`/`deny` are the static policy seed (runtime grants
  /// live in ActivePermissions, I3).
  #[derive(Debug, Clone, Default)]
  pub struct AgentState {
      pub model: String,
      pub instructions: String,
      pub tools: std::collections::HashSet<String>,
      pub skills: std::collections::HashSet<String>,
      pub mcp: std::collections::HashSet<String>,
      pub allow: std::collections::HashSet<String>,
      pub deny: std::collections::HashSet<String>,
  }

  impl AgentState {
      /// Enabled loadable names = skills ∪ mcp. The single replacement for the
      /// deleted `ActiveLoadables` state (a flat name-set), so `<loadable-index>`
      /// and the MCP-load loop read IDENTICAL output.
      pub fn enabled_loadables(&self) -> std::collections::HashSet<String> {
          self.skills.union(&self.mcp).cloned().collect()
      }
  }

  /// Checkpoint reset path: the `Agent` proto snapshot → working state.
  /// (Vec→Set for the enabled fields; permissions split into allow/deny sets.)
  impl From<&pb::Agent> for AgentState {
      fn from(a: &pb::Agent) -> Self {
          let (allow, deny) = a.permissions.as_ref()
              .map(|p| (p.allow.iter().cloned().collect(),
                        p.deny.iter().cloned().collect()))
              .unwrap_or_default();
          AgentState {
              model: a.model.clone(),
              instructions: a.instructions.clone(),
              tools: a.tools.iter().cloned().collect(),
              skills: a.skills.iter().cloned().collect(),
              mcp: a.mcp_servers.iter().cloned().collect(),
              allow,
              deny,
          }
      }
  }
  ```
  Note: field names (`mcp_servers`, `through_seq`, `permissions`) are the Phase-2
  `pb::Agent` shape (renamed from `Manifest`; `output_schema_retries` already dropped
  per M4). Confirm against the generated `pb::Agent` if a field differs.
- [ ] **Run** (expect: PASS):
  ```bash
  cargo test agent_state_from_pb_agent_maps_enabled_sets 2>&1 > /tmp/gait-test-output.txt; echo "exit: $?"
  grep "test result" /tmp/gait-test-output.txt
  ```
- [ ] **Commit:**
  ```
  feat(retrace): add AgentState + From<&pb::Agent> (agent-dimension working state)

  Phase 3a step 1: the working-state struct and the checkpoint-reset
  conversion. No caller wired yet; old projections untouched.

  Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
  ```

## Task 2 — Add `AgentState::from_config` (the M3 seed)

**Goal:** the config→seed conversion, mirroring `Profile::from_config_and_agent`'s
`config_enabled` derivation (`profile.rs:75-78`).

**Files:** `src/session/retrace.rs`

- [ ] **Test first.** Add:
  ```rust
  #[test]
  fn agent_state_from_config_seeds_enabled_from_config() {
      use crate::config::{GaitConfig, McpServerDef, SkillDef};
      let cfg = GaitConfig {
          model: "anthropic/x".into(),
          mcp_servers: vec![
              McpServerDef { name: "slack".into(), spawn: None, connect: None,
                  enabled: true, description: None },
              McpServerDef { name: "off".into(), spawn: None, connect: None,
                  enabled: false, description: None },
          ],
          skills: vec![SkillDef { name: "tdd".into(), description: "d".into(),
              content: "c".into(), enabled: true }],
          ..GaitConfig::default()
      };
      let st = AgentState::from_config(&cfg, "anthropic/x".into(), "sys".into(),
          vec!["read".into()], vec!["shell:rm *".into()]);
      assert!(st.mcp.contains("slack"), "enabled mcp seeded");
      assert!(!st.mcp.contains("off"), "disabled mcp NOT seeded");
      assert!(st.skills.contains("tdd"), "enabled skill seeded");
      assert_eq!(st.model, "anthropic/x");
      assert!(st.allow.contains("read") && st.deny.contains("shell:rm *"));
  }
  ```
- [ ] **Run** (expect: no-compile, `from_config` missing). Same grep as Task 1.
- [ ] **Implement.** Add to `impl AgentState`:
  ```rust
  /// The genesis seed (M3): enabled-set derived from config exactly like
  /// `Profile::from_config_and_agent` derives `config_enabled` (profile.rs:75-78)
  /// — enabled mcp_servers + enabled skills — plus the resolved model /
  /// instructions / allow / deny. So the first turn (before any `Agent` event
  /// lands) yields the right enabled-set.
  pub fn from_config(
      cfg: &crate::config::GaitConfig,
      model: String,
      instructions: String,
      allow: Vec<String>,
      deny: Vec<String>,
  ) -> Self {
      let mcp = cfg.mcp_servers.iter().filter(|s| s.enabled)
          .map(|s| s.name.clone()).collect();
      let skills = cfg.skills.iter().filter(|s| s.enabled)
          .map(|s| s.name.clone()).collect();
      AgentState {
          model, instructions,
          tools: Default::default(),   // audit-only; populated by Agent checkpoints, not seed (I2)
          skills, mcp,
          allow: allow.into_iter().collect(),
          deny: deny.into_iter().collect(),
      }
  }
  ```
  *Decision (matches `enabled_loadables` today):* the seed's enabled-set is `mcp ∪
  skills` — identical to `Profile.config_enabled`, so `enabled_loadables()` over the
  seed equals today's `config_enabled`.
- [ ] **Run** (expect: PASS): `cargo test agent_state_from_config_seeds_enabled_from_config ...`.
- [ ] **Commit:**
  ```
  feat(retrace): add AgentState::from_config — the M3 genesis seed

  Mirrors Profile::from_config_and_agent's config_enabled derivation
  (enabled mcp_servers + skills), so the first turn before any Agent
  event yields the correct enabled-set.

  Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
  ```

## Task 3 — Add the `ActiveAgent` projection (compat ToolCall fold)

**Goal:** the projection itself — `seed` / `checkpoint` / `apply` — with the **compat
ToolCall path** so it reproduces `ActiveLoadables` exactly. Old projections still
present; not yet wired into `turn.rs`.

**Files:** `src/session/retrace.rs`

> **Phase-2 note.** Phase 2 renamed the old `active_manifest()` accessor (returning
> `Option<pb::Manifest>`) to `active_agent()` returning `Option<pb::Agent>`. Phase 3a
> **repurposes** the `active_agent` name to return the folded `AgentState`. To avoid a
> silent signature clash, this task:
> 1. renames the Phase-2 `Option<pb::Agent>` accessor to `active_agent_checkpoint()`
>    (it's only used by the drift guard, updated in Task 5), then
> 2. introduces `active_agent() -> AgentState` as the new folded accessor.

- [ ] **Test first.** Add, modelled on `loadables_projection_full_session_and_mutation_aware`
  and `active_cwd_*`:
  ```rust
  #[test]
  fn active_agent_folds_load_toolcall_like_loadables() {
      use std::collections::HashMap;
      let seed = AgentState::from_config(
          &crate::config::GaitConfig::default(),
          "m".into(), "sys".into(), vec![], vec![]);
      let evs = mint_events(vec![
          events::tool_call("1".into(), "load".into(),
              &serde_json::json!({"name":"slack","enabled":true})),
          events::tool_call("2".into(), "load".into(),
              &serde_json::json!({"name":"slack","enabled":false})),
          events::tool_call("3".into(), "load".into(),
              &serde_json::json!({"name":"tdd","enabled":true})),
      ]);
      let st = run_projection(&ActiveAgent { seed }, &evs, &HashMap::new());
      let names = st.enabled_loadables();
      assert!(!names.contains("slack"), "LWW: slack toggled off");
      assert!(names.contains("tdd"), "tdd toggled on");
  }

  #[test]
  fn active_agent_checkpoint_resets_baseline() {
      use std::collections::HashMap;
      let seed = AgentState::from_config(
          &crate::config::GaitConfig::default(), "m".into(), "sys".into(), vec![], vec![]);
      let evs = mint_events(vec![
          events::tool_call("1".into(), "load".into(),
              &serde_json::json!({"name":"old","enabled":true})),
          // an Agent checkpoint must wipe pre-checkpoint deltas (positional reset):
          wire::body(payload::Kind::Agent(pb::Agent {
              model: "m2".into(), instructions: "sys2".into(),
              tools: vec![], mcp_servers: vec!["slack".into()], skills: vec![],
              permissions: Some(pb::PermissionPolicy { allow: vec![], deny: vec![] }),
              through_seq: 0,
          })),
          events::tool_call("2".into(), "load".into(),
              &serde_json::json!({"name":"tdd","enabled":true})),
      ]);
      let st = run_projection(&ActiveAgent { seed }, &evs, &HashMap::new());
      assert_eq!(st.model, "m2", "checkpoint reset the model");
      let names = st.enabled_loadables();
      assert!(!names.contains("old"), "pre-checkpoint load wiped by Agent reset");
      assert!(names.contains("slack"), "checkpoint's mcp present");
      assert!(names.contains("tdd"), "post-checkpoint load folded");
  }
  ```
  (The reset test relies on Phase-1's `run_projection` checkpoint dispatch.)
- [ ] **Run** (expect: no-compile, `ActiveAgent` missing).
- [ ] **Implement.** Add to `retrace.rs` (place where `ActiveManifest`/`ActiveLoadables`
  are — they're deleted in Task 4):
  ```rust
  /// The agent-dimension projection (replaces ActiveManifest + ActiveLoadables).
  /// `seed` is the config-derived genesis (M3). `checkpoint` resets the baseline to
  /// an `Agent` snapshot (recompose-preserving re-baseline, I4). `apply` folds the
  /// deltas: in 3a that's the `load` ToolCall **compat path** (byte-identical to the
  /// old ActiveLoadables), switched to typed `Toggle`/`ModelChange` in 3b.
  pub(crate) struct ActiveAgent {
      pub seed: AgentState,
  }
  impl Projection for ActiveAgent {
      type State = AgentState;
      fn seed(&self) -> AgentState { self.seed.clone() }
      fn checkpoint(&self, kind: &payload::Kind) -> Option<AgentState> {
          if let payload::Kind::Agent(a) = kind { Some(AgentState::from(a)) } else { None }
      }
      fn apply(&self, st: &mut AgentState, _ev: &Event, kind: &payload::Kind) {
          // 3a compat path — fold `load` ToolCalls exactly as ActiveLoadables did.
          // Enabled names live in `skills ∪ mcp`; `load` doesn't know the kind, so a
          // toggled name lands in BOTH sets (the union is what callers read, so this
          // reproduces the flat name-set semantics). 3b replaces this with Toggle.
          if let payload::Kind::ToolCall(tc) = kind {
              if tc.name == "load" {
                  let input = struct_to_value(&tc.input);
                  let name = input.get("name").and_then(|v| v.as_str())
                      .unwrap_or("").to_string();
                  let on = input.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false);
                  if on { st.skills.insert(name.clone()); st.mcp.insert(name); }
                  else  { st.skills.remove(&name); st.mcp.remove(&name); }
              }
          }
      }
  }
  ```
  > **Compat-path caveat (called out per spec):** `ActiveLoadables` kept a single flat
  > `HashSet<String>`; `AgentState` splits skills vs mcp. To keep `enabled_loadables()`
  > **identical**, the compat `load` fold inserts/removes the name in **both** sets, and
  > every caller reads the *union* (`enabled_loadables()`). This is intentionally lossy
  > on which-kind (it always was — `load` carries no kind) and is removed in 3b when
  > `Toggle.kind` makes it precise.
- [ ] **Add the folded accessor** (and rename the Phase-2 checkpoint accessor):
  ```rust
  /// The Phase-2 `Agent`-checkpoint accessor, renamed to free `active_agent` for the
  /// folded state. Used only by the drift guard (turn.rs). LWW-Register over `Agent`.
  pub fn active_agent_checkpoint(events: &[Event]) -> Option<pb::Agent> { /* Phase-2 body */ }

  /// Folded agent-dimension state (config seed + Agent checkpoints + deltas).
  pub fn active_agent(events: &[Event], seed: AgentState) -> AgentState {
      run_projection(&ActiveAgent { seed }, events, &HashMap::new())
  }
  ```
- [ ] **Run** (expect: PASS): `cargo test active_agent_ 2>&1 > /tmp/...; grep "test result"`.
- [ ] **Commit:**
  ```
  feat(retrace): add ActiveAgent projection (3a compat ToolCall fold)

  seed from config (M3), checkpoint resets to Agent snapshot (I4),
  apply folds `load` ToolCalls in the compat path (byte-identical to
  ActiveLoadables). Rename Phase-2 active_agent → active_agent_checkpoint.

  Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
  ```

## Task 4 — Delete `ActiveManifest` + `ActiveLoadables`; update `fold`/`RetraceResult`

**Goal:** remove the two replaced projections and the `enabled_loadables` shim;
re-point `fold`/`RetraceResult` at `ActiveAgent`. `turn.rs` still references the old
names → compile fails until Task 5 (do 4+5 back-to-back; commit together if cleaner).

**Files:** `src/session/retrace.rs`

- [ ] **Delete:** `struct ActiveLoadables` + its `impl Projection` (retrace.rs:112-130);
  `struct ActiveManifest` + its `impl` (132-144); `pub fn enabled_loadables` (503-512);
  and the now-dead tests `active_manifest_returns_last_manifest`,
  `active_manifest_returns_none_for_no_manifest`,
  `active_manifest_projection_is_last_write_wins`,
  `enabled_loadables_seeds_from_config_and_replays_toggles`,
  `loadables_projection_full_session_and_mutation_aware` (port the
  mutation-aware assertion into a new `active_agent_*` test — see below).
- [ ] **Port the mutation-aware test** (the redact-drops-load guarantee must survive):
  ```rust
  #[test]
  fn active_agent_honors_mutations_redacted_load_dropped() {
      use std::collections::HashMap;
      let seed = AgentState::from_config(
          &crate::config::GaitConfig::default(), "m".into(), "sys".into(), vec![], vec![]);
      let one = mint_events(vec![
          events::tool_call("1".into(), "load".into(),
              &serde_json::json!({"name":"slack","enabled":true})),
      ]);
      let mut muts = HashMap::new();
      muts.insert(one[0].id.clone(), MutationState::Redacted { reason: None });
      let st = run_projection(&ActiveAgent { seed }, &one, &muts);
      assert!(!st.enabled_loadables().contains("slack"), "redacted load disabled");
  }
  ```
- [ ] **Update `RetraceResult`** (retrace.rs:514-528): replace
  ```rust
  pub enabled_loadables: HashSet<String>,
  pub active_manifest: Option<pb::Manifest>,
  ```
  with
  ```rust
  /// Folded agent-dimension state (replaces enabled_loadables + active_manifest).
  pub agent: AgentState,
  ```
- [ ] **Update `fold`** (retrace.rs:537-556): it gains the seed. Change signature to
  thread an `AgentState` seed (the caller — `turn.rs` — already has the config):
  ```rust
  pub fn fold(
      all_events: &[Event],
      agent_seed: AgentState,
  ) -> anyhow::Result<RetraceResult> {
      let mut mutations = active_mutations(all_events);
      apply_cascade(&mut mutations, all_events);
      let agent = run_projection(&ActiveAgent { seed: agent_seed }, all_events, &mutations);
      let allow_state =
          run_projection(&ActivePermissions { seed: Default::default() }, all_events, &mutations);
      let effective_allow = allow_state.session.union(&allow_state.once).cloned().collect();
      let messages = run_projection(&ActiveSummary, all_events, &mutations).msgs;
      Ok(RetraceResult { messages, agent, effective_allow, mutations })
  }
  ```
  (`config_enabled: &HashSet` param is gone — it's subsumed by `agent_seed`.) Update the
  `fold` doc-comment to drop the `ActiveManifest`/`ActiveLoadables` names. Update `fold`
  unit tests (`fold_returns_all_three_projections`, `fold_runs_all_projections_single_pass`,
  `fold_*`) to construct a seed via `AgentState::from_config(&GaitConfig::default(), ...)`
  and assert on `result.agent.enabled_loadables()` / `result.agent.model`.
- [ ] **Update module doc** (retrace.rs:1-11): replace the `enabled_loadables` /
  `active_manifest` bullets with one `active_agent` bullet.
- [ ] (Compile deferred — `turn.rs` still references old names. Proceed to Task 5.)
- [ ] **Commit** (with Task 5, or standalone if it compiles via a temporary shim):
  ```
  refactor(retrace): delete ActiveManifest + ActiveLoadables; fold via ActiveAgent

  RetraceResult.{enabled_loadables,active_manifest} → .agent: AgentState.
  fold() now takes an AgentState seed instead of config_enabled.
  Mutation-aware redact-drops-load coverage ported to active_agent test.

  Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
  ```

## Task 5 — Re-point `turn.rs` at `ActiveAgent` (enabled-names + drift guard)

**Goal:** every `turn.rs` read of the deleted projections now reads `ActiveAgent`.
`<loadable-index>` output stays IDENTICAL. Build + full test pass.

**Files:** `src/turn.rs`

- [ ] **Build the seed once** near the top of `run_turn` (after `config_enabled` is
  pulled from `caps`, ~418). The seed needs the config; `ResolvedProfile` carries
  `config_enabled: HashSet` today but not the full `GaitConfig`. Two options — pick the
  minimal one:
  - **(a) Preferred, no plumbing:** build the seed from the resolved fields already in
    scope, reusing `config_enabled` as the enabled-set directly:
    ```rust
    let agent_seed = retrace::AgentState {
        model: model.clone(),
        instructions: system_prompt.clone(),   // stable layers (C3)
        tools: Default::default(),              // audit-only (I2)
        skills: skill_registry.iter().map(|s| s.name.clone()).collect(),
        mcp: Default::default(),                // mcp enable comes via config_enabled below
        allow: allow.iter().cloned().collect(),
        deny: deny.iter().cloned().collect(),
    };
    // config_enabled already = enabled mcp_servers ∪ enabled skills (profile.rs:75-78);
    // fold it straight into the enabled union the same way ActiveLoadables seeded it:
    let agent_seed = { let mut s = agent_seed;
        for n in &config_enabled { s.skills.insert(n.clone()); s.mcp.insert(n.clone()); }
        s };
    ```
    This reproduces the *exact* old seed: `ActiveLoadables { seed: config_enabled }`. The
    union `enabled_loadables()` equals `config_enabled` ∪ skill-names — identical to
    today (skills were already in `config_enabled` per profile.rs:77).
  - **(b) Cleaner long-term:** thread `GaitConfig` into `ResolvedProfile` and call
    `AgentState::from_config`. **Defer to Phase 4** — 3a must be behaviour-identical,
    and (a) achieves that with zero plumbing. Note this in the commit body.
- [ ] **Drift guard** (turn.rs:589-595): replace
  ```rust
  if retrace::active_manifest(&events_for_guard) != Some(manifest.clone()) {
      emitter.emit(wire::body(payload::Kind::Manifest(manifest)))?;
  }
  ```
  with the Phase-2-renamed `agent_msg` + `active_agent_checkpoint`:
  ```rust
  if retrace::active_agent_checkpoint(&events_for_guard) != Some(agent.clone()) {
      emitter.emit(wire::body(payload::Kind::Agent(agent)))?;
  }
  ```
  (and the binding at ~404 becomes `let agent = crate::proto::events::agent_msg(&caps);`
  — Phase-2 rename. If Phase 2 already did this rename, only the guard body changes.)
- [ ] **Enabled-names** (turn.rs:618-625): replace the `ActiveLoadables` run with
  `ActiveAgent`, reading the union:
  ```rust
  let all_events = ingestor.events()?;
  let mut muts = retrace::active_mutations(&all_events);
  retrace::apply_cascade(&mut muts, &all_events);
  let agent_state = retrace::run_projection(
      &retrace::ActiveAgent { seed: agent_seed.clone() }, &all_events, &muts);
  let enabled_names = agent_state.enabled_loadables();
  ```
  The downstream MCP-load loop (626-634), the `<loadable-index>` entries
  (`enabled_names.contains(...)` at 670, 679) and `build_loadable_index` are **unchanged**
  — `enabled_names` is still a `HashSet<String>` with identical contents.
- [ ] **Build** (expect: clean):
  ```bash
  cargo build 2>&1 > /tmp/gait-build-output.txt; echo "exit: $?"
  grep "^error" /tmp/gait-build-output.txt | head -20
  ```
- [ ] **Full test** (expect: all pass — this is the behaviour-identical gate):
  ```bash
  cargo test 2>&1 > /tmp/gait-test-output.txt; echo "exit: $?"
  grep "FAILED\|test result" /tmp/gait-test-output.txt
  ```
- [ ] **Commit:**
  ```
  refactor(turn): read agent state via ActiveAgent (drift guard + enabled-names)

  <loadable-index> and enabled-names output byte-identical: enabled_names is
  still skills ∪ mcp from the same load-ToolCall fold. Seed built from the
  resolved fields + config_enabled (matches old ActiveLoadables seed); threading
  full GaitConfig into ResolvedProfile is deferred to Phase 4.

  Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
  ```

## Task 6 — 3a behaviour-parity verification (dogfood snapshot)

**Goal:** prove 3a is behaviour-identical end-to-end before introducing deltas.

**Files:** none (verification only)

- [ ] **Run the full suite once more, captured:**
  ```bash
  cargo test 2>&1 > /tmp/gait-test-3a.txt; echo "exit: $?"
  grep "test result" /tmp/gait-test-3a.txt
  ```
  Expect zero failures; note the total test count.
- [ ] **Manual `<loadable-index>` diff (optional, recommended):** dogfood a 2-turn
  session that calls `load("tdd", true)` and confirm the `<loadable-index>` block and
  enabled flags match a pre-3a run. (Per MEMORY: `--gait-dir /Users/aicoder/gait-test-ws
  --model minimax/MiniMax-M3`.)
- [ ] No commit (verification gate). Proceed to 3b.

---

# Phase 3b — introduce typed deltas

## Task 7 — Add `Toggle` / `ModelChange` proto messages

**Goal:** the new delta messages on the wire + regenerated Rust types.

**Files:** `proto/log.proto`

- [ ] **Add the messages** near `Cwd` (log.proto, after line 99). Free oneof field nums
  ≥ 53 (52 is `cwd`; 50–51 taken; 20–49 reserved):
  ```proto
  // capability enable/disable delta — OR-Set per kind; folded by ActiveAgent.
  enum ToggleKind { TOGGLE_KIND_UNSPECIFIED = 0; SKILL = 1; MCP = 2; TOOL = 3; }
  message Toggle {
    ToggleKind kind = 1;
    string     name = 2;
    bool       enabled = 3;
  }
  // model-swap delta — LWW-Register; folded by ActiveAgent.
  message ModelChange { string model = 1; }
  ```
  **No `PermissionGrant`** (I3) — runtime grants stay on `PermissionsReply`.
- [ ] **Wire into the `Payload` oneof** (log.proto:90-93, after `cwd = 52`):
  ```proto
    Cwd         cwd          = 52;
    Toggle      toggle       = 53;
    ModelChange model_change = 54;
  ```
- [ ] **Regenerate + build** (build.rs runs prost/pbjson on `cargo build`):
  ```bash
  cargo build 2>&1 > /tmp/gait-build-output.txt; echo "exit: $?"
  grep "^error" /tmp/gait-build-output.txt | head -20
  ```
  Confirm `pb::Toggle`, `pb::ToggleKind`, `pb::ModelChange`, `payload::Kind::Toggle`,
  `payload::Kind::ModelChange` exist:
  ```bash
  grep -rn "pub struct Toggle\|pub enum ToggleKind\|pub struct ModelChange" $(find target -name 'gait.log.v1.rs' | head -1)
  ```
- [ ] **Commit:**
  ```
  feat(proto): add Toggle{kind,name,enabled} + ModelChange deltas

  OR-Set-per-kind capability toggle + LWW model swap, oneof nums 53/54.
  No PermissionGrant (I3) — runtime grants stay on PermissionsReply.

  Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
  ```

## Task 8 — `ActiveAgent::apply` folds `Toggle` + `ModelChange`; drop compat path

**Goal:** typed deltas folded (precise per-kind sets), ToolCall compat path removed.

**Files:** `src/session/retrace.rs`

- [ ] **Test first.** Add:
  ```rust
  #[test]
  fn active_agent_folds_toggle_per_kind_and_model_change() {
      use std::collections::HashMap;
      use crate::proto::gait::log::v1::ToggleKind;
      let seed = AgentState::from_config(
          &crate::config::GaitConfig::default(), "m".into(), "sys".into(), vec![], vec![]);
      let evs = mint_events(vec![
          wire::body(payload::Kind::Toggle(pb::Toggle {
              kind: ToggleKind::Skill as i32, name: "tdd".into(), enabled: true })),
          wire::body(payload::Kind::Toggle(pb::Toggle {
              kind: ToggleKind::Mcp as i32, name: "slack".into(), enabled: true })),
          wire::body(payload::Kind::Toggle(pb::Toggle {
              kind: ToggleKind::Skill as i32, name: "tdd".into(), enabled: false })),
          wire::body(payload::Kind::ModelChange(pb::ModelChange { model: "m2".into() })),
      ]);
      let st = run_projection(&ActiveAgent { seed }, &evs, &HashMap::new());
      assert!(!st.skills.contains("tdd"), "skill toggled off (OR-Set remove)");
      assert!(st.mcp.contains("slack"), "mcp toggled on, in mcp set ONLY");
      assert!(!st.skills.contains("slack"), "mcp toggle did NOT leak into skills");
      assert_eq!(st.model, "m2", "ModelChange LWW");
  }
  ```
  This test asserts the **precision** the compat path lacked (kind-correct sets).
- [ ] **Run** (expect: no-compile — `Kind::Toggle` arm missing from apply / fails).
- [ ] **Implement.** Replace the compat `ToolCall` block in `ActiveAgent::apply` with:
  ```rust
  fn apply(&self, st: &mut AgentState, _ev: &Event, kind: &payload::Kind) {
      use crate::proto::gait::log::v1::ToggleKind;
      match kind {
          payload::Kind::Toggle(t) => {
              let set = match ToggleKind::try_from(t.kind).unwrap_or(ToggleKind::Unspecified) {
                  ToggleKind::Skill => &mut st.skills,
                  ToggleKind::Mcp   => &mut st.mcp,
                  ToggleKind::Tool  => &mut st.tools,   // audit-only (I2)
                  ToggleKind::Unspecified => return,
              };
              if t.enabled { set.insert(t.name.clone()); }
              else         { set.remove(&t.name); }
          }
          payload::Kind::ModelChange(m) => st.model = m.model.clone(),
          // permissions: NOT here — ActivePermissions owns runtime grants (I3)
          _ => {}
      }
  }
  ```
- [ ] **Run** (expect: PASS): `cargo test active_agent_folds_toggle ...`.
- [ ] **Commit:**
  ```
  feat(retrace): ActiveAgent folds Toggle (OR-Set/kind) + ModelChange (LWW)

  Drops the 3a `load` ToolCall compat path; per-kind sets are now precise.
  Tool toggles land in st.tools (audit-only, I2). No permission delta (I3).

  Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
  ```

## Task 9 — Emit `Toggle` from `load()` (the model path)

**Goal:** the `load` tool's enable/disable now emits a typed `Toggle` delta. Since
Task 8 removed the ToolCall fold, this is what keeps enabled-state working.

**Files:** `src/proto/events.rs`, `src/turn.rs`

- [ ] **Add body helpers** to `events.rs` (alongside `cwd`/others):
  ```rust
  /// A `Toggle` delta body (capability enable/disable).
  pub fn toggle(kind: pb::ToggleKind, name: String, enabled: bool) -> pb::event::Body {
      wire::body(pb::payload::Kind::Toggle(pb::Toggle {
          kind: kind as i32, name, enabled }))
  }
  /// A `ModelChange` delta body (LWW model swap).
  pub fn model_change(model: String) -> pb::event::Body {
      wire::body(pb::payload::Kind::ModelChange(pb::ModelChange { model }))
  }
  ```
- [ ] **Emit from the `LoadContext` arm** in `turn.rs` (the
  `ExecutionSignal::LoadContext` branch, ~989-1015). After resolving `item_name`/`on`,
  emit a `Toggle` whose `kind` is determined by registry lookup (mcp registry → `Mcp`,
  else → `Skill`):
  ```rust
  if let (Some(item_name), Some(on)) = (
      tc.args.get("name").and_then(|v| v.as_str()),
      tc.args.get("enabled").and_then(|v| v.as_bool()),
  ) {
      let kind = if mcp_registry.iter().any(|d| d.server_name == item_name) {
          pb::ToggleKind::Mcp
      } else {
          pb::ToggleKind::Skill
      };
      emitter.emit(events::toggle(kind, item_name.to_string(), on))?;
      // ... existing tools.push / tools.retain MCP-load body unchanged ...
  }
  ```
  Keep the existing `execute_tool_call(tc, ...)` (it still emits the `ToolCall`/`ToolResult`
  pair for the model's audit trail — that's fine; the fold now ignores the `load`
  ToolCall and reads the `Toggle`).
- [ ] **Add a turn-level regression test** asserting a `load` call produces a `Toggle`
  event and the folded `enabled_loadables()` reflects it (model-facing path). Place in
  `turn.rs` tests using the `MockProvider` that returns one `load` ToolCallComplete then
  `output()`. Assert via `read_events` that a `payload::Kind::Toggle` with the right
  `name`/`enabled` is present.
- [ ] **Build + full test:**
  ```bash
  cargo test 2>&1 > /tmp/gait-test-3b.txt; echo "exit: $?"
  grep "FAILED\|test result" /tmp/gait-test-3b.txt
  ```
  Expect all pass. (The 3a `<loadable-index>` tests still pass because enabled-state now
  flows through `Toggle` → `ActiveAgent` → `enabled_names`, contents identical.)
- [ ] **Commit:**
  ```
  feat(load): emit Toggle delta on enable/disable; fold drops ToolCall path

  load()'s LoadContext arm emits Toggle{kind,name,enabled} (kind resolved
  via mcp_registry lookup). ToolCall/ToolResult pair still emitted for the
  model's audit trail; the fold now reads the typed Toggle.

  Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
  ```

## Task 10 — Recompose-preserving re-baseline (I4)

**Goal:** editing `AGENT.md` mid-session emits a fresh `Agent` snapshot that PRESERVES
runtime `Toggle`s — `snapshot = active_agent(events)` with new `instructions` patched
in, NOT a wipe.

**Files:** `src/session/retrace.rs` (helper), call-site TBD (see note)

- [ ] **Test first.** The core guarantee — re-baseline keeps prior toggles:
  ```rust
  #[test]
  fn rebaseline_preserves_runtime_toggles() {
      use std::collections::HashMap;
      use crate::proto::gait::log::v1::ToggleKind;
      let seed = AgentState::from_config(
          &crate::config::GaitConfig::default(), "m".into(), "old sys".into(), vec![], vec![]);
      let evs = mint_events(vec![
          wire::body(payload::Kind::Toggle(pb::Toggle {
              kind: ToggleKind::Skill as i32, name: "tdd".into(), enabled: true })),
      ]);
      // recompose: fold current state, then patch instructions:
      let snapshot = rebaseline_agent(&ActiveAgent { seed }, &evs, &HashMap::new(),
          "new sys".to_string());
      assert_eq!(snapshot.instructions, "new sys", "new AGENT.md body patched in");
      assert!(snapshot.skills.contains("tdd"),
          "runtime Toggle survives the re-baseline (NOT wiped, I4)");
  }
  ```
- [ ] **Implement** the helper in `retrace.rs`:
  ```rust
  /// I4 — recompose-preserving re-baseline. Folds the CURRENT agent state, then
  /// patches in the freshly-composed `instructions` (the edited AGENT.md body).
  /// Runtime `Toggle`s survive (unlike a Summary, which is lossy by design). The
  /// caller emits the returned AgentState as a fresh `Agent` checkpoint.
  pub fn rebaseline_agent(
      proj: &ActiveAgent,
      events: &[Event],
      muts: &HashMap<String, MutationState>,
      new_instructions: String,
  ) -> AgentState {
      let mut st = run_projection(proj, events, muts);
      st.instructions = new_instructions;
      st
  }
  ```
  And a converter `AgentState → pb::Agent` for emission (inverse of `From<&pb::Agent>`):
  ```rust
  impl From<&AgentState> for pb::Agent {
      fn from(s: &AgentState) -> Self {
          let mut tools: Vec<_> = s.tools.iter().cloned().collect();    tools.sort();
          let mut mcp: Vec<_> = s.mcp.iter().cloned().collect();        mcp.sort();
          let mut skills: Vec<_> = s.skills.iter().cloned().collect();  skills.sort();
          let mut allow: Vec<_> = s.allow.iter().cloned().collect();    allow.sort();
          let mut deny: Vec<_> = s.deny.iter().cloned().collect();      deny.sort();
          pb::Agent {
              model: s.model.clone(),
              instructions: s.instructions.clone(),
              tools, mcp_servers: mcp, skills,
              permissions: Some(pb::PermissionPolicy { allow, deny }),
              through_seq: 0,
          }
      }
  }
  ```
  (Sets are sorted on the way out so the emitted checkpoint is stable — same rationale as
  `agent_msg` sorting `mcp_servers`, events.rs:252.)
  > **Round-trip note:** `From<&pb::Agent>` then `From<&AgentState>` is lossy on
  > skills-vs-mcp ONLY for names a 3a-era compat `load` put in both sets; in 3b every
  > toggle is kind-precise, so the round-trip is clean. The re-baseline test above
  > guards the live (3b) path.
- [ ] **Wire the call site.** The trigger is "AGENT.md edited mid-session." **Note:**
  the *detection* mechanism (file-mtime watch / explicit `gait` command) is **Phase 4's**
  markdown-context-resolver work — this task lands the *mechanism* (`rebaseline_agent` +
  `From<&AgentState> for pb::Agent`) and its test. If a natural hook exists today (e.g.
  the drift guard re-composing `instructions` each turn), route it through
  `rebaseline_agent` so a changed system prompt emits a recompose rather than a raw
  `agent_msg`. Otherwise leave the helper unwired with a `// TODO(phase4): call from
  AGENT.md-edit detector` and cover it by the unit test. **Do not** wipe-and-reseed.
- [ ] **Run** (expect: PASS): `cargo test rebaseline ...`.
- [ ] **Commit:**
  ```
  feat(retrace): recompose-preserving Agent re-baseline (I4)

  rebaseline_agent folds current state then patches new instructions, so
  runtime Toggles survive an AGENT.md edit. Adds From<&AgentState> for
  pb::Agent (sorted sets, stable emission). Call-site detection deferred
  to Phase 4; helper covered by unit test.

  Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
  ```

## Task 11 — 3b verification

**Files:** none (verification)

- [ ] **Full suite, captured:**
  ```bash
  cargo test 2>&1 > /tmp/gait-test-3b-final.txt; echo "exit: $?"
  grep "FAILED\|test result" /tmp/gait-test-3b-final.txt
  ```
  Expect zero failures.
- [ ] **Dogfood:** a session that `load`s a skill, confirm via `gait log` /
  `read_events` that a `Toggle` event (not just the `load` ToolCall) is on disk and the
  `<loadable-index>` shows `enabled=true`.
- [ ] **Hard-constraint audit (read-only grep):**
  ```bash
  grep -rn "PermissionGrant" src/ proto/        # expect: ZERO (I3)
  grep -rn "ActiveManifest\|ActiveLoadables\|enabled_loadables" src/  # expect: ZERO (deleted)
  grep -rn "ActivePermissions" src/session/retrace.rs  # expect: present, UNCHANGED (I3)
  ```

---

## Constraint recap (verify at the end)

- **M3** — `ActiveAgent::seed` = `AgentState::from_config(...)` (Task 2); wired in
  `turn.rs` from config-derived fields (Task 5).
- **I2** — `Agent.tools` / `AgentState.tools` audit-only; `Toggle{kind:TOOL}` folds into
  `st.tools` but nothing gates built-ins (no `turn.rs` tool-list filter). Deferred to
  Phase 8.
- **I3** — no `PermissionGrant` proto; `ActivePermissions` untouched; `Agent.permissions`
  is the static seed only (`From<&pb::Agent>` reads it into `allow`/`deny`, never folded
  by `Toggle`).
- **I4** — `rebaseline_agent` recompose-preserves (Task 10), never wipes.
- **`<loadable-index>` identical through 3a** — `enabled_names` is still `skills ∪ mcp`
  fed to the unchanged `build_loadable_index` (Task 5); 3a compat fold reproduces the
  old flat name-set; verified in Task 6.

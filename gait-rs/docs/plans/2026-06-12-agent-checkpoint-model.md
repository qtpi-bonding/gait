# Agent-as-Checkpoint — the delta/checkpoint projection model

> **Status: DESIGN SPEC (for review).** This is the foundational architecture. The
> skills loader, `/skillname` trigger, agent capability-bundles, the unified
> markdown-context resolver, and the `gait init` rework are all **consequences** of
> it and are sketched at the end as downstream plans. Decisions marked **[locked]**
> were settled in the 2026-06-12 design session; **[open]** ones need your call.

**Goal:** Collapse the three overlapping representations of "agent/config state"
(`Profile`, `ActiveManifest`, `ActiveLoadables`) into **one projection** folded from
the same CRDT delta substrate as messages and codebase — by recognizing that the
log has only two event *roles*: **deltas** and **checkpoints**.

---

## 0. Review fixes (Opus review, 2026-06-12)

A principal-level review found the model **sound-with-fixes**. Resolutions, applied below:

- **C1 — `through_seq` is dead today; the checkpoint reset is *positional*.** `ActiveSummary`
  resets when it *encounters* a `Compaction`/`Summary` payload in source order; it never
  reads `through_seq` (written-once-as-`None`, read nowhere). **Decision:** keep the reset
  **positional** (matches today); `through_seq` stays audit-only metadata. The "fold
  coordinate" language is removed. CRDT caveat: `effective_kinds` iterates **file order**,
  not seq-sorted — convergence holds today only because single-writer append makes
  file-order == seq-order; multi-writer merge would need a seq-sort pass (§7, deferred).
- **C2 — message checkpoint is a *fold-from-payload*, not a static snapshot.** `ActiveSummary::checkpoint`
  must return a baseline containing the synthesized summary message (`"[Session history
  summary: {summary}]"`), not empty — see §3. Add a **two-compaction** regression test.
- **C3 — `Agent.instructions` = stable layers only.** The live prompt appends
  `<loadable-index>` + `<permission-status>` + `pre_turn` hooks *after* the manifest, from
  projections — so a frozen full prompt was never replay-complete. `instructions` holds the
  **stable cacheable layers** (prompt.rs: harness/agent/project/user); the dynamic suffix is
  a **derived view** of `ActiveAgent`/`ActivePermissions`, never stored.
- **I1 — the proto rename is NOT additive on the wire.** JSON is camelCase by field *name*;
  old `sessions/*/log.jsonl` carry `"manifest"`/`"compaction"` keys. Phase 2 MUST add serde
  aliases (`#[serde(alias="manifest")]` on `agent`, `alias="compaction"` on `summary`) and a
  pre-rename `log.jsonl` fixture test, or old logs silently lose those events.
- **I2 — tools enable-set has no runtime consumer yet.** Built-ins/config-tools are built
  unconditionally in `Profile::resolve`; nothing filters them by an enabled-set. **Decision:**
  `Agent.tools` stays **audit-only** (the resolved list, as today) for now; the *tool-gating
  consumer* (turn.rs filters tools by `ActiveAgent.tools`) is its **own later phase**, not
  part of the projection collapse. O1/O3 intent (tools toggleable) is preserved as the target;
  only the runtime gate is deferred.
- **I3 — drop the new `PermissionGrant` delta.** Runtime grants already flow through
  `PermissionsReply` (which `ActivePermissions` folds with session/once **expiry + revoke** —
  semantics a grow-only set can't express). **Decision:** `Agent.permissions` = the static
  policy **seed** (config+agent floor); `ActivePermissions` keeps owning runtime grants. No new
  permission delta. (User intent "permissions are deltas" is honored — via the *existing*
  delta path.)
- **I4 — re-baseline is recompose-preserving, not wipe.** Editing `AGENT.md` mid-session emits
  a fresh `Agent` snapshot = `snapshot(active_agent(events))` with the new `instructions`
  patched in — so runtime `Toggle`s are **not** lost (unlike a `Compaction`, which is lossy by
  design). This is the one place the two checkpoint dimensions legitimately differ.
- **M3 — `ActiveAgent::seed` comes from config**, like today's `ActiveLoadables{seed}`, so the
  first turn (before any `Agent` event lands) still yields the right enabled-set.
- **M4 — `output_schema_retries` leaves the envelope** → read from gait.jsonc at resolve time
  (it's runtime, not enabled-state).

---

## 1. The core model

The log vocabulary bifurcates into two roles:

- **Deltas** — the incremental worldline. `UserTurn`, `AssistantText`,
  `ToolCall`/`ToolResult`, `Cwd`, `PermissionsReply` (existing), and the new `Toggle` /
  `ModelChange`. Each is a small, commutative mutation.
- **Checkpoints** — a baseline state for *one dimension*, applied **positionally** where the
  checkpoint event sits in the stream (C1 — `through_seq` is audit-only, not a fold coordinate
  today). `Summary` (the **message** dimension) and `Agent` (the **agent** dimension, renamed
  from `Manifest`). The shadow-git checkpoint commit is the **codebase** dimension's equivalent.

`Summary` and `Agent` are **the same kind of thing** — state
checkpoints — differing only in which projection they baseline. Every projection then
has exactly one shape:

```
state = fold( latest checkpoint for my dimension , deltas after it )
```

| dimension | deltas | checkpoint |
|---|---|---|
| messages | `UserTurn`, `AssistantText`, `ToolCall`/`Result` | `Summary` |
| agent | `Toggle`, `ModelChange` (+ existing `PermissionsReply`) | `Agent` |
| codebase/env | `Cwd`, dirty-stamp | shadow-git checkpoint commit |

**Checkpointing generalizes** from a message-only operation to: *emit a checkpoint for
dimension X so older deltas can be GC'd.* You can checkpoint the agent dimension the same
way you compact messages — collapse a long run of `Toggle`s into a fresh `Agent`
snapshot. One operation, every dimension.

### Why this is CRDT-preserving (and better) **[locked]**

Deltas use commutative merge rules, so concurrent/divergent logs still converge by the
existing `(seq, id)` ordering — and *better* than today's whole-snapshot `Manifest`
(which clobbers concurrent edits):

- `model` → LWW-register (last `(seq,id)` wins)
- `skills` / `mcp` enabled-set → OR-Set (add/remove with `(seq,id)` tiebreak)
- `permissions` granted-set → G-Set (grow-only)
- `instructions` / agent-swap → LWW-register

CRDT is kept as **near-free optionality** (no concurrent-writer use today, but commutative
deltas cost nothing to keep). The *separate* question of simplifying the unused envelope
Lamport `seq`/`parents` DAG is **out of scope** here — see §7.

---

## 2. Proto changes **[locked: distinct types; rename; envelope]**

Keep checkpoints as **distinct messages** (a single `oneof` `Checkpoint` would just
relocate per-dimension handling — the payloads genuinely differ: prose summary vs typed
config). Unify *behavior*, not *bytes*.

1. **Rename `Manifest` → `Agent`** and **`Compaction` → `Summary`**
   (O6). `Agent` is the **envelope** holding a full snapshot of every agent
   sub-state — which today's `Manifest` already half-is. **No hooks** (O5):

   ```proto
   // The agent-dimension checkpoint: a full snapshot of the composed agent state
   // as of `through_seq`. Each delta event mutates exactly one sub-state below.
   message Agent {
     string model                 = 1;   // LWW
     string instructions          = 2;   // STABLE prompt layers only (C3); dynamic suffix is derived
     repeated string tools        = 3;   // resolved tool list — AUDIT-ONLY for now (I2)
     repeated string mcp_servers  = 4;   // ENABLED mcp
     repeated string skills       = 5;   // ENABLED skills
     PermissionPolicy permissions = 6;   // static policy SEED; runtime grants stay in ActivePermissions (I3)
     uint64 through_seq           = 7;   // audit metadata only — reset is positional, NOT seq-driven (C1)
     // output_schema_retries removed (M4) — read from gait.jsonc at resolve time
   }
   ```
   Field numbers stay in the existing `Manifest` slot (oneof arm `2`), renamed
   `Agent agent = 2;`. `Compaction` (arm `17`) → `Summary`.

2. **Add `through_seq` to `Agent`** so the fold knows its baseline coordinate,
   mirroring `Summary.through_seq`.

3. **New delta event messages** (O1 — toggleable capabilities; free oneof field nums ≥ 53):
   ```proto
   enum ToggleKind { TOGGLE_KIND_UNSPECIFIED = 0; SKILL = 1; MCP = 2; TOOL = 3; }
   message Toggle      { ToggleKind kind = 1; string name = 2; bool enabled = 3; } // OR-Set per kind
   message ModelChange { string model = 1; }                                       // LWW op
   ```
   **No `PermissionGrant`** (I3): runtime permission grants already flow through the
   existing `PermissionsReply` event (folded by `ActivePermissions` with session/once
   expiry+revoke). `TOOL` toggles are emitted but the runtime *consumer* (filtering the
   tool list) is a later phase (I2) — until then `Toggle{kind:TOOL}` is recorded but
   built-ins aren't yet gated.

---

## 3. Projection trait split **[locked]**

Generalize the fold so checkpoints are a framework concern, not an `ActiveSummary`
special-case.

```rust
pub(crate) trait Projection {
    type State;
    /// Genesis baseline (before any events).
    fn seed(&self) -> Self::State;
    /// If `kind` is a checkpoint for THIS projection's dimension, return the
    /// baseline state it establishes (the fold resets to it). Default: None.
    fn checkpoint(&self, _kind: &payload::Kind) -> Option<Self::State> { None }
    /// Fold one delta into the state.
    fn apply(&self, state: &mut Self::State, ev: &Event, kind: &payload::Kind);
    fn finish(&self, state: Self::State) -> Self::State { state }
}

// run_projection inner loop:
//   for (ev, kind) in effective_kinds(...) {
//       match p.checkpoint(kind) {
//           Some(base) => *state = base,     // checkpoint resets baseline
//           None       => p.apply(state, ev, kind),
//       }
//   }
```

- `ActiveSummary::checkpoint` returns a baseline that **contains the synthesized summary
  message** (C2) — a *fold-from-payload*, not an empty snapshot:
  ```rust
  fn checkpoint(&self, kind) -> Option<SummaryState> {
      if let Kind::Summary(c) = kind {
          Some(SummaryState::with_summary(format!("[Session history summary: {}]", c.summary)))
      } else { None }
  }
  ```
  This exactly reproduces today's `clear()`-then-push-summary reset (positional, C1).
  **Regression test: two `Summary` events in one log** (the second wipes the first's summary).
- `ActiveAgent::checkpoint` returns the snapshot on `Agent` (the recompose-preserving
  re-baseline, I4).
- `ActiveManifest` and `ActiveLoadables` are **deleted**; `ActiveAgent` replaces both. Its
  `seed` is built from config (M3) so the first turn — before any `Agent` event lands — still
  yields the correct enabled-set.

### `ActiveAgent`

```rust
pub(crate) struct ActiveAgent { pub seed: AgentState }   // seed = genesis/opening
impl Projection for ActiveAgent {
    type State = AgentState;   // { model, instructions, tools, skills:Set, mcp:Set, allow:Set, deny:Set, ... }
    fn seed(&self) -> AgentState { self.seed.clone() }
    fn checkpoint(&self, kind) -> Option<AgentState> {
        if let Kind::Agent(a) = kind { Some(AgentState::from(a)) } else { None }
    }
    fn apply(&self, st, _ev, kind) {
        match kind {
            Kind::Toggle(t) => {
                let set = match t.kind { Skill => &mut st.skills, Mcp => &mut st.mcp, Tool => &mut st.tools };
                if t.enabled { set.insert(t.name) } else { set.remove(&t.name) }
            }
            Kind::ModelChange(m)     => st.model = m.model.clone(),
            _ => {}  // permissions: NOT here — ActivePermissions owns runtime grants (I3)
        }
    }
}
pub fn active_agent(events: &[Event]) -> AgentState { run_projection(&ActiveAgent{..}, events, &HashMap::new()) }
```

---

## 4. Assembly: the seed is an event, not a file re-read **[locked]**

The `.md`/`.jsonc` are the agent's **source**, not the agent. Assembly happens **once**,
written as the opening `Agent` checkpoint:

```
opening Agent = compose( builtin , gait.jsonc , agents/<name>/AGENT.md )
```

where `compose` writes the **stable prompt layers** into `instructions` (C3 — the dynamic
`<loadable-index>`/`<permission-status>`/hook suffix is a derived view of the projections,
never stored). The fold then **never re-reads** those files (they can change under a running
session); replay stays pure.

Mid-session source changes (you edit `AGENT.md`) emit a **fresh `Agent` checkpoint**, but as a
**recompose, not a wipe** (I4): the new snapshot = `active_agent(events)` (current folded
state) with the new `instructions` patched in — so runtime `Toggle`s survive. (Unlike a
`Summary`, which is lossy *by design*; this is the one place the two checkpoint dimensions
legitimately differ.)

**Agent = capability bundle [locked].** `gait.jsonc` *installs* (registry/infra) +
policy/fallback; an `AGENT.md` *enables* a subset + identity (model, role, allow); runtime
`load()` / `/skill` are ad-hoc `Toggle` deltas. `agent_state = compose(builtin,
gait.jsonc-installed, AGENT.md-enabled, runtime-deltas)`. (See O2 for the installed-vs-
enabled split.)

Merge rules (extending today's `Profile::from_config_and_agent`):
- `model`: override › agent › config (precedence) — unchanged
- `allow`: agent-if-nonempty replaces config (innermost-wins) — unchanged
- `deny`: config ∪ agent (monotonic union) — unchanged
- `skills`/`mcp` enabled-at-start: `[open]` union vs replace — see §6
- `tools` available: `[open]` — see §6

---

## 5. Downstream consequences (each becomes its own plan)

These all reduce to "emit/seed agent-dimension state," so they fall out of §1–4:

- **Unified markdown-context resolver** — `resolve_markdown_context_path(gait_dir, kind, name)`:
  `Agent → agents/<name>/AGENT.md`, `Project → codebases/<key>/PROJECT.md`,
  `Skill → skills/<name>/SKILL.md`, `User → USER.md` (singleton). Clean switch
  (no legacy `agents/<name>.md` fallback). Kill the duplicate resolver in
  `exec_prompt.rs`; retire the `AGENTS.md` relic.
- **Skills disk-loader + progressive disclosure** — scan `skills/<name>/SKILL.md`
  (agent-style frontmatter `name`/`description` → `<loadable-index>`; body → injected
  on enable). Feeds the existing `load`/loadable machinery. Disk presence = *available*;
  enabled = agent's list + runtime toggles.
- **`/skillname` REPL trigger** — emits a `Toggle{kind:SKILL, enabled:true}` delta (persistent,
  undoable — undo truncates the delta). Same path as the model's `load()`. A new
  `repl_input` handler alongside `$()`/`>(N)`/`@`.
- **`gait init` rework** — drop submodule/codebase relic; `gait init [path]` (path or
  cwd, like `git init`); `--help`/`-h` prints usage with **no side effects** (fixes the
  bug that committed to `main` this session); scaffold the new layout
  (`agents/default/AGENT.md`, `USER.md`, `skills/`); concise hint output with the
  tab-spaced committed/ignored table; fix the stale `AGENTS.md` staging.

---

## 6. Decisions (round 2, 2026-06-12)

- **O1 [locked, amended by I2/I3] — toggleable capabilities.** One generic delta
  `Toggle { kind: skill|mcp|tool, name, enabled }` (OR-Set per kind) covers skills, mcp, tools.
  Plus `ModelChange { model }` (LWW). **No `PermissionGrant`** — runtime permissions reuse the
  existing `PermissionsReply` delta (I3). Tools are toggled in the model, but the runtime
  tool-gating consumer is a deferred phase (I2); skills/mcp gate today.
- **O3 [locked] — no restriction concept.** An agent's `tools:` is an **enable-set**, not
  a restriction. "Reviewer can't write" = just don't enable `write` (not-enabled = absent).
  Enable-set gives restriction-by-omission for free, so the separate restriction idea is
  dropped.
- **O4 [locked] — skills disk-only, enabled via config.** A `skills/<name>/SKILL.md` on
  disk = *available/loadable*. *Enabled* is config-driven (where that config lives = O2).
  No inline skill bodies in `gait.jsonc`.
- **O5 [locked] — hooks stay OUT of the agent.** Hooks are workspace-level, in
  `gait.jsonc`, not in the `Agent` envelope. (So the envelope = model,
  instructions, tools, skills, mcp, permissions — no hooks.)
- **O6 [locked] — `*Checkpoint` naming.** `Compaction → Summary`,
  `Manifest → Agent`. Projections stay `ActiveSummary` / `ActiveAgent`.

### O2 [locked] — `gait.jsonc` owns "installed", `AGENT.md` owns "enabled"

The clarifying axis: **installed vs enabled** (like a package manager — the workspace
*installs* a capability; an agent *enables* a subset; runtime `Toggle` deltas flip
enabled-state live).

- **`gait.jsonc` = INSTALLED + policy/fallback** (never *enabled* state):
  - *installed*: `mcp_servers` (connection defs), `tools` (name→cmd registry). Skills are
    *installed* by presence on disk (`skills/<name>/SKILL.md`), not listed here.
  - *runtime*: `compaction_threshold`, provider/api, `output_schema_retries`, `hooks` (O5).
  - *policy/fallback* (neither installed nor enabled, named explicitly): a **fallback
    default `model`** (so a minimal `AGENT.md` boots) and a workspace **`deny` floor**
    (monotonic union — composes under every agent).
- **`AGENT.md` = ENABLED + identity** — the `Agent` seed: `model` (identity;
  else the gait.jsonc fallback), `allow`/`deny` (agent adds to the floor),
  `skills:`/`mcp:`/`tools:` (the **enabled** subsets of what's installed), body =
  instructions.

The `Agent` envelope's `skills`/`mcp`/`tools` fields *are* the enabled set;
the installed registry is **not** in the envelope (it lives in gait.jsonc/disk and feeds
the `<loadable-index>`). This also subsumes the old "merge" sub-question: an agent's
enabled set composes on the installed registry; nothing to union.

## 7. Out of scope / non-goals

- **Envelope simplification** — the Lamport `seq`/`parents` DAG is "populated now,
  folded later" and unused. If gait stays single-writer it's droppable, but that's a
  *separate* cleanup, not part of this model. CRDT-via-commutative-deltas is kept.
- **WASM plugins** — `WasmPluginDef` stays a stub.

## 8. Phasing (each a standalone, testable plan)

1. **Projection trait split** — add `checkpoint()`, generalize `run_projection`, move
   `ActiveSummary` compaction into it. **Reset stays positional; `through_seq` untouched**
   (C1/M5). *Zero behavior change* — verified by an added **two-compaction** test (C2).
2. **`Manifest` → `Agent`, `Compaction` → `Summary` rename** — proto + codegen + call sites,
   **plus serde aliases for old JSON keys + a pre-rename `log.jsonl` fixture test** (I1).
   Drop `output_schema_retries` from the envelope (M4).
3a. **`ActiveAgent` (behavior-identical)** — new projection replacing `ActiveManifest` +
   `ActiveLoadables` with the **same** results (`load()` still emits `ToolCall`, folded as a
   compat path). Seeded from config (M3). Pure projection collapse, no new events.
3b. **New delta events** — `Toggle{kind,name,enabled}` + `ModelChange`; wire `load()` and the
   model to emit `Toggle` instead of the compat path. **No `PermissionGrant`** (I3).
4. **Markdown-context resolver + agents → dir form** (clean switch).
5. **Skills disk-loader + progressive disclosure.**
6. **`/skillname` trigger** (emits `Toggle{kind:SKILL}`).
7. **`gait init` rework.**
8. *(future, separate)* **Tool-gating consumer** — turn.rs filters the tool list by
   `ActiveAgent.tools` (I2). Not required for the projection collapse.

Phases 1–3b are the spine and land first; 4–7 are the consequences. Each phase: dogfood
split (keystones = me, mechanical = gait), verify build+test+diff per commit.

## Self-review notes
- Type consistency: `active_agent(events) -> AgentState`, `ActiveAgent{seed}`,
  `Projection::checkpoint`, delta events `Toggle{kind}` / `ModelChange` (no `PermissionGrant`).
- Replay-completeness: seed is an event (opening `Agent`) holding the **stable** prompt layers;
  the dynamic prompt suffix is a derived view (C3); fold never re-reads files.
- CRDT: per-field LWW + OR-Sets, applied positionally; converges under single-writer append
  (file-order == seq-order); multi-writer merge needs a seq-sort pass (deferred, §7).

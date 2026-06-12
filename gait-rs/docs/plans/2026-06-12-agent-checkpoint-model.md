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

## 1. The core model

The log vocabulary bifurcates into two roles:

- **Deltas** — the incremental worldline. `UserTurn`, `AssistantText`,
  `ToolCall`/`ToolResult`, `Cwd`, and the new `SkillToggle` / `PermissionGrant` /
  `ModelChange`. Each is a small, commutative mutation.
- **Checkpoints** — a baseline state for *one dimension*, valid as of a `through_seq`
  fold-coordinate. `Compaction` (the **message** dimension) and `Agent` (the
  **agent** dimension, renamed from `Manifest`). The shadow-git checkpoint commit is
  the **codebase** dimension's equivalent.

`Compaction` and `Agent` are **the same kind of thing** — state checkpoints — differing
only in which projection they baseline. Every projection then has exactly one shape:

```
state = fold( latest checkpoint for my dimension , deltas after it )
```

| dimension | deltas | checkpoint |
|---|---|---|
| messages | `UserTurn`, `AssistantText`, `ToolCall`/`Result` | `Compaction` |
| agent | `SkillToggle`, `PermissionGrant`, `ModelChange` | `Agent` (was `Manifest`) |
| codebase/env | `Cwd`, dirty-stamp | shadow-git checkpoint commit |

**"Compaction" generalizes** from a message-only operation to: *emit a checkpoint for
dimension X so older deltas can be GC'd.* You can compact the agent dimension the same
way you compact messages — collapse a long run of `SkillToggle`s into a fresh `Agent`
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

1. **Rename `Manifest` → `Agent`.** It is the **envelope** holding a full snapshot of
   every agent sub-state — which today's `Manifest` already half-is:

   ```proto
   // The agent-dimension checkpoint: a full snapshot of the composed agent state
   // as of `through_seq`. Each delta event mutates exactly one sub-state below.
   message Agent {
     string model                 = 1;   // LWW
     string instructions          = 2;   // composed system prompt (replay-complete)
     repeated string tools        = 3;
     repeated string mcp_servers  = 4;
     repeated string skills       = 5;   // ENABLED skill set as of the checkpoint
     PermissionPolicy permissions = 6;
     optional uint32 output_schema_retries = 7;
     uint64 through_seq           = 8;   // fold coordinate (NEW — matches Compaction)
     // hooks? — see [open] below
   }
   ```
   Field numbers stay in the existing `Manifest` slot (oneof arm `2`), renamed.
   The `oneof kind` arm becomes `Agent agent = 2;`.

2. **Add `through_seq` to the `Agent` checkpoint** so the fold knows its baseline
   coordinate, mirroring `Compaction.through_seq`.

3. **New delta event messages** (free oneof field numbers ≥ 53):
   ```proto
   message SkillToggle    { string name = 1; bool enabled = 2; }       // OR-Set op
   message PermissionGrant{ string pattern = 1; Scope scope = 2; }     // G-Set op
   message ModelChange    { string model = 1; }                        // LWW op
   ```
   `[open]` exact set — see §6.

---

## 3. Projection trait split **[locked]**

Generalize the fold so checkpoints are a framework concern, not an `ActiveMessages`
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

- `ActiveMessages::checkpoint` returns the summary baseline on `Compaction`
  (lifting today's special-case out of `run_projection`).
- `ActiveAgent::checkpoint` returns the snapshot on `Agent`.
- `ActiveManifest` and `ActiveLoadables` are **deleted**; `ActiveAgent` replaces both.

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
            Kind::SkillToggle(t)     => if t.enabled { st.skills.insert(t.name) } else { st.skills.remove(&t.name) },
            Kind::PermissionGrant(g) => { st.allow.insert(g.pattern.clone()); }
            Kind::ModelChange(m)     => st.model = m.model.clone(),
            _ => {}
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

The fold then **never re-reads** those files (they can change under a running session);
replay stays pure. Mid-session source changes (you edit `AGENT.md`) emit a **fresh
`Agent` checkpoint** (re-seed) — the same snapshot+delta pattern `ActiveMessages` uses
for compaction.

**Agent = capability bundle [locked].** `gait.jsonc` is the global baseline + registry
of what *exists*; an `AGENT.md` curates a composition on top (model, allow, deny, and —
new — `skills:`/`tools:`/`mcp:` enabled-at-start); runtime `load()` / `/skill` are
ad-hoc toggles. `manifest = compose(builtin, gait.jsonc, agent, runtime-deltas)`.

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
- **`/skillname` REPL trigger** — emits a `SkillToggle{enabled:true}` delta (persistent,
  undoable — undo truncates the delta). Same path as the model's `load()`. A new
  `repl_input` handler alongside `$()`/`>(N)`/`@`.
- **`gait init` rework** — drop submodule/codebase relic; `gait init [path]` (path or
  cwd, like `git init`); `--help`/`-h` prints usage with **no side effects** (fixes the
  bug that committed to `main` this session); scaffold the new layout
  (`agents/default/AGENT.md`, `USER.md`, `skills/`); concise hint output with the
  tab-spaced committed/ignored table; fix the stale `AGENTS.md` staging.

---

## 6. Open decisions (need your call before plans)

- **O1 — exact delta event set.** Minimum: `SkillToggle`, `PermissionGrant`. Also
  `ModelChange`? `McpToggle` (or reuse `SkillToggle`-shaped generic `Toggle{kind,name,on}`)?
  Tool enable/disable as a delta, or tools fixed at seed?
- **O2 — `skills`/`mcp` enabled-at-start merge** when both `gait.jsonc` and `AGENT.md`
  list them: union (additive) or agent-replaces-config (like `allow`)?
- **O3 — tool availability semantics.** For *tools* specifically, is an agent's `tools:`
  list a **restriction** (only these available — a reviewer can't `write`) or an
  **enable set** (all available, these on)? Skills are clearly enable-set; tools may want
  restriction.
- **O4 — `gait.jsonc` inline skills.** Disk-only (`SKILL.md`), or disk + inline override?
  (You leaned "two-part, like tools" — needs pinning to a concrete rule.)
- **O5 — hooks in the `Agent` envelope?** Hooks are agent-dimension config but aren't in
  `Manifest` today. Fold them in (so an agent can declare hooks) or leave in `gait.jsonc`
  global only?
- **O6 — `Compaction` rename?** Keep `Compaction`, or rename to `MessageCheckpoint` for
  symmetry with `Agent`? (Leaning keep — `Compaction` is well-understood.)

## 7. Out of scope / non-goals

- **Envelope simplification** — the Lamport `seq`/`parents` DAG is "populated now,
  folded later" and unused. If gait stays single-writer it's droppable, but that's a
  *separate* cleanup, not part of this model. CRDT-via-commutative-deltas is kept.
- **WASM plugins** — `WasmPluginDef` stays a stub.

## 8. Phasing (each a standalone, testable plan)

1. **Projection trait split** — add `checkpoint()`, generalize `run_projection`, move
   `ActiveMessages` compaction into it. *No behavior change* (pure refactor + tests).
2. **`Manifest` → `Agent` rename + `through_seq`** — proto + codegen + call sites.
3. **`ActiveAgent`** — new projection; delete `ActiveManifest`/`ActiveLoadables`; delta
   events (`SkillToggle`/`PermissionGrant`/…); wire `load()` to emit deltas.
4. **Markdown-context resolver + agents → dir form** (clean switch).
5. **Skills disk-loader + progressive disclosure.**
6. **`/skillname` trigger.**
7. **`gait init` rework.**

Phases 1–3 are the spine and land first; 4–7 are the consequences. Each phase: dogfood
split (keystones = me, mechanical = gait), verify build+test+diff per commit.

## Self-review notes
- Type consistency: `active_agent(events) -> AgentState`, `ActiveAgent{seed}`,
  `Projection::checkpoint`, delta events `SkillToggle`/`PermissionGrant`/`ModelChange`.
- Replay-completeness: seed is an event (opening `Agent`), fold never re-reads files.
- CRDT: per-field LWW + OR/G-Sets, ordered by existing `(seq,id)`.

# Proto Log Schema — Design & Implementation Plan

**Date:** 2026-06-10
**Status:** Design v1.1 (code-grounded; reviewed)
**Artifacts:** [`proto/log.proto`](../../../proto/log.proto) (the log) ·
[`proto/state.proto`](../../../proto/state.proto) (`gait state` query contract)

> **For agentic workers:** this is a design spec with an embedded implementation
> plan. The proto schema in `proto/log.proto` is the canonical artifact; this doc
> captures the *why* and the *how*.

**Goal:** Make gait's event log a proto-defined, cross-language, CRDT-shaped,
content-addressed format — so Flutter/other-Rust consumers integrate against a
typed schema, the log is git-merge-robust, and the working transcript becomes a
genuinely mutable projection (redact/replace).

**Architecture:** proto3 is the **source of truth** for the log format; generated
Rust (prost + pbjson) replaces the hand-written `AnyEvent`. The wire stays **JSON**
(pbjson canonical, camelCase) so transcripts remain git-readable. The log is an
**op-based CRDT**: a grow-only event set + LWW-per-target mutations, folded by the
**Retrace** into working state.

**Tech stack:** prost + prost-build, pbjson + pbjson-build, vendored `protoc`,
`google.protobuf.{Struct,Value,Duration}` well-known types. Dart generation is
**out of scope** (other agents own it, from the same `.proto`).

---

## 1. Decisions & rationale

### 1.1 proto is the format (not a boundary mapping)
We considered keeping serde internally and mapping to proto only at the broker
edge ("anti-corruption layer"). Rejected: gait has no separate public-vs-internal
event split today, the same owner controls gait + pocketcoder + Flutter, and a
mapping layer means two definitions that silently drift. proto-as-truth is the
non-half-measure; proto's additive field-number rules give the (currently
ad-hoc-evolving) vocabulary real backward/forward-compat discipline.

### 1.2 JSON wire, externally-tagged, camelCase
We sacrificed byte-compat with today's internally-tagged snake_case format (the
user is unattached to the current schema). proto3 canonical JSON is
externally-tagged (`{"payload":{"userTurn":{…}}}`) and camelCase — frictionless
for generated consumers. This is a **hard-cut migration** (see §4).

### 1.3 CRDT shaping — `(seq, id)` + `parents`
The fold must be a pure function of the event **set**, not file position, so
merged/rebased/resynced logs (session logs are git branches!) converge.
- **`seq`** — a logical Lamport clock (`max(parents.seq) + 1`, degenerating to
  `prev.seq + 1` linearly). The order key. Removes wall-clock skew from
  correctness. Chosen over `(ts, id)`.
- **`id`** — a random **UUIDv4**: identity (dedup on merge) + LWW tiebreak.
- **`content_hash`** — `hash(JCS(body))`: integrity only. Separate from `id`
  because identity ("same occurrence?") and integrity ("inside changed?") are
  different jobs — a UUID makes two identical-payload events (e.g. `ls` twice)
  distinct, which a content-hash-as-id could not. JCS (RFC 8785 canonical JSON)
  so any language can recompute it.
- **`parents`** — the causal DAG (git's model). `[]` root, `[prev]` linear,
  `[a,b]` merge; a fork is two events sharing a parent. **Populated now, folded
  later** — branching logic is deferred, but recording parents means it lands
  additively with zero migration.
- Not built: vector clocks, a Merkle parent-hash chain (level-C), or any CRDT
  library. The hash chain would duplicate git at rest.
- **`seq` monotonicity (corrected per review B2):** in the *linear* regime
  (today, no forks) `seq` is strictly `prev+1`, so a gap detects a dropped event.
  Once branching lands (deferred), `seq` becomes DAG-Lamport (`max(parents.seq)+1`)
  and concurrent events can share a `seq` — `seq` is then **not** append-monotonic
  and gap-detection no longer holds; drop-detection moves to **parent-reachability**
  (every referenced `parents` id must be present). Both regimes are honest; the two
  properties don't coexist.

### 1.4 The envelope (locked)
`version, id, seq, ts, content_hash, parents`, then `oneof body { Payload | Redact
| Replace }`. `version` is per-event (the log is resumable across binary
upgrades) and lives at the top level, so it versions the whole body including a
nested `Replace.replacement`. `ts`/`content_hash`/`version` are **metadata** — not
in the CRDT math (the key is `(seq, id)`).

### 1.5 Two-layer consistency
- **Generative spine** (which turns happened, in what causal order) — a causal
  DAG, like git history. NOT commutative: re-running the same context with a
  different model is a **fork**, not a merge.
- **Mutations** (redact, replace, loadable toggles, model switches) — LWW,
  commutative, convergent.
CRDT convergence guarantees agreement on the merged *set*, not that a forked
conversation is coherent — forks are kept as alternatives (git branches), not
auto-merged.

### 1.6 `Resolution` event — model/tools are worldline, not config
`Session.model` is dropped (it was a holdover; the design doc already scopes model
off the durable token). The live model lives on `ResolvedProfile`; the **history**
lives in the Log as a recurring `Resolution` event (model + tools + mcp + skills +
permissions + full instructions), **LWW-projected**. First instance = session
opening. This closes the "per-turn model provenance is config-only" gap and
implements the design doc's re-resolution/drift contract (an accepted drift is a
logged fact). Generalizes the proven `derive_enabled_loadables` pattern.

### 1.7 `SessionStart` shrunk to identity
`SessionStart` was a mini-`TurnOpts` god-bag (identity + resolution + codebase).
Split by concern: identity → a pure-identity root event (`session_id`,
`assigned_by`); resolution → `Resolution`; codebase → the first turn's
`CodebaseState` stamp. The root event is self-describing (identity survives the
git→broker boundary). Own-session identity is **not** repeated per-event; only
cross-agent references (`AgentUpdate.session_id`) name another session.

### 1.8 Typed `Replace` over a reusable `Payload`
`Replace` hands back a fully-typed replacement `Payload`, not a text string — so
it can rewrite any substantive event with a properly-typed new one. `Payload`
(substantive events only) is referenced by both `Event.body` and
`Replace.replacement`: DRY (types listed once) and no Replace-of-Replace
recursion. Same-type-as-target is a convention validated **in code**, not the
schema.

### 1.9 Redact/Replace fold (the mutable transcript)
Reuses the LWW-per-entity replay (entity = target event id). For each target, the
latest mutation by `(seq, id)` wins → working state is *normal / replaced /
removed*. Redact and Replace mutually override (a later Replace un-redacts; the
Log keeps everything). **Cascade:** redacting either member of a `ToolCall`/
`ToolResult` pair (matched by `call_id`) removes the whole pair atomically, else
the provider rejects an orphan. Cascade is computed from the *converged* LWW state
and overrides an orphaned result's own Replace — deterministic, so convergence
holds. Scope this round = **events + fold only; no producer tool** (model can't
self-redact until a later spec).

### 1.10 Enums for closed sets
`ErrorPhase`, `ClarifyKind`, `AgentStatus`, `Decision`, `Scope` — each with
`*_UNSPECIFIED = 0` (absorbs legacy/None, e.g. `Scope` for legacy null replies).
Open-ended ids (`from`, tool `name`) stay strings.

### 1.11 Dead code removed (found by reading the emitters)
- **`CommandResult`** — defined, never emitted; shell goes through
  `ToolCall→ToolResult`. Dropped. Its *intent* (exit/timing) returns as structure:
  generic `ToolResult.duration` (every tool) + `ProcessResult{exit_code,
  timed_out}` (shell only). `shell.rs:118` stops baking `"exit N (Xms)"` into the
  output string.
- **`LinkRef` + `links` + `to`** (on Clarify/Inform items) — never constructed/
  read. Dropped, with the `relation` enum.
- **`"instruct"` decision** — never emitted (only approve/deny). Dropped.
- **`pending` array** in `gait state --json` — a hand-built partial duplicate of
  `state.items`. Dropped; typed items replace it.

### 1.12 Workflow deferred
A workflow is a *script* (a definition — its own proto → yaml/json, a layer on
top), distinct from the log of what happens when it plays out. The workflow
subsystem is being redesigned separately, so its events (`WorkflowStart`,
`Node*`, `HookRun`) are excluded from this vocabulary. Payload field numbers
20–49 are left for them; the opaque `RawEvent` arm keeps legacy workflow lines
decodable in the meantime.

---

## 2. Schema overview

See [`proto/log.proto`](../../../proto/log.proto). Vocabulary:

| Category | Messages |
|---|---|
| Envelope | `Event` (version,id,seq,ts,content_hash,parents + body oneof) |
| Substantive (`Payload`) | `SessionStart`, `Resolution`, `UserTurn`, `AssistantText`, `ToolCall`, `ToolResult`, `Clarify`/`ClarifyReply`/`ClarifyConvergence`, `Inform`/`InformReply`/`InformConvergence`, `Permissions`/`PermissionsReply`, `AgentUpdate`, `TurnComplete`, `Compaction`, `Error`, `RawEvent` |
| Mutations | `Redact`, `Replace` |
| Value objects | `CodebaseState`, `PermissionPolicy`, `TurnUsage`, `TurnOutput`, `CompactionUsage`, `ProcessResult`, `ConvergenceItem`, the `*Item` types |
| Enums | `ErrorPhase`, `ClarifyKind`, `AgentStatus`, `Decision`, `Scope` |
| Projections (separate contract — `proto/state.proto`, `gait.state.v1`) | `SessionStateReport`, `SessionState` + `Idle`/`Awaiting*` — references log item types |

---

## 3. Implementation plan

> Subagent-driven (see `superpowers:subagent-driven-development`). Each phase ends
> green (`cargo test`) before the next. Dogfood gait/M3 as implementer per the
> established loop.

### Phase 0 — Build wiring ✅ DONE (commit 539a361)
- Added `prost`/`prost-types`/`pbjson`/`pbjson-types` + build deps
  `prost-build`/`pbjson-build`/`protoc-bin-vendored` (vendored `protoc`,
  zero-config). `sha2`/`serde_jcs`/`hex` for the hash.
- `build.rs` compiles **both** `proto/{log,state}.proto`; WKTs map to
  `pbjson-types` (camelCase canonical JSON). Generated types land under
  `proto::gait::{log,state}::v1` (module tree mirrors package paths so state's
  cross-package refs resolve).
- `proto::hash::content_hash` = SHA-256 over `"v"+version+"."+JCS(body)` (the
  pinned pipeline). 4-test gate: round-trip lossless · wire is canonical proto3
  (camelCase, externally-tagged, default-omission) · hash recomputes stably + is
  body/version sensitive.

### Phase 1 — Envelope, identity, hashing ✅ DONE (commit 15b50a2)
- `proto::envelope::mint` builds an `Event`: `id` (UUIDv4), `seq` (`prev.seq+1`;
  `1` for root, `0` reserved as unset/malformed), `ts`, `parents` (`[prev_id]` /
  `[]`), `version`, `content_hash` stamped last. `verify_hash` recomputes the body
  hash and rejects an unset body. Pure functions, fully unit-tested.
- **NOTE (phasing correction):** the emit-path rewire originally listed here moved
  to **Phase 2** — `emit_event`/`append_event` take `&AnyEvent` and have ~70 call
  sites, so they can't write generated `Event` lines until those call sites are
  converted off `AnyEvent`, which IS the Phase 2 work. Reading the tail for `seq`
  on append/resume also lands there (no caller until emit is rewired).

### Phase 2 — Generated types replace `AnyEvent`; emit rewire; `Session.model` factor-out
Decomposed into green sub-units (the format flip is atomic, so it was split into
a converter + a coordinated read-side flip rather than one big-bang):
- **2.1 ✅ (fbca17d):** `proto::wire` — `emit`/`append`/`read_events`/`last_event`
  over `Event` (mint off the tail) + `body(kind)` / `raw(type, json)`.
- **2.2a ✅ (c0a7d2a):** `proto::convert::any_to_body` — the single tested place
  encoding the restructuring (SessionStart→identity, enum-strings→enums, Value→
  Struct, workflow/CommandResult→RawEvent). Migration scaffolding.
- **2.2b ✅ (3ed4866):** on-disk format flipped to proto. `emit_event`/`append_event`
  convert + write via wire (the ~53 write call sites stay untouched); ALL readers
  (retrace, session_state, count/last_compaction, list_agents, workflow/state via
  RawEvent, turn, chat, bin, integration tests) rewritten over `Event`. `AnyEvent`
  + `convert.rs` retained as write scaffolding.
- **2.2c ⏳:** migrate the ~53 write call sites to build `Payload` directly (via
  new ergonomic constructors), then DELETE `AnyEvent`, the `*Item` structs,
  `CommandResult`/`LinkRef`/`relation`, `convert.rs`, and the old emit shims.
  Incrementally green (each site flips independently; deletion lands once all do).
- **2.3 ✅ (4704a2e):** Dropped `Session.model` (live model on `ResolvedProfile`);
  added `events::resolution_msg`/`resolution`/`last_resolution`; `run_turn` emits a
  `Resolution` at run start under a drift guard (skip if identical to the last
  logged one). Write-only; the retrace LWW projection is Phase 3.
- **2.4 ✅ (f77ac5a):** `execute_tool_call` times every `tool.execute()` into
  `ToolResult.duration`; shell returns a structured `{output, exit_code, timed_out}`
  object that `execute_tool_call` unwraps (clean `output` to the model) + lifts into
  `ToolResult.process`. `shell.rs` no longer stringifies `"exit N (Xms)"`.

**Phase 2 COMPLETE** — proto is the sole event format end to end (write + read + on
disk). Remaining original scope: Phase 3 (unified Retrace projections incl. the
`Resolution` LWW-Register) and Phase 4 (Redact/Replace mutations + migration cut-over).

### Phase 3 — Unified Retrace (five projections)
The Retrace folds the Log (sorted by `(seq, id)`) into:
1. **message list** (Transcript) — honoring Compaction `through_seq` + the pairing
   cascade;
2. **enabled loadables** (existing LWW-Map);
3. **active resolution** (new LWW-Register over `Resolution`);
4. **redact/replace** working-transcript mutations (new LWW-per-target, §1.9);
5. **`SessionState`** (the tail-scan projection).

### Phase 4 — Mutations + migration
- `Redact`/`Replace` fold + cascade + LWW (Phase 3.4), with the
  converged-state precedence rule for cascade × orphan-Replace.
- `RawEvent` round-trips unknown/legacy lines.
- **Migration: hard cut.** Bump `version`; regenerate/discard test transcripts; no
  dual-read of the old serde format.
- `SessionStateReport` for `gait state` (drop `pending`).

### Out of scope (this effort)
- Branching **fold** (parents populated + reserved, not walked).
- Producer **tools** for redact/replace (model-facing trigger).
- **Dart** generation (other agents, same `.proto`).
- **Workflow** event redesign (deferred; `RawEvent` bridges).
- Merkle parent-hash chain, vector clocks, `origin` field.

---

## 4. Migration & rollout
Hard cut. The format changes (externally-tagged camelCase) and old transcripts are
not read by new gait. Test fixtures are regenerated. `version` starts at the next
generation; readers dispatch by per-event `version`. `RawEvent` ensures any
unmodeled/legacy line still decodes rather than crashing the reader.

## 5. CRDT-safety argument
Mutations form an LWW-Register per target: ops are a G-Set (union-merge, dedup by
`id` — C/A/I), and each target's value is `max` by the **total, tie-free** order
`(seq, id)` — a deterministic function of the op-set. Merge = union + recompute,
so all replicas converge (strong eventual consistency). The cascade is a
deterministic function of the converged state, preserving convergence.

## 6. Open items for review
- **`node_type` enum** — deferred with workflow; when revived, mirror
  `node_type_name()`'s full taxonomy as a `NodeType` enum.
- **`routed_to = "__gate_paused__"`** magic-string smell (workflow) — model as
  state during the workflow redo.
- **`content_hash` scope** — currently `hash(JCS(body))`; confirm whether to bind
  envelope fields (then it approaches the deferred Merkle chain).
- **`seq` on resume/merge** — confirm `max(parents.seq)+1` vs `max(all seen seq)+1`
  semantics at the fold/append boundary.

---

## 7. Review resolutions (v1.1)

Two independent reviews (proto/CRDT correctness; code-grounding). Both confirmed
all four dead-code drops and that no field is silently lost on a *live* event.
Resolutions:

**Applied to `proto/log.proto`:**
- `Compaction.through_seq` and `Resolution.output_schema_retries` → `optional`
  (unset ≠ 0).
- Explicit `reserved 7 to 15` (envelope) and `reserved 20 to 49` (Payload /
  workflow) — `reserved` enforces the deferral; comments don't.
- Note on `Event.body`: an unset oneof is a malformed event; the Retrace **must**
  hard-reject `body == None` (proto3 can't enforce "exactly one arm").

**`content_hash` — pinned pipeline (was under-specified):** the hash is
**SHA-256** over **JCS(RFC 8785) of the canonical proto3-JSON object of the `body`
oneof** — i.e. the externally-tagged object `{"<arm>":{…}}` (e.g.
`{"payload":{"userTurn":{…}}}`). The producer's proto3-JSON must use a **frozen**
field-omission setting; flipping it later changes every hash, so it is part of the
`version` contract. **`version` is prefixed into the hash input** so a flipped
`version` can't pass verification under a different scheme. `content_hash` attests
*body integrity given a trusted version*; it is not a standalone tamper-evidence
chain (that's the deferred Merkle level-C). A round-trip + recompute-hash test is
the **first Phase 0 gate**, since this only surfaces against generated code.

**Cascade call_id matching (review B1):** the `ToolCall`/`ToolResult` pairing
cascade matches on the **converged (post-LWW-`Replace`) payload's `call_id`**, not
the original event's. A `Replace` can change a result's `call_id`; pinning to the
post-Replace value makes two implementations converge to the same transcript.

**`SessionStart.assigned_by` is an ADDED field (review A1, grounding):** it does
not exist on today's `SessionStart` event — it's sourced from `session.json`
(`session/session_obj.rs:12`). Implementer must populate it in `init_session`
/ `InitSessionOpts` (`transcript.rs:331-337,416`), which have no `assigned_by`
today.

**Workflow `RawEvent` is load-bearing on day one (grounding D4):** `Workflow*` /
`Node*` / `HookRun` are **actively emitted today** (`workflow/runner.rs`,
`exec_gate.rs`, `bin/gait.rs:736`). So the `RawEvent` round-trip must work from the
first migrated build — it is not merely a legacy bridge.

**Designed-not-observed (grounding B5/B6, D1, D2):** `Error`, `Compaction`,
`AgentUpdate.items`, and `UserTurn.injected_files` are currently **never emitted /
always None** in the live code. Their shapes (esp. the `ErrorPhase` value set and
`through_seq`) are *designed*, not verified-from-emission — do not treat them as
observed. `UserTurn.from` *is* live (`spawn_agent.rs:81`); keep it.

**Resolved judgment calls:**
- **`Resolution` LWW granularity (review B3):** **whole-snapshot LWW-Register**
  (no deltas). Any drift re-emits the full `Resolution` incl. `instructions`.
  Accepted: drift is rare, simplicity wins.
- **Projection coupling (review B5):** **split.** `SessionState`/`Awaiting*` +
  `SessionStateReport` moved to `proto/state.proto` (`gait.state.v1`), which
  `import`s `log.proto` and references `gait.log.v1.{PermissionItem,ClarifyItem,
  InformItem}`. The state-query envelope now evolves independently of the log; the
  item shapes stay defined once in `log.proto` (a clarify item is a clarify item in
  both). The build compiles **both** protos.
- **`RawEvent.payload` is `Struct` (object-only):** confirmed fine — all legacy
  `AnyEvent` bodies serialize to JSON objects.

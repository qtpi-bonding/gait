# Architecture plan: gait's turn I/O boundary

Status: draft for review. This is the umbrella plan; each phase has its own spec:

- Phase 1 — [`../specs/2026-06-13-stream-output-sink.md`](../specs/2026-06-13-stream-output-sink.md) (output port + Recorder rename + result envelope)
- Phase 2 — [`../specs/2026-06-13-phase-2-tools-hooks-publish.md`](../specs/2026-06-13-phase-2-tools-hooks-publish.md)
- Phase 3 — [`../specs/2026-06-13-phase-3-workflow-boundary.md`](../specs/2026-06-13-phase-3-workflow-boundary.md)
- Phase 4 — [`../specs/2026-06-13-phase-4-input-port.md`](../specs/2026-06-13-phase-4-input-port.md)
- Phase 5 — [`../specs/2026-06-13-phase-5-embedding-api.md`](../specs/2026-06-13-phase-5-embedding-api.md) (reframed from cloud adapters → in-process embedding API)

Diagrams: [`../specs/2026-06-13-turn-io-ports.diagram.md`](../specs/2026-06-13-turn-io-ports.diagram.md).
Process: spec all phases → plan+impl Phase 1 → revisit Phase 2 spec → plan+impl
Phase 2 → … (each downstream spec is revisited after the prior phase lands).

## Goal

Give gait **one coherent I/O boundary**: a single, well-owned way that turn
output leaves the system and turn input enters it, consistent across *all three*
entrypoints (`gait run`, the REPL, the workflow runner). Concretely:

- **stdout has exactly one owner and one contract** — the machine-consumable
  result. Today no one owns it (see below).
- **Every turn-scoped event reaches live subscribers** through one path — including
  tool- and hook-originated events, which today silently bypass the channel.
- A new driver (a **cloud control agent**) can plug in by implementing a small
  port pair, with no changes to the turn.

## Non-goals

- Rewriting the event/log format, hashing, or `seq` chaining (untouched).
- Changing the REPL's *rendered* UX.
- Building the cloud transport itself (we build the seam it needs).

## Current state (verified inventory)

There are **three parallel output mechanisms**, and the three entrypoints each
use a different one:

| Mechanism | What it does | Used by |
|---|---|---|
| `Emitter::emit` / `delta` (`proto/emitter.rs:26,36`) | durable log **+** live mpsc channel | the **turn** (`turn.rs`, 26 sites) |
| `wire::emit` (`proto/wire.rs:79`) | durable log **+ `println!` to stdout**, no channel | the **workflow runner** (`workflow/runner.rs` ×11, `exec_gate.rs:15`, `gait.rs:914`) |
| `wire::append` (`proto/wire.rs:73`) | durable log **only** | tools, hooks, harness, replies, toggles, init |

Consequences, all verified:

1. **stdout has no single owner.** `cmd_run` re-`println!`s *every* channel Event
   and Delta as NDJSON (`gait.rs:327,330`) plus a `session_id` line (`:217`); the
   workflow runner echoes every workflow event to stdout via `wire::emit`
   (`wire.rs:81`); the REPL prints rendered blocks to stdout (`ui/renderer.rs`).
   So stdout simultaneously carries NDJSON *and* human-rendered text, from two
   subsystems, with no contract. **This is the root of the "can't redirect"
   bug** — and it is broader than `cmd_run`.
2. **The live token stream + event echo are on stdout, not stderr** (`gait.rs:327,
   330`) — the originating complaint.
3. **The final result is never explicitly emitted.** `turn_complete` is both a
   channel event (`turn.rs:857,1212`) *and* the `TurnResult::Done` return value
   (`:864,1219`); `cmd_run` prints neither as a distinct payload — it's buried in
   the NDJSON firehose.
4. **Tool- and hook-originated events bypass the live channel.** `Tool::execute`
   has no emitter (`types.rs:197`), so `shell` writes `cwd` via `wire::append`
   (`shell.rs:103`) and `config_hook` writes `hook_run` via `wire::append`
   (`config_hook.rs:58`) — log-only. A live viewer never sees them. The turn
   *does* hold the emitter exactly where it calls tools (`turn.rs:200`), so the
   handle is available; only the trait signature blocks it.
5. **The workflow path has no live channel at all** — the prompt node builds an
   `Emitter` with a dropped receiver (`exec_prompt.rs:92`), so sub-turn deltas
   vanish and workflow progress goes to stdout via `wire::emit`.
6. **Input is scattered across 6 surfaces**, only two of which (CLI/REPL task)
   reach the emitter (indirectly, inside `run_turn:690`). Replies
   (`ui/builder.rs`), toggles (`chat.rs:129`), permission CLI (`permission.rs:132`),
   subagent injection (`spawn_agent.rs:78`), and harness (`harness.rs:61`) all
   `wire::append` directly. **Replies already produce `event::Body`**, not a
   bespoke intent type.

### Corrected facts (do not regress)

The earlier spec asserted three things the code disproves; recorded here so we
don't repeat them:

- `wire::emit` is **not dead** — it is the workflow runner's whole output path.
- `session_start` is **commit-only** (`transcript.rs:231`), never on the channel;
  it cannot be "classified and routed" live.
- `turn_complete` exists on **both** the channel and the return value — naive
  "Result-class to stdout" would double-emit the payload.

## Target architecture

The event **log is the central store** (hub). The `Emitter` is the single
write+publish primitive: it commits durably, then publishes a live copy to a
channel. Everything a turn produces goes through it. Consumers are **adapters**
behind two small ports; the stdout/stderr split is a property of *one* adapter,
not of the system.

```
 input adapters ──▶ TurnSource ──▶ event::Body ──append/record──▶  LOG (store)
   (argv/stdin/cloud)                                                  │
                                              turn reads ◀─────────────┤
                                              turn emits ──Emitter──────┤ commit
                                                              └─publish─▶ channel ──▶ TurnSink adapters
                                                                                        ├ StdioSink (cmd_run): Result→stdout, Trace+Error→stderr
                                                                                        ├ ReplView   (REPL): render
                                                                                        ├ WorkflowSink: structured progress
                                                                                        └ RemoteSink (cloud)
```

### 1. One write primitive; eliminate the bypasses

The end-state: **all turn-scoped event production goes through the `Emitter`
(log + channel)**, including tools and hooks. This is the single biggest
structural change and it's what makes "every event reaches subscribers" true.

- Give `Tool::execute` (and the `Hook` methods) access to the emitter — the
  principled option is to thread it through the call (the turn already holds it
  at `turn.rs:200`). 17 trait impls take a mechanical signature edit (16 concrete
  tools + the `ToolDef` blanket impl; ~23 `execute` fns incl. tests); most ignore
  it. This replaces the `shell.rs:98` log-path-reconstruct append and the
  `config_hook.rs` baked-`tx_path` append with a real `record` — so `cwd`/`hook_run`
  finally reach live viewers (`shell` keeps its `ctx`, used for `active_cwd`).
- After this, `wire::append` survives **only** for genuinely out-of-band writes
  where there is no live channel to publish to: **cross-session** injection
  (`harness.rs:61`, `spawn_agent.rs:78` writing a *different* session's log) and
  **pre-turn** init (`session_start` in `init_session`). Those are not "the turn
  emitting"; they seed a log the turn will later read. This makes the invariant
  honest (see Invariants).

### 2. Classify at read, in the sink — no channel retype

`classify(&Event) -> {Result, Trace, Error}` is a pure, sub-microsecond match on
`payload::Kind`. It lives in one place and **any** consumer calls it at read time
(the durable log stores `kind`, which is finer-grained and already greppable). We
do **not** put class on the channel or in the log — that would retype the channel
(forcing every consumer, incl. the REPL, to change in lockstep) and re-introduce
routing policy into the data. "Classify once" = one `classify()` definition, not
one call.

### 3. Output port — `TurnSink`

A small consumer contract; each entrypoint is an adapter:
- **`StdioSink`** (`cmd_run`): routes by class — Result→stdout, Trace+Error→
  stderr; sets a `failed` flag → non-zero exit. The stdout/stderr split lives
  *here only*.
- **`ReplView`**: the existing `ui` renderer, already an adapter in spirit
  (`builder.handle_stream_item`); adopt the trait so it shares the driver.
- **`WorkflowSink`**: replaces the workflow runner's `wire::emit`→stdout with
  structured routing (its own Result/Trace split), bringing the workflow under
  the same contract and fixing *its* stdout conflation.
- **`RemoteSink`** (future): serializes to a transport.

The marquee `Result` payload on stdout is the **`TurnResult`** (the return
value), printed once by the run loop — *not* the channel `turn_complete` event,
which stays Trace (history/observability). This resolves the double-emit.

### 3a. `structured_done` is the first-class Result payload

The Result payload is not an ad-hoc blob — it is the **`structured_done`** turn
output (`tools/output.rs`, formerly `output`), promoted to first-class. The
turn's output is a single envelope, the same value that is the `turn_complete`
payload (logged) and the stdout Result:

```json
{
  "message": "...",            // LLM prose — streamed live to stderr; a string field here
  "result":  { ... },          // LLM-declared structured output
  "changes": {                // HARNESS-filled ground truth from the git checkpoint diff
    "added": [...], "modified": [...], "deleted": [...]
  }
}
```

- **`result`** — the LLM's declared output. When `--result-schema X` (or
  `gait.jsonc result_schema`) is set, the harness validates *this field* against
  `X` and **will not let the turn end until it conforms** — the validate +
  inject-error + retry (`output_schema_retries`) + hard-fail loop already exists
  (`turn.rs` TurnComplete path, structured-done spec). Hard-fail → Error class →
  non-zero exit. No schema → `result` is freeform/optional (the flexible default).
- **`changes`** — ground truth the model cannot fake. **This is net-new
  work, not free harness data** (corrected from an earlier draft): the per-turn
  delta is `diff(new_checkpoint, its_parent)` in the shadow git the checkpoint
  subsystem maintains. The shadow already commits a per-turn snapshot whose parent
  is the prior state (`git/checkpoint.rs:112`), so the diff is exactly this turn's
  changes — but **no diff method exists today**. Required:
  - a new `CheckpointRepo::diff(...) -> {added, modified, deleted}` (git2 `diff_tree_to_tree` over the two commits);
  - the checkpoint runs at turn **complete** (`turn.rs:846,1201`), not start — the
    baseline is the shadow HEAD seeded at `init` (`turn.rs:503`,
    `checkpoint.rs:72`);
  - **non-git universality** (per design intent): today `init` no-ops for non-git
    codebases and the shadow borrows the codebase's git objects via `alternates`
    (`checkpoint.rs:18-25,59`). Making the shadow self-contained (drop the
    early-return, conditional `alternates`, own object store — the unborn-HEAD
    path already exists at `checkpoint.rs:70-71`) lets it checkpoint *any*
    directory, so `changes` is universal rather than git-only.
  `changes` is always a sibling of `result`, never merged, never schema-bound;
  `Option`/empty when no checkpoint is available.
- **`message`** — prose; the live assistant-text deltas already go to stderr
  (Trace), and the final `message` rides the envelope as a JSON string.

**Composition is the point.** This one envelope is read identically by all three
consumers — workflow `$node.output.field`, `spawn_agent` parents, and shell
`gait run | jq .done.result` / `jq .done.changes.modified` (stdout is the proto
`RunResult` oneof). The agent produces
artifacts on disk; the harness certifies what actually changed; everyone
downstream reads one typed record. This makes the output port's reason to exist
concrete: deliver the agent's certified, typed result to whoever is composing.

This extends the (already-done) `structured_done` spec
(`../specs/2026-06-12-structured-done.md`): the schema-enforcement machinery for
`result` is **reused**; the `changes` field, the `CheckpointRepo::diff` +
non-git work, and the `TurnOutput`/`TurnResult::Done` envelope extension (both are
2-field today, `turn.rs:43` / `transcript.rs:30`) are **net-new and land with
Phase 1** (see Phasing) so the stdout contract is correct on first print.

### 4. Input port — currency is `event::Body`, not a bespoke intent enum

Because replies already produce `event::Body` (`ui/builder.rs`), the input
boundary's currency is `event::Body` (what the log stores). A `TurnSource` yields
the next `event::Body` to append; each adapter maps its native input:
- **`ArgSource`** (CLI): `--task` → `user_turn` body, once.
- **`ReplSource`**: the `chat.rs` dispatcher + `ui/builder` replies → bodies.
- **`RemoteSource`** (future): cloud commands → bodies.

This avoids the circular `String → Input → to_event → Body` round-trip the review
flagged; the only real mapping is `ArgSource`'s one `String → user_turn`.

### 4a. The machinery's input is two sources folded through the log

The input port is only *half* the input side. The machinery (the LLM loop) does
not consume the new input directly — its prompt is built from **two sources**:

1. **New input** — from the input port (cli/repl/cloud), and
2. **`retrace(log)`** — the deterministic fold of the event log into conversation
   state (`retrace::messages`, `active_agent`, `active_cwd`, `active_mutations`,
   `fold()`; permissions via the `ActivePermissions` projection surfaced through
   `fold()`'s `effective_allow`, not a free function).

These compose into **"the folded thing we send"** — the actual message list +
state handed to the provider. The composition is **append-then-fold**: the new
input is appended to the log as an `event::Body`, then `retrace(log)` folds the
*whole* log (now including it) into the prompt. The new input is simply the newest
event — the same way the output side records events *back* to the log; the cloud
API is not special.

**Two honest exceptions** the clean picture hides (named here like deltas are on
the output side):
- The `<status>` turn-context block (codebase hash, dirty files, cwd) is
  **prepended to the folded messages but never logged** (`turn.rs:696`) — an
  ephemeral prompt prefix, not part of the fold.
- The fold can trigger a **write back**: when agent state has drifted, the turn
  records a reconciling `Agent` event after folding (`turn.rs:677-679`). So the
  input side is read→fold→(sometimes)record, not purely read.

This exposes the system's structure — **two log-derived read paths with the
machinery between them.** They are symmetric in *direction* but asymmetric in
*nature*, and the difference matters:

| | Input side (`retrace`) | Output side (`classify`) |
|---|---|---|
| shape | stateful **fold** over the whole log | stateless **map** per event |
| cost | O(log length), order-dependent | O(1) per event |
| obligation | **deterministic / bit-reproducible** (feeds the drift-equality check, `turn.rs:677`) — change-controlled | presentation only — free to change without corrupting history |
| path | correctness / replay | presentation / routing |

So: `retrace` and `classify` are both projections of the one log — one *forward*
into a prompt for the model, one *outward* into destinations for consumers — but
`retrace` is a reduction in the trust path and `classify` is a map in the
presentation path. Not interchangeable duals; siblings of different weight. The
input port (§4) only gets the new event *into* the log; `retrace`, which already
exists, does the fold.

### Invariants (honestly scoped)

- **Within a turn, event production goes through the `Emitter` and does both** —
  commit (durable) then publish (channel). After §1, tools and hooks included.
  No turn-scoped log-without-publish.
- **Deltas are the named ephemeral exception** — publish-only, never committed.
- **`wire::append` is the out-of-band / cross-session primitive** — used only
  where there is no live channel for the target log (harness, subagent seeding,
  pre-turn init). Not a turn-emit path. This is a deliberate, enumerated set, not
  a vestige.
- **proto stays free of routing policy** — `classify` is Rust, read-time.

## Phased migration

Each phase is independently shippable and leaves the tree green. Ordering note
(per review): Phase 1 is the highest-value slice; **Phase 3 (workflow) is the
riskiest**, not Phase 2. The `Emitter → Recorder` rename is reviewability, not
risk-reduction — its blast radius is `turn.rs`, exactly what Phases 1–2 rewrite,
so a standalone rename creates a rebase seam; it is therefore folded **into**
Phase 1 (same file, same review pass) rather than a separate phase.

**Decision to close first:** open-decision #3 (`turn_complete` → Trace, the
`TurnResult` return value is the sole stdout Result). Phase 1 depends on it.

**Phase 1 — Output port for `gait run`, with the Recorder rename and the result
envelope (the bug fix + the contract).** Bundled because they all touch the same
surface and Phase 1's stated goal is "stdout has *one* contract" — shipping a
2-field Result now and adding `changes` later would break that contract:
- `Emitter → Recorder` rename (`emit→record`, `delta→publish_delta`; ~26 sites,
  mostly `turn.rs`; zero behavior change).
- `classify` (read-time, pure) + `StdioSink`; rewrite `cmd_run`'s consumer loop to
  route by class. Channel type unchanged; REPL untouched; `wire::emit` untouched.
- Extend the envelope to `{message, result, changes}`: extend `TurnOutput` /
  `TurnResult::Done` (2-field today), add `CheckpointRepo::diff` +
  the non-git-shadow enhancement, fill `changes` from the per-turn checkpoint
  diff. `result` schema-enforcement is reused as-is.
- Print the `TurnResult` envelope once on stdout (Result); `turn_complete` event
  stays Trace.
- Extract `cmd_run → cli/run.rs` + testable `RunArgs::parse`.
- *Note:* this phase **adds a read layer without removing any write mechanism** —
  the 3→1 write-path consolidation lands in Phases 2–3. Consider splitting the
  pure `cli/run.rs` extraction from the behavioral routing change into two commits.

**Phase 2 — Tools & hooks publish.** Thread the `Recorder` into `Tool::execute` /
`Hook` methods (the handle is already in scope at the call site, `turn.rs:200`;
`Emitter` is `Send+Sync`, so `&Recorder` threads through the async call cleanly);
replace the `shell`/`config_hook` `wire::append` bypasses with `record`. Now
`cwd`/`hook_run` reach subscribers. Mechanical across **17 trait impls** (16
concrete tools + the `ToolDef` blanket impl; ~23 `execute` fns incl. tests);
`shell` keeps its `ctx` (used for `active_cwd`), only the append is replaced.
*Makes the "one write path" invariant real.*

**Phase 3 — Workflow under the boundary (riskiest; split 3a/3b).** The workflow
emits via inline synchronous `wire::emit`→stdout with no channel.
- **3a:** stand up a channel + a consumer task in `run_workflow` (it's `async`);
  rewrite the 11 `wire::emit` raw-event sites to `record`; add `classify` arms for
  the opaque `wire::raw` workflow events (`workflow_start`, `node_*`, …).
- **3b (the load-bearing change):** re-plumb the dropped-receiver sub-turn emitter
  (`exec_prompt.rs:92`) so sub-turn deltas/events reach the same channel.
*Brings the third entrypoint into the boundary.*

**Phase 4 — Input port.** `TurnSource` (currency `event::Body`) + `ArgSource`;
then `ReplSource`, collapsing `chat.rs` and `cmd_run` onto one run loop — done
when the REPL is refactored (it has its own view-layer track). *Adds the input
half once a second driver (REPL) justifies the abstraction.*

**Phase 5 — Cloud adapters.** `RemoteSink` + `RemoteSource` over a transport;
decide whether to tee input events to the channel so remote observers see input
live. *The payoff the whole boundary was shaped for.*

## Open decisions

1. ~~**Phase 0 rename**~~ — **decided: keep**, folded into Phase 1 (not a
   standalone phase — avoids a `turn.rs` rebase seam).
2. **Phase 2 mechanism** — `Recorder` in the `execute` *signature* (per-call,
   cleanest; async-safe — verified) vs. injected into `ToolCtx` at construction
   (requires reordering construction after emitter creation). Plan assumes the
   signature.
3. **`turn_complete` class** — **close before Phase 1**: confirm it becomes Trace
   and the `TurnResult` envelope is the sole stdout Result. (Recommended; resolves
   double-emit.)
4. **Phase scope to commit to now** — Phase 1 (now including rename + envelope) as
   the next implementation target, or spec Phases 1–3 together before code?

## Appendix: key file:line references

- Write primitives: `proto/emitter.rs:26,36`; `proto/wire.rs:73,79`.
- stdout offenders: `bin/gait.rs:217,328,330`; `proto/wire.rs:81` (+ all
  `workflow/runner.rs` sites); `ui/renderer.rs:187,219-229`.
- Result paths: `turn.rs:857,1212` (channel) vs `turn.rs:864,1219` (return);
  envelope today 2-field (`turn.rs:43`, `transcript.rs:30`).
- Checkpoint/changes: `git/checkpoint.rs:9` (`checkpoint`, returns SHA, no
  diff), `:18-25` (non-git no-op), `:59` (alternates borrow), `:70-71` (unborn
  path), `:112` (parent); `turn.rs:503` (init), `:846,1201` (checkpoint at
  turn-complete).
- Tool/hook bypass: `types.rs:197` (no emitter); `shell.rs:103`;
  `config_hook.rs:58`; tool call site holding emitter `turn.rs:200`.
- Input surfaces: `gait.rs:317` / `turn.rs:690` (task); `turn.rs:696` (`<status>`
  prompt prefix, unlogged); `turn.rs:677-679` (Agent drift write-back);
  `ui/builder.rs:18,80` (replies → `event::Body`); `chat.rs:129` (toggle);
  `permission.rs:132`, `gait.rs:1084` (perm CLI); `spawn_agent.rs:78`,
  `harness.rs:61` (cross-session).
- Channel construct/consume: `gait.rs:314/324`; `chat.rs:214/261`;
  dropped-rx `turn.rs:394`, `exec_prompt.rs:92`.

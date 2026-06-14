# Architecture plan: gait's turn I/O boundary

Status: draft for review. Supersedes the narrower framing in
[`../specs/2026-06-13-stream-output-sink.md`](../specs/2026-06-13-stream-output-sink.md)
(which becomes the detailed spec for Phase 1). Diagrams:
[`../specs/2026-06-13-turn-io-ports.diagram.md`](../specs/2026-06-13-turn-io-ports.diagram.md).

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
| `Emitter::emit` / `delta` (`proto/emitter.rs:26,36`) | durable log **+** live mpsc channel | the **turn** (`turn.rs`, ~30 sites) |
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
  at `turn.rs:200`). 16 `Tool` impls take a mechanical signature edit; most
  ignore it. This **deletes** the `shell.rs:98` log-path-reconstruct hack and the
  `config_hook.rs` baked `tx_path`, replacing both `wire::append` bypasses with
  real `emit` — so `cwd`/`hook_run` finally reach live viewers.
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

### 4. Input port — currency is `event::Body`, not a bespoke intent enum

Because replies already produce `event::Body` (`ui/builder.rs`), the input
boundary's currency is `event::Body` (what the log stores). A `TurnSource` yields
the next `event::Body` to append; each adapter maps its native input:
- **`ArgSource`** (CLI): `--task` → `user_turn` body, once.
- **`ReplSource`**: the `chat.rs` dispatcher + `ui/builder` replies → bodies.
- **`RemoteSource`** (future): cloud commands → bodies.

This avoids the circular `String → Input → to_event → Body` round-trip the review
flagged; the only real mapping is `ArgSource`'s one `String → user_turn`.

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

Each phase is independently shippable, ordered lowest-risk-first, and each leaves
the tree green.

**Phase 0 — (optional) `Emitter → Recorder` rename.** Cosmetic; method
`emit→record`, `delta→publish_delta`. 26 call-sites + tests, zero behavior change.
Land alone if desired; not a prerequisite for anything. *Defer unless the naming
is actively wanted.*

**Phase 1 — Output port for `gait run` (the bug fix).** `classify` (read-time) +
`StdioSink`; rewrite `cmd_run`'s consumer loop to route by class; print
`TurnResult` once on stdout; extract `cmd_run → cli/run.rs` + testable
`RunArgs::parse`. Channel type unchanged; `wire::emit` untouched; REPL untouched.
Fixes the originating stdout/stderr bug. *Smallest valuable slice.*

**Phase 2 — Tools & hooks publish.** Thread the emitter into `Tool::execute` /
`Hook` methods; replace the `shell`/`config_hook` `wire::append` bypasses with
`emit`. Now `cwd`/`hook_run` reach subscribers; `StdioSink` shows them live on
stderr. Mechanical across 16 impls; behavior-additive. *Makes the "one write
path" invariant real.*

**Phase 3 — Workflow under the boundary.** Replace `workflow/runner.rs`'s
`wire::emit`→stdout with the channel + a `WorkflowSink` (or `StdioSink` reused).
Fixes the workflow stdout conflation; gives sub-turn deltas a live consumer
instead of a dropped receiver. *Brings the third entrypoint into the boundary.*

**Phase 4 — Input port.** `TurnSource` (currency `event::Body`) + `ArgSource`;
then `ReplSource`, collapsing `chat.rs` and `cmd_run` onto one run loop — done
when the REPL is refactored (it has its own view-layer track). *Adds the input
half once a second driver (REPL) justifies the abstraction.*

**Phase 5 — Cloud adapters.** `RemoteSink` + `RemoteSource` over a transport;
decide whether to tee input events to the channel so remote observers see input
live. *The payoff the whole boundary was shaped for.*

## Open decisions

1. **Phase 0 rename** — do it, or keep `Emitter`/`emit`? (Pure preference;
   reviewers call it churn, you wanted it clean.)
2. **Phase 2 mechanism** — emitter in the `execute` *signature* (per-call,
   cleanest) vs. injected into `ToolCtx` at construction (requires reordering
   construction after emitter creation). Plan assumes the signature.
3. **`turn_complete` class** — confirm it becomes Trace and the `TurnResult`
   return value is the sole stdout Result. (Recommended; resolves double-emit.)
4. **Phase scope to commit to now** — is Phase 1 the next implementation target,
   or do we want Phases 1–3 specced together before writing code?

## Appendix: key file:line references

- Write primitives: `proto/emitter.rs:26,36`; `proto/wire.rs:73,79`.
- stdout offenders: `bin/gait.rs:217,327,330`; `proto/wire.rs:81` (+ all
  `workflow/runner.rs` sites); `ui/renderer.rs:187,219-229`.
- Result paths: `turn.rs:857,1212` (channel) vs `turn.rs:864,1219` (return).
- Tool/hook bypass: `types.rs:197` (no emitter); `shell.rs:103`;
  `config_hook.rs:58`; tool call site holding emitter `turn.rs:200`.
- Input surfaces: `gait.rs:317` / `turn.rs:690` (task); `ui/builder.rs:18,80`
  (replies → `event::Body`); `chat.rs:129` (toggle); `permission.rs:132`,
  `gait.rs:1084` (perm CLI); `spawn_agent.rs:78`, `harness.rs:61` (cross-session).
- Channel construct/consume: `gait.rs:314/324`; `chat.rs:214/261`;
  dropped-rx `turn.rs:394`, `exec_prompt.rs:92`.

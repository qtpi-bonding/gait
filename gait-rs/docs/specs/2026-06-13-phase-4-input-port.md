---
status: done
---

# Phase 4 spec — Input port

Parent: [`../plans/2026-06-13-turn-io-architecture.md`](../plans/2026-06-13-turn-io-architecture.md).
Prereq: Phases 1–3 **and** the REPL view-layer track — **both landed**
(`status: done`). **Revised against current code** (post-Phase-3, commit
`0642da0`); anchors below are verified as of that point. The earlier draft was
explicitly forward-looking; this revision corrects a wrong premise it carried
(see *Reality check*).

## Goal

Give input the same boundary output got in Phases 1–3: a small `TurnSource` port
whose adapters turn native input (CLI arg, REPL line, later a cloud command) into
log events, and **one run loop** shared by `cmd_run` and the REPL instead of the
two separate hand-rolled loops that exist today.

## Reality check (what the earlier draft got wrong)

The draft assumed "`cmd_run` builds its `event::Body` inline" and that the only
real mapping left was `ArgSource: String → user_turn`. **That is not how the code
works today:**

- **The primary input is a `String`, not an event.** `cmd_run`
  (`cli/run.rs:37`, now extracted out of `bin/gait.rs` — the bin keeps only a
  path-resolving wrapper at `bin/gait.rs:177`) parses `--task` into
  `RunArgs.task: Option<String>` (`cli/run.rs:6,17`), expands it
  (`cli/run.rs:127-137`), and passes it as `TurnInput { task, .. }`
  (`cli/run.rs:149`). **`run_turn` itself appends the `user_turn`**
  (`turn.rs:682-691`): `if !did_resume { if let Some(task) = &task { …
  emitter.record(events::user_turn(task, None, Some(&ctx)))? } }`, attaching a
  **turn-time `UserTurnContext`** (`transcript.rs:14`) it computes inside the turn
  (`codebase_hash` / `codebase_dirty` / `dirty_files`, `turn.rs:684-689`).
- **Only the *reply* path already speaks `event::Body`.** The REPL's
  `InteractionOutcome { event_body: event::Body, respawn: bool }`
  (`ui/builder.rs:18-21`) is produced by `run_clarify`/`run_inform`/`run_permission`
  via `events::clarify_reply` / `inform_reply` / `permissions_reply`. The REPL
  appends it and **respawns** the turn (`chat.rs:286-291`:
  `wire::append(&tx_path, outcome.event_body)?; if outcome.respawn { continue 'spawn }`).
  That reply leg is already "append-then-fold".
- **`did_resume` is not a general resume flag.** It is specifically the
  *permissions* fast-path (`turn.rs:560`: last event is `PermissionsReply` →
  re-execute approved tools with no LLM round-trip). The Clarify/Inform reply path
  is *not* `did_resume`; it simply arrives with `task: None`, so the
  `if let Some(task)` guard is skipped and the turn folds from the log via
  `retrace::messages` (`turn.rs:692-693`).

So the input boundary today is **asymmetric**: replies are events appended before
a re-fold; the primary turn is a `String` the machinery turns into an event. Phase
4 is the work of making the *primary* path symmetric with the reply path.

## Current state (verified anchors)

**CLI** — `cli/run.rs:37` `cmd_run(argv, gait_dir, codebase_path)`:
single-shot. Builds `StdioSink::stdio(false)` (`:43`), announces a fresh id via
`sink.session_id(&id)` (`:49`), resolves profile/provider (`:95-122`), expands the
task (`:127-137`), spawns `run_turn` (`:150-153`), `drive(rx, &mut sink)` (`:157`),
prints the one `RunResult` via `sink.result(&turn_result)` (`:172`), runs
after-turn `Scheduler`/`Harness` coordination (`:174-188`). No interaction loop —
a `Clarify`/`Inform`/`Permissions` result just prints and exits.

**REPL** — `chat.rs:30` `run_chat(gait_dir, codebase, agent_name, session_id)`,
launched from the `"chat"` subcommand (`bin/gait.rs:756` `cmd_chat`). A
`'spawn` loop per turn: rebuilds the `Profile` **per turn** (`chat.rs:220-226` —
`Profile` is not `Clone`), `builder.begin_turn()` (`:258`), hand-drains the channel
`while let Some(item) = rx.recv().await { builder.handle_stream_item(item) }`
(`:261-266`) collecting `ViewSignal::TurnDone(blocks)` (`ui/state.rs:24-27`),
`builder.flush()` (`:267`), awaits the turn, then if `builder.has_interaction()`
(`:286`) calls `run_interaction` → `wire::append(event_body)` → `continue 'spawn`
on `respawn` (`:286-291`). The line dispatcher (`chat.rs:143-255`) keeps
session-control commands client-side (`/n` `/q` `/exit` `:143-153`; `/u*` undo
`:157-192`; `/skill` toggle `:117-140`) and treats any other line as task input.

**Output port (Phase 1, reused as-is)** — `port/output.rs`: `TurnSink { item,
result, session_id, diagnostic }` (`:29-34`); `drive(rx, &mut sink)` (`:36-38`);
`classify` / `StreamClass{Trace,Error}` (`:6-24`). CLI adapter `StdioSink`
(`cli/output.rs`); REPL adapter is the `ReplBuilder`/`ReplState`/`ReplRenderer`
view layer (`ui/`).

**`run_turn`** — `turn.rs:452` `run_turn(session, emitter, caps, input) ->
Result<TurnResult>`. `TurnInput { task: Option<String>, auto, max_iter,
preview_policy }` (`turn.rs:50-55`). `TurnResult { Done{message,result,changes},
Permissions, Clarify{items}, Inform{items} }` (`turn.rs:42-47`).

**Honest exceptions (unchanged, still documented)** — the transient unlogged
`<status>` prefix `messages.insert(0, …)` (`turn.rs:695-696`, recomputed each
non-resume turn) and the `Agent` drift write-back (`turn.rs:677-679`).

## Design

### Currency: `event::Body`
The port speaks the log's own currency (`event::Body`), matching the reply leg
that already does. `String`-shaped intent (`--task`, a typed REPL line) is mapped
to a `user_turn` body **by the adapter**, not by `run_turn`.

```rust
#[async_trait]
pub trait TurnSource {
    /// None = EOF (single-shot done / REPL quit / disconnect).
    async fn next(&mut self) -> Option<event::Body>;
}
```

### Adapters
- **`ArgSource`** (CLI): yields the expanded `--task` as one `user_turn` body, then
  `None`. No `--task` (`--auto` resume) → `None` immediately (the turn folds from
  the log). One-shot.
- **`ReplSource`**: wraps the `chat.rs` line reader + `ui/builder` replies. A typed
  task line → `user_turn` body; a `run_interaction` reply → its existing
  `event_body`; a session-control command (`/n` `/q` `/u` `/skill`) is handled
  client-side and is **not** turn input (it yields nothing to the loop, or ends it).
- **`RemoteSource`** (Phase 5): cloud commands → bodies.

### Unified run loop
```
run_loop(source, sink, run_ctx):
  sink.session_id(id)                              // fresh id only
  while let Some(body) = source.next().await:
     wire::append(body → log)                      // append-then-fold
     drive(spawn run_turn(task: None), sink)       // run_turn folds from the log
     sink.result(turn_result)
     after_turn(run_ctx)                           // Scheduler/Harness (CLI today)
     // Clarify/Inform/Permissions are surfaced by the sink; the SOURCE decides
     // whether a reply body follows (REPL) or the loop ends (CLI single-shot).
```
- CLI = `run_loop(ArgSource, StdioSink)`; REPL = `run_loop(ReplSource, <view sink>)`.
- Because the body is appended **before** the turn and the turn runs with
  `task: None`, `run_turn`'s internal `user_turn` append (`turn.rs:682-691`) is no
  longer the primary path. See the key decision below for how far to take that.

### Key decision — where `user_turn` (and its `UserTurnContext`) is built
Today `run_turn` owns both the append **and** the turn-time `UserTurnContext`
computation (`codebase_hash`/`dirty_files`, `turn.rs:684-689`). Moving the append
into the adapter/loop forces a choice:

- **Option A — Full unification (recommended; matches "full DI, not
  half-measures").** Extract the `UserTurnContext` computation into a small shared
  helper (it already overlaps with the `<status>`-prefix inputs at
  `turn.rs:694-695` and the Agent guard at `turn.rs:668-675`). The adapter/loop
  builds the complete `user_turn` body (with context) and appends it; **`run_turn`
  drops its internal `user_turn` append entirely** and always folds from the log.
  `TurnInput.task` is removed (or kept transiently only to thread the expanded
  string into the helper, then deleted). Cleanest symmetric boundary; touches the
  delicate resume/context region of `turn.rs`, so it is the real risk.
- **Option B — Minimal port.** Keep `run_turn`'s primary `user_turn` append; the
  port's `event::Body` currency covers only **replies** and future cloud commands,
  while `ArgSource`/`ReplSource` thread the *primary* turn as a string through an
  unchanged `TurnInput`. Smaller blast radius, but the "unified run loop" is shallow
  (the primary append still lives in `run_turn`) and the boundary stays asymmetric
  — i.e. a half-measure.

**Decision: Option A** (chosen 2026-06-14). It is the symmetric, full-DI boundary
the whole turn-I/O track is shaped for — Phase 5's `RemoteSource` needs input to be
event-shaped, so Option B would be a half-measure Phase 5 must later undo. Split to
de-risk the `turn.rs` surgery (below).

### Split (Option A)
- **4a — port + CLI.** Define `TurnSource` + `ArgSource`; extract the
  `UserTurnContext` helper; move the primary `user_turn` build/append out of
  `run_turn` into `run_loop`; put **`cmd_run` on `run_loop(ArgSource, StdioSink)`**.
  REPL untouched. Independently shippable + testable (the CLI behaves identically:
  same events appended, same stdout `RunResult`).
- **4b — REPL.** Introduce `ReplSource`; move `run_chat` onto `run_loop`,
  reconciling the per-turn `Profile` rebuild (`chat.rs:220`), `begin_turn`/`flush`,
  the `ViewSignal::TurnDone` block capture, and `has_interaction`/`run_interaction`/
  `respawn` into the loop + a REPL `TurnSink`. The riskier half; the REPL works
  today, so this is a refactor, not a fix.

## Out of scope
- `RemoteSource`/cloud (Phase 5).
- Teeing input events to the live channel so remote observers see input (Phase 5
  decision).
- The `<status>` prefix and `Agent` drift write-back remain documented honest
  exceptions (not "fixed" here).

## Tests
- `ArgSource`: `--task` → one `user_turn` body then `None`; no `--task` → `None`.
- `run_loop` over a **scripted `TurnSource`** + a channel sink: assert the body is
  appended to the log **before** the turn folds, and assert sink receipts
  (`session_id` once, one `result` per turn, ordering).
- **CLI parity (4a):** a `cmd_run` invocation produces the same transcript events
  (`user_turn` with the same `UserTurnContext`) and the same single stdout
  `RunResult` as before the refactor — the existing `run_output_split` /
  `integration_*` suites are the regression gate.
- **REPL parity (4b):** a scripted REPL session renders identically after moving
  onto `run_loop` (same blocks, same interaction/respawn behavior).

## Risk / open
- **Option A's `turn.rs` surgery** is the crux: removing the internal `user_turn`
  append must not change the persisted `UserTurnContext` nor the transient
  `<status>` prefix, and must not disturb the `did_resume` permissions fast-path
  (`turn.rs:560`) or the Clarify/Inform `task: None` fold.
- The REPL's per-turn `Profile` rebuild and block-capture (`ViewSignal::TurnDone`)
  must fold into the loop without changing rendering — reconcile during 4b.

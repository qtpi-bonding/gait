# Phase 4 spec — Input port

Parent: [`../plans/2026-06-13-turn-io-architecture.md`](../plans/2026-06-13-turn-io-architecture.md).
Prereq: Phases 1–3. *Forward-looking — expect substantial revision after Phases
1–3 and the REPL view-layer track land.*

## Goal

Give input the same boundary output got: a small `TurnSource` port whose adapters
turn native input (CLI arg, REPL line, cloud command) into `event::Body` appended
to the log. Collapse `cmd_run` and the REPL onto one run loop. This phase is
justified only once a **second** input driver (the REPL) actually exercises the
abstraction — until then `cmd_run` builds its `event::Body` inline.

## Design (provisional)

### Currency: `event::Body`, not a bespoke intent enum
Replies already produce `event::Body` (`ui/builder.rs:18` `InteractionOutcome`),
so the port speaks the log's own currency; only `ArgSource` does a real mapping
(`String → user_turn`). Avoids the circular `String → Input → to_event → Body`
round-trip an earlier draft proposed.

```rust
pub trait TurnSource {
    async fn next(&mut self) -> Option<event::Body>;   // None = EOF / single-shot done / disconnect
}
```

### Adapters
- `ArgSource` (CLI): `--task` → `user_turn` body once, then `None`. No `--task`
  (`--auto`) → `None` immediately (turn resumes from the log).
- `ReplSource`: the `chat.rs` line dispatcher + `ui/builder` replies → bodies.
  Session-control commands (`/n`, `/q`, `/u`) stay client-side, not turn input.
- `RemoteSource` (Phase 5): cloud commands → bodies.

### The two-source fold (already true; named here)
The machinery's prompt = `retrace(log)` after the new input is appended
(append-then-fold). The input port only gets the event *into* the log; `retrace`
(existing) does the fold. Honest exceptions to keep documented: the unlogged
`<status>` prompt prefix (`turn.rs:696`) and the `Agent` drift write-back
(`turn.rs:677-679`).

### Unified run loop
```
run_loop(source, sink, ctx):
  sink.session_id(id)
  while let Some(body) = source.next().await:
     append body → log
     drive(spawn_turn(), sink)
     sink.result(turn_result)
     match turn_result { Clarify|Inform|Permissions => continue; Done => after_turn(ctx) }
```
CLI = `run_loop(ArgSource, StdioSink)`; REPL = `run_loop(ReplSource, ReplView)`.

## Risk / open
- The REPL's per-turn `Profile` rebuild (`chat.rs:219`), `begin_turn`/`flush`, and
  `ViewSignal::TurnDone` block capture must be reconciled into the loop — these
  diverge from `cmd_run` today. The REPL works; this is a refactor, not a fix, and
  should follow the REPL view-layer track.

## Tests
- `ArgSource`: `--task` → one body then `None`; no `--task` → `None`.
- `run_loop` over a scripted source + channel: assert append order + sink receipts.
- REPL parity: a REPL session still renders identically after moving onto `run_loop`.

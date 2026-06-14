# Phase 3 spec — Workflow under the I/O boundary

Parent: [`../plans/2026-06-13-turn-io-architecture.md`](../plans/2026-06-13-turn-io-architecture.md).
Prereq: Phases 1–2. *Revisit after Phase 2 lands.* **Riskiest phase** — split into
3a/3b.

## Goal

Bring the third entrypoint (the workflow runner) under the same boundary: stop
echoing workflow events to stdout via `wire::emit`, route them through a channel
+ a sink, and give the sub-turn's events a live consumer instead of a dropped
receiver.

## Current state (verified)

- `run_workflow` is `async` (`runner.rs:28`). It emits 11 structured events via
  **synchronous `wire::emit`** (log **+ `println!` stdout**, no channel) inline
  through the node loop (`runner.rs:48,63,88,102,109,139,162,174,192,220,303`),
  plus `exec_gate.rs:15` and `gait.rs:914`. Events are opaque `wire::raw(...)`
  envelopes: `workflow_start`, `node_start/skip/complete/error`, `hook_run`,
  `workflow_complete/error`, `workflow_gate`.
- The prompt node builds a `Recorder` with a **dropped receiver**
  (`exec_prompt.rs:92`), so sub-turn deltas/events vanish and only the durable log
  keeps them.

## Design

### 3a — channel + consumer + classify for raw events
- Create the `StreamItem` channel + a `Recorder` once in `run_workflow`; spawn a
  consumer task (`tokio::spawn`) that `drive`s a sink concurrently with the node
  loop.
- Rewrite the 11 `wire::emit` sites to `rec.record(wire::raw(...))` (commit +
  publish). `wire::emit` (the stdout-`println!` path) is then deletable once
  `gait.rs:914`/`exec_gate.rs:15` are migrated too.
- Extend `classify` (Phase 1) to handle `payload::Kind::Raw` workflow events.
  Proposed: `workflow_error`/`node_error` → Error; everything else (`*_start`,
  `*_complete`, `node_skip`, `hook_run`, `workflow_gate`) → Trace. The workflow's
  machine result (if any) is the workflow's own return value, printed once like a
  turn's `TurnResult` (mirror Phase 1).
- Sink: reuse `StdioSink`, or a thin `WorkflowSink` if workflow stdout needs a
  different Result shape. Decide at impl time.

### 3b — sub-turn re-plumbing (the load-bearing change)
- `exec_prompt.rs:92` must pass the **real** sender into the sub-turn's
  `run_turn` so sub-turn events/deltas reach the workflow's channel (today the
  receiver is dropped). This is a change to how sub-turns are wired, not a sink
  swap — it is the actual risk in this phase.
- Decide sub-turn event routing: do workflow sub-turn deltas stream to the
  workflow's stderr live, or stay quiet? Likely Trace → stderr, matching `gait
  run`.

## Out of scope
- The workflow's *own* result schema/`changes` (mirror Phase 1 if wanted) — can
  be a follow-up.

## Tests
- A workflow run: assert workflow events arrive on the channel and route by class
  (no NDJSON on stdout); `node_error` → Error → non-zero exit.
- A workflow whose prompt node runs a sub-turn: assert sub-turn deltas/events
  reach the workflow channel (regression on the dropped-receiver fix).
- `classify` raw-event arms: table test over the workflow event types.

## Risk notes
`exec_prompt.rs:92`'s dropped receiver is the crux; everything else is mechanical
rewrites. Land 3a (channel + raw classify) before 3b (sub-turn wiring) so each is
independently reviewable and green.

---
status: done
---

# Phase 3 spec — Workflow under the I/O boundary

Parent: [`../plans/2026-06-13-turn-io-architecture.md`](../plans/2026-06-13-turn-io-architecture.md).
Prereq: Phases 1–2. **Revised after Phase 2 landed** (commit `f9e767e`); line numbers
below are current as of that point. **Riskiest phase** — split into 3a/3b.

## Goal

Bring the third entrypoint (the workflow runner) under the same boundary: stop
echoing workflow events to stdout via `wire::emit`, route them through a channel
+ a sink, and give the sub-turn's events a live consumer instead of a dropped
receiver.

## Current state (verified)

- `run_workflow` is `async` (`runner.rs:28`). It emits **11** structured events via
  **synchronous `wire::emit`** (verified at `wire.rs:79` = `write_to_transcript` +
  `println!` to stdout, no channel) inline through the node loop
  (`runner.rs:48,63,88,102,109,139,162,174,192,220,303`). Events are opaque
  `wire::raw(event_type, payload)` envelopes (→ `payload::Kind::Raw(RawEvent {
  r#type, payload })`, `wire.rs:27`): `workflow_start`,
  `node_start/skip/complete/error`, `hook_run`, `workflow_complete/error`. The
  12th in-loop emit is **`exec_gate.rs:15`** (`workflow_gate`), a **sync** helper
  `exec_gate_node(..., tx_path: &Path)` called from the loop.
- One more `wire::emit` lives **outside** `run_workflow`: **`gait.rs:735`**
  (`cmd_workflow_abort`, was `:914` pre-Phase-2) — a **standalone sync CLI command
  with no live channel/consumer**. It emits one `workflow_error` then commits. This
  one cannot take the channel `Recorder`; migrate it to **`wire::append`** (log-only)
  so it stops echoing NDJSON to stdout but still records the abort. (`transcript.rs:406`
  also calls `wire::emit`, but that is a `#[cfg(test)]` test — it must move off
  `wire::emit` before the symbol can be deleted.)
- The prompt node builds a `Recorder` with a **dropped receiver**
  (`exec_prompt.rs:92`: `let (tx, _rx) = …; Recorder::for_session(&session.gait_dir,
  &session.id, tx)`), so the sub-turn's deltas/events publish into a channel with no
  consumer and survive only in the sub-turn's own durable log (the sub-turn runs in
  its **own** `session.id`, distinct from the workflow's `tx_path`).

## Design

### 3a — channel + consumer + classify for raw events
- Create the `StreamItem` channel + a `Recorder` once in `run_workflow` (the
  `Recorder` keeps `tx_path` so `record` still writes the same transcript). Spawn a
  consumer task (`tokio::spawn`) that `drive`s a `StdioSink` concurrently with the
  node loop. After the loop: `drop(rec)` to close the channel, then `await` the
  consumer's `JoinHandle` to flush; recover `sink.failed` from it to drive the exit
  code (the consumer returns the sink or its `failed` bool).
- Rewrite the **11 in-loop** `wire::emit(&tx_path, …)` sites to `rec.record(…)`
  (commit + publish, same `event::Body`). **`exec_gate_node`** also moves over: change
  its signature from `tx_path: &Path` to `rec: &Recorder` (it stays a sync fn —
  `record` is sync) and update its one call site in the loop. The standalone
  **`gait.rs:735`** abort goes to `wire::append` (no channel there). Once those plus
  the `transcript.rs:406` test are migrated, **`wire::emit` is deleted** (it is the
  only `println!`-to-stdout writer left in the event path).
- Extend `classify` (`port/output.rs`, currently `Error`-kind → Error, all else →
  Trace) to inspect **`payload::Kind::Raw(r)`**: if `r.r#type == "workflow_error"`
  or `"node_error"` → **Error**; every other raw type (`*_start`, `*_complete`,
  `node_skip`, `hook_run`, `workflow_gate`) → **Trace**. This is the only
  `classify` change; no other consumer is affected (read-time).
- **Workflow result on stdout is out of scope** (see below): after 3a, the
  workflow's stdout is empty (all events route to stderr). A machine result printed
  once (mirroring Phase 1's `RunResult`) is a deliberate follow-up, not part of this
  phase. So a plain `StdioSink` suffices — no `WorkflowSink` needed.

### 3b — sub-turn re-plumbing (the load-bearing change)
- `exec_prompt.rs:92` builds its own `Recorder` with a dropped receiver. Replace it
  so the sub-turn **publishes to the workflow's channel** while still writing its
  **own** transcript: `Recorder::for_session(&session.gait_dir, &session.id,
  workflow_tx.clone())` — same sub-turn `tx_path`, but the workflow's sender. The
  `UnboundedSender` is `Clone`; the workflow's consumer then sees sub-turn
  events/deltas too. Thread `workflow_tx` (or the workflow `Recorder`'s sender) down
  through `exec_prompt`'s call path from `run_workflow`. This is cross-**channel**
  wiring (one consumer, many sessions' transcripts), not a transcript change — and
  it is the actual risk in this phase.
- Sub-turn deltas/events classify exactly as in `gait run` (Trace → stderr, Error →
  stderr), so once they reach the workflow channel the existing `StdioSink` routes
  them correctly with no extra work.

## Out of scope
- The workflow's *own* result schema/`changes` (mirror Phase 1 if wanted) — can
  be a follow-up.

## Tests
- `classify` raw-event arms (3a, pure/cheap): table test asserting
  `Raw{r#type:"workflow_error"}` and `Raw{r#type:"node_error"}` → Error; the other
  raw types (`workflow_start`, `node_start/skip/complete`, `hook_run`,
  `workflow_complete`, `workflow_gate`) → Trace.
- A workflow run (3a): drain the channel and assert all workflow events arrive on it
  and route by class; **stdout is empty** (no NDJSON `println!`); `node_error` →
  Error → non-zero exit.
- A workflow whose prompt node runs a sub-turn (3b): assert sub-turn deltas/events
  reach the **workflow's** channel (regression on the dropped-receiver fix), while
  the sub-turn's own transcript still records them.
- `cmd_workflow_abort` (3a): asserts the `workflow_error` lands in the log via
  `wire::append` and **nothing** is printed to stdout (only the human `[gait
  workflow] aborted …` line).
- Full suite is the gate; after migration, `rg 'wire::emit' src` returns **no
  production hits** (only — until removed — the one test, which 3a migrates).

## Risk notes
`exec_prompt.rs:92`'s dropped receiver is the crux; everything else is mechanical
rewrites. Land 3a (channel + raw classify) before 3b (sub-turn wiring) so each is
independently reviewable and green.

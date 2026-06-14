# Turn I/O ports: input + output around the event log

## Problem

`gait run` conflates everything on **stdout**. In `cmd_run` (`src/bin/gait.rs`):

- `:217` — `session_id` → `println!` (stdout)
- `:327` — full event JSONL echo → `println!` (stdout)
- `:330` — live `text_delta` token stream → `println!` (stdout)
- diagnostics/errors use `eprintln!("[gait] …")` (stderr)

The high-volume live stream sits on stdout next to the result, so you can't
separate "the live noise" from "the result" by redirection. And the final
`TurnResult` (`:337`) is **never printed** — it only exists implicitly as a
`turn_complete` event buried in the firehose.

That's the symptom. The deeper issue is that **gait has no I/O boundary**: how
input enters and how output leaves are scattered inline across `cmd_run`,
`chat.rs`, `ui/builder.rs`, `harness.rs`, and `spawn_agent.rs`, each doing it its
own way. There is no port a new driver (a cloud control agent) could implement.

This spec defines that boundary: **two ports around the event log** — an input
port and an output port — with CLI adapters now and REPL/cloud adapters to
follow.

Safe to change: nothing consumes `gait run`'s stdout programmatically (the only
subprocess spawns, `harness.rs:71` / `spawn_agent.rs:93`, use `Stdio::null()`;
the REPL uses the in-process channel). Events already persist to
`sessions/<id>/log.jsonl` independently — the stdout echo is a pure duplicate.

## Architecture: ports & adapters around the log

> Diagrams: [`2026-06-13-turn-io-ports.diagram.md`](./2026-06-13-turn-io-ports.diagram.md)
> — ports & adapters, the emit transaction, and the unified run loop.

The event log is the central **store** — the hub, not a stage in a pipe. It is
neither downstream of the `Recorder` nor a sink alongside stdout/stderr: the
`Recorder` *writes* it, the turn *reads* it, and the live sinks are projections
of a tee off that write. Flow is a loop around the store, not a line:

```
 external in ──▶ TurnSource ──▶ Input ──append──▶ ┌──────────────┐
  (argv, stdin,                                   │  LOG (store) │──read on spawn──▶ turn
   cloud cmd)    turn ──record──▶ Recorder ─commit─▶│              │                  │
                                  │                └──────────────┘                  │
                                  └─ publish Classified ─▶ TurnSink ─▶ stdout/stderr/cloud
                            (turn keeps recording until done) ───────────────────────▲
```

- **Input** enters by appending events (`user_turn`, `clarify_reply`,
  `inform_reply`, `permissions_reply`, `toggle`) straight to the store; the turn
  reads them on spawn.
- **Output**: the turn **records** each event → the `Recorder` runs one
  transaction with two halves in lockstep: **commit** (durable log append, the
  authoritative point) then **publish** (tee a live `Classified` copy to the
  channel). One call produces both, so the durable record and the live sinks
  never diverge. The commit-before-publish order is why the log is the
  authoritative store and not an async subscriber (a crash must not lose events
  the turn will later read back).
- **Sinks** (stdout/stderr/cloud) are projections of the published tee — siblings
  of the durable commit, both produced by `record`. The log is the *source* they
  derive from, never a peer sink.

The log is the core (the "domain"); `TurnSource` and `TurnSink` are the ports
(the boundary); CLI/REPL/cloud are adapter **pairs**. The ports are agnostic —
they assume no stdout, no TUI, no socket — so a cloud agent implements one
`TurnSource` + one `TurnSink` and works unchanged.

Note: today input events bypass the channel (appended log-only), so a remote
observer would not see them live. Teeing input to the channel for the cloud sink
is a Phase-3 decision; it does not affect Phase 1.

### Recording — the write transaction

`Emitter` is renamed **`Recorder`** (its own mechanical commit) to name what it
does: record events. The contract is the lockstep invariant.

```rust
impl Recorder {
    /// The transaction: commit + publish, ALWAYS both, in lockstep.
    /// Commit (durable log append) first, then publish a Classified copy.
    fn record(&self, body: event::Body) -> anyhow::Result<Event>;
    /// Publish only — ephemeral, explicitly NOT a record (never committed).
    fn publish_delta(&self, text: String);
}
```

- **`record` always does both.** There is no log-without-publish or
  publish-without-log through `record`.
- **Deltas are the named exception.** A delta is a live-only fragment, not
  history, so it goes through `publish_delta` (publish only). The invariant
  ("`record` does both") holds because deltas are not records.
- **`classify` runs once, at publish time.** `record` computes the class and
  sends a Rust-side `Classified { class, item }` on the channel; sinks route by
  the carried class without re-deriving. The class is **never** persisted — the
  durable log stores `payload::Kind` only, which is finer-grained than class and
  already searchable (`grep kind=error`), so class stays a pure read-time
  function and proto stays free of routing policy.
- **`wire::append` stays as a deliberate low-level commit-only escape hatch** for
  the niche input injections that append without a live channel (subagent task
  `spawn_agent.rs:78`, replies in `ui/builder.rs`). It is never hidden behind
  `Recorder`; the `Recorder` contract remains "always both."

```rust
/// Rust-side wrapper on the channel — proto stays dumb, class carried live.
pub struct Classified { pub class: StreamClass, pub item: StreamItem }
```

### Symmetry

| | Input port | Output port |
|---|---|---|
| pure mapping | `to_event(&Input) -> event::Body` | `classify(&StreamItem) -> Class` |
| agnostic trait | `TurnSource { next() -> Option<Input> }` | `TurnSink { item(); diagnostic() }` |
| substrate op | append event to log (`record`/`wire::append`) | read `Classified` from channel |
| CLI adapter | `ArgSource` | `StdioSink` |
| REPL adapter | `ReplSource` (follow-up) | REPL view builder (follow-up) |
| cloud adapter | `RemoteSource` (future) | `RemoteSink` (future) |

Neither port knows the other exists; they meet only at the log. Request/response
(clarify, permissions) is **not** wired through a port pairing: the turn surfaces
the request via the *output* port (a `turn_complete`/clarify result), ends, and
the client produces the reply via the *input* port on the next iteration. The
log + the spawn loop do the pairing.

## Output port

### Classes (pure classifier — proto stays dumb)

`classify(&StreamItem) -> StreamClass` labels each live item; it reads
`payload::Kind` but knows nothing about destinations.

| Class | Meaning | Examples |
|---|---|---|
| **Result** | the payload | `session_start`, `turn_complete` |
| **Trace** | progress / observability | tool calls, tool results, assistant text, `cwd`, `model_change`, **deltas** |
| **Error** | failures / diagnostics | `error` events, run-path `[gait] …` failures |

Deltas are Trace and **ephemeral** (never logged) — a producer-side fact
(`Recorder::publish_delta` is channel-only), not a separate class.

### Destinations (each sink subscribes to a class combo)

The `log` column is **not** a sink — it is the emitter's durable store write,
shown here only to make the routing matrix complete. stdout/stderr/cloud are the
sinks; they project the live tee by class.

| Class | stdout | stderr | log (store) | cloud | exit code |
|---|:--:|:--:|:--:|:--:|---|
| Result | ✓ | | ✓ | cfg | 0 |
| Trace  | | ✓ | ✓ (deltas excl.) | cfg | — |
| Error  | | ✓ | ✓ | cfg | ≠0 |

- **stdout = Result** — `gait run … > result.json` yields a clean payload.
- **stderr = Trace + Error** — Unix has no third output fd; progress and errors
  share stderr, interleaved in order (like `cargo`/`git`/`docker`). The Error
  class still earns its keep: drives the **non-zero exit code**, lets cloud flag
  failures, keeps stdout uncontaminated.
- **log = unchanged** — durable events only; stays in the `Recorder.commit` half
  so hash-chain/`seq` integrity is untouched.
- **cloud** = future sink subscribing to its own combo; zero upstream change.

### Contract + CLI adapter

```rust
pub enum Diag { Info, Error }

pub trait TurnSink {
    /// One published item off the channel, class already attached by `record`;
    /// the impl routes by `item.class` (no re-classify).
    fn item(&mut self, item: &Classified);
    /// Out-of-band Result payload not on the channel (e.g. the pre-turn
    /// session_id line). Routed as Result → stdout.
    fn result_line(&mut self, value: &serde_json::Value);
    /// Out-of-band diagnostic not on the channel (e.g. a run-path failure).
    fn diagnostic(&mut self, level: Diag, msg: &str);
}

pub async fn drive(mut rx: Receiver<Classified>, sink: &mut impl TurnSink) {
    while let Some(item) = rx.recv().await { sink.item(&item); }
}

pub struct StdioSink<O: Write, E: Write> { out: O, err: E, quiet: bool, failed: bool }
// Result → out; Trace → err (gated by quiet); Error → err + failed=true.
// Generic over Write so tests inject buffers; prod = StdioSink::stdio(quiet).
```

## Input port

### Intents (one variant per input-class log event)

```rust
pub enum Input {
    Task(String),                                // -> user_turn
    ClarifyReply(Vec<ClarifyReplyItem>),         // -> clarify_reply
    InformReply(Vec<InformReplyItem>),           // -> inform_reply
    PermissionsReply(Vec<PermissionsReplyItem>), // -> permissions_reply
    Toggle { kind: String, name: String, enabled: bool }, // -> toggle
}

/// Pure mapping — the input-side dual of `classify`.
pub fn to_event(input: &Input) -> event::Body { /* match each variant */ }
```

### Contract + adapters

```rust
pub trait TurnSource {
    /// Next input intent, or None when the input stream ends (EOF / single-shot
    /// done / disconnect). Async so stdin/socket adapters can await.
    async fn next(&mut self) -> Option<Input>;
}
```

- **`ArgSource`** (CLI): yields `Some(Task(--task))` once if given (else `None`
  immediately → turn resumes from the existing log, matching `--auto` today),
  then `None`. Single-shot.
- **`ReplSource`** (follow-up): the `chat.rs` line dispatcher + `ui/builder.rs`
  reply collection, refactored to yield `Input` intents — task lines, skill
  toggles, and the interactive clarify/permission replies. Session-control
  commands (`/n`, `/q`, `/u`) stay client-side, not turn input.
- **`RemoteSource`** (future): awaits cloud control messages → `Input`.

## The unified run loop

With both ports, `cmd_run` and the REPL collapse to the same shape — only the
adapter pair differs:

```rust
async fn run_loop(mut source: impl TurnSource, sink: &mut impl TurnSink, ctx: &RunCtx) {
    sink.result_line(&session_id_json);                 // Result → stdout
    while let Some(input) = source.next().await {
        append_event(&ctx.log_path, to_event(&input))?; // input → log
        let rx = spawn_turn(ctx);                       // turn reads the log
        drive(rx, sink).await;                          // output → sink
        match turn_result {
            Clarify | Inform | Permissions => continue, // source yields the reply next
            Done => { /* single-shot: source.next() → None */ }
        }
        after_turn_coordination(ctx, &turn_result);     // scheduler/harness
    }
}
```

CLI = `run_loop(ArgSource, &mut StdioSink, …)`; REPL = `run_loop(ReplSource,
&mut ReplView, …)`. Today these are two divergent ~150-line loops
(`cmd_run` and `chat.rs`); the ports converge them.

## Module structure

```
src/proto/
  emitter.rs   Emitter → Recorder: record (commit+publish), publish_delta  (rename)
src/port/
  mod.rs       pub mod input; pub mod output;
  input.rs     Input, to_event, trait TurnSource                  (agnostic)
  output.rs    StreamClass, Classified, classify, Diag, TurnSink, drive  (agnostic)
src/cli/
  mod.rs       pub mod run; pub mod output; pub mod input;
  output.rs    StdioSink: TurnSink                                (CLI adapter)
  input.rs     ArgSource: TurnSource + RunArgs::parse             (CLI adapter)
  run.rs       cmd_run → run_loop(ArgSource, StdioSink)           (orchestration)
```

`cmd_run` moves out of the 1918-line `bin/gait.rs` into `cli/run.rs`; the flag
loop (`:196–213`) becomes the testable `RunArgs::parse`. The other ~28 `cmd_*`
handlers stay put.

## Phasing

0. **Rename `Emitter → Recorder`** (its own mechanical commit, first): method
   `emit → record` (commit + publish), `delta → publish_delta`. Mechanical, wide,
   landed alone so the behavioral PR stays reviewable.
1. **Output port + run extraction** (this PR): `port/output.rs` (`Classified`,
   `classify`, `TurnSink`, `drive`), `cli/output.rs` (`StdioSink`), `cli/run.rs` +
   `RunArgs`, `ArgSource` as a thin `port/input.rs` + `cli/input.rs` so `run_loop`
   is symmetric from day one. `record` publishes `Classified`. Delete dead
   `wire::emit` (`wire.rs:79`, no non-test callers). Fixes the stdout/stderr bug.
2. **REPL adapters**: `ReplSource` + REPL `TurnSink`, collapsing `chat.rs` onto
   `run_loop`. The REPL works today, so this is a deliberate follow-up (tracked
   with the repl-view-layer plan), not a prerequisite.
3. **Cloud adapters**: `RemoteSource` + `RemoteSink` over a transport.

## Stays the same

- `Recorder` (née `Emitter`) commit+publish fan-out behavior, `log.jsonl` write
  path / hash-chain, durable (`record`) vs ephemeral (`publish_delta`) split —
  the rename changes names, not behavior.
- The REPL until Phase 2.
- The 128 `eprintln!` in other `gait.rs` subcommands (status, init, cost…) —
  command UX, not turn I/O; out of scope.

## Testing

- `classify` / `to_event` — table tests over every kind / variant.
- `Recorder` — `record` writes the log **and** sends a `Classified` with the
  right class (existing emitter tests, updated); `publish_delta` sends a channel
  item and writes **nothing** to the log.
- `StdioSink` over two `Vec<u8>` buffers: Result→out only; Trace→err, suppressed
  when `quiet`; Error→err + `failed`, even when `quiet`.
- `ArgSource` — `--task` yields one `Task` then `None`; no `--task` yields `None`.
- `drive` / `run_loop` — feed a scripted source + channel of `Classified`, assert
  log appends and sink receipts in order.
- `cmd_run` smoke: a run yields parseable JSON on stdout with no Trace mixed in
  (stderr redirected away); a turn error exits non-zero with clean stdout.

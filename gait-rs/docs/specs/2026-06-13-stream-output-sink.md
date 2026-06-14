# Stream output: classifier + uniform sinks

## Problem

`gait run` conflates everything on **stdout**. In `cmd_run` (`src/bin/gait.rs`):

- `:217` — `session_id` → `println!` (stdout)
- `:327` — full event JSONL echo → `println!` (stdout)
- `:330` — live `text_delta` token stream → `println!` (stdout)
- diagnostics/errors use `eprintln!("[gait] …")` (stderr)

So the high-volume live stream sits on stdout next to the actual result, and you
cannot separate "the live noise" from "the result" by redirection. There is also
no explicit result payload at all: the final `TurnResult` is computed at
`:337` and **never printed** — it only exists implicitly as a `turn_complete`
event buried in the stdout firehose.

Unix convention: **stdout = the machine-consumable result you'd pipe; stderr =
diagnostics/progress/errors a human watches.** By that rule the live stream and
errors belong on stderr, and only the result belongs on stdout.

This is safe to change: nothing consumes `gait run`'s stdout programmatically.
The only two subprocess spawns (`harness.rs:71`, `tools/spawn_agent.rs:93`) run
with `Stdio::null()`. The REPL consumes the in-process channel, not stdout. And
events are already persisted to `sessions/<id>/log.jsonl` by the `Emitter`
independently — the stdout echo is a pure duplicate.

## Model: one classifier labels, destinations subscribe

The producer side is already unified: every event flows through `Emitter::emit`
(`src/proto/emitter.rs`), which writes the durable `log.jsonl` **and** sends a
`StreamItem` on the channel. The log is the canonical union; stdout/stderr are
just **projections** of that same typed stream.

The routing decision is **not** a property of the data (proto must stay pure)
and **not** universal (only a consumer with two channels cares about the
stdout/stderr split). So:

1. A pure-Rust **classifier** labels each live item with an abstract class.
   It knows nothing about stdout/stderr/log/cloud.
2. Each **destination** is a uniform sink that subscribes to a combination of
   classes.

### Classes

| Class | Meaning | Examples |
|---|---|---|
| **1 Result** | the payload | `session_start`, `turn_complete` |
| **2 Trace** | progress / observability | tool calls, tool results, assistant text, `cwd`, `model_change`, **deltas** |
| **3 Error** | failures / diagnostics | `error` events, run-path `[gait] …` failures |

Deltas are Trace and are **ephemeral** (never logged) — that is a producer-side
fact (`Emitter::delta` is channel-only), not a separate class.

### Destinations (class subscriptions)

| Class | stdout | stderr | log | cloud | exit code |
|---|:--:|:--:|:--:|:--:|---|
| 1 Result | ✓ | | ✓ | cfg | 0 |
| 2 Trace  | | ✓ | ✓ (deltas excluded) | cfg | — |
| 3 Error  | | ✓ | ✓ | cfg | ≠0 |

- **stdout = Result only** — `gait run … > result.json` yields a clean payload.
- **stderr = Trace + Error** — Unix has no third output fd; progress and errors
  share stderr, interleaved in order (like `cargo`/`git`/`docker`). The Error
  class still earns its keep: it drives the **non-zero exit code**, lets the
  cloud sink flag failures, and keeps stdout uncontaminated.
- **log = unchanged** — durable events only, deltas never logged. Stays in the
  `Emitter` (producer side) so hash-chain/`seq` integrity is untouched.
- **cloud** = a future sink subscribing to its own configurable combination;
  registering it requires zero upstream changes.

## Layering

```
proto/            pure data — Event kinds, wire, log. No classes, no routing.
session/stream.rs classify(&StreamItem) -> StreamClass{Result,Trace,Error}. pure, no I/O.
turn_sink.rs      trait TurnSink (agnostic contract) + drive(rx, &mut sink) loop.
cli/output.rs     StdioSink: TurnSink — owns the class→stdout/stderr policy + exit code.
```

- `classify` reads `payload::Kind` (reading the data model is fine) but the
  decision lives in the **consumer**, never in proto.
- `TurnSink` is the agnostic contract — the consumer-side mirror of `Emitter`.
  A cloud agent implements the same trait with a network writer; the REPL's view
  builder implements it (follow-up). Each impl brings its own class policy.

### `TurnSink` trait

```rust
pub enum Diag { Info, Error }

pub trait TurnSink {
    /// One live item off the channel (event or delta). The impl classifies
    /// and routes per its own policy.
    fn item(&mut self, item: &StreamItem);
    /// Out-of-band Result payload not on the channel (e.g. the pre-turn
    /// session_id line). Routed as Result → stdout.
    fn result_line(&mut self, value: &serde_json::Value);
    /// Out-of-band diagnostic not on the channel (e.g. a run-path failure).
    fn diagnostic(&mut self, level: Diag, msg: &str);
}
```

### Shared driver

```rust
pub async fn drive(mut rx: Receiver<StreamItem>, sink: &mut impl TurnSink) {
    while let Some(item) = rx.recv().await {
        sink.item(&item);
    }
}
```

### `StdioSink` (CLI adapter — the stdout/stderr split lives here, in one place)

```rust
pub struct StdioSink<O: Write, E: Write> {
    out: O,         // stdout — Result
    err: E,         // stderr — Trace + Error
    quiet: bool,    // suppress Trace on stderr; Error still shown
    failed: bool,   // any Error seen → non-zero exit
}

impl<O: Write, E: Write> TurnSink for StdioSink<O, E> {
    fn item(&mut self, item) {
        match classify(item) {
            Result => writeln!(self.out, …),                    // stdout
            Trace  => if !self.quiet { writeln!(self.err, …) }, // stderr
            Error  => { self.failed = true; writeln!(self.err, …) },
        }
    }
    fn diagnostic(&mut self, level, msg) {
        if level == Diag::Error { self.failed = true; }
        if level == Diag::Error || !self.quiet { writeln!(self.err, "[gait] {msg}"); }
    }
}
```

`out`/`err` are generic over `Write` so tests inject buffers and assert routing.
Production constructs `StdioSink::stdio(quiet)` over `io::stdout()`/`io::stderr()`.

## Changes to `cmd_run`

- The inline `match item { println! … }` loop (`:324`–`:335`) collapses to
  `drive(rx, &mut sink).await`.
- The final `turn_result` is emitted as Result → stdout. **This is the first
  time the result is explicitly on stdout.** It arrives as a `turn_complete`
  event on the channel, so `classify` routes it through `item` with no new shape
  (projection stays a pure filter).
- `session_id` (`:217`) is printed *before* the emitter/channel exist (`:315`),
  so it cannot ride the `drive` loop. It is emitted as a Result directly through
  the sink via a small `sink.result_line(&value)` call before the turn spawns —
  same stdout destination, just out-of-band w.r.t. the channel.
- Run-path `eprintln!("[gait] error: …")` exits become `sink.diagnostic(Error, …)`
  + propagate the exit code from `sink.failed` / the error.
- Delete the dead `wire::emit` (`src/proto/wire.rs:79`) — no non-test callers; it
  is a parallel stdout path superseded by this design.

## Stays the same

- `Emitter` durable+channel fan-out and the `log.jsonl` write path / hash-chain.
- The producer durable (`emit`) vs ephemeral (`delta`) split.
- The REPL (`chat.rs`) — it already has its own channel consumer
  (`builder.handle_stream_item`); untouched here.
- The 128 `eprintln!` in other `gait.rs` subcommands (status, init, cost, …) —
  command UX, not turn output; out of scope.

## Out of scope / follow-ups

- **REPL adopts `TurnSink`** — port `builder` onto the trait so it drops into the
  same `drive` loop. It works today; tracked with the repl-view-layer plan.
- **Cloud sink** — a `RemoteSink: TurnSink` streaming a configurable class combo.
- **Broader `gait.rs` cmd_\* breakup** — splitting the ~30 handlers into a `cli/`
  module tree; this spec seeds `cli/output.rs` as the first occupant.
- **`--quiet` flag** — optional sugar; `2>/dev/null` already works once Trace is
  on stderr.

## Testing

- `classify` — table test: each `payload::Kind` + delta → expected class.
- `StdioSink` over two `Vec<u8>` buffers:
  - Result item → bytes on `out`, none on `err`.
  - Trace item → bytes on `err`, none on `out`; suppressed when `quiet`.
  - Error item/diagnostic → bytes on `err`, `failed` set, even when `quiet`.
- `drive` — feed a channel of mixed items, assert the sink received them in order.
- `cmd_run` smoke: a run with a result yields parseable JSON on stdout with no
  Trace lines mixed in (redirect stderr away).
```

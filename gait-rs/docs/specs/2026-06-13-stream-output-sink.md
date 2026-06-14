# Phase 1 spec — Output port for `gait run` (+ Recorder rename + result envelope)

Parent: [`../plans/2026-06-13-turn-io-architecture.md`](../plans/2026-06-13-turn-io-architecture.md).
Diagrams: [`2026-06-13-turn-io-ports.diagram.md`](./2026-06-13-turn-io-ports.diagram.md).

## Goal

Give `gait run` a single, owned stdout contract, fix the stdout/stderr
conflation, and land the result envelope in one slice so the contract is correct
on first print. Bundled here (per the architecture plan) because all of it
touches the same surface (`cmd_run` + the turn-complete path):

1. `Emitter → Recorder` rename (`emit→record`, `delta→publish_delta`).
2. `classify` (read-time) + `TurnSink` + `StdioSink`; route the channel by class.
3. The result envelope `{message, result, changes}` — extend `TurnResult::Done`
   / `TurnOutput`; fill `changes` from a new `CheckpointRepo::diff` (+ make the
   checkpoint shadow work for non-git codebases).
4. Extract `cmd_run → cli/run.rs` + testable `RunArgs::parse`.

**Decision resolved:** `turn_complete` is **Trace** (history on stderr). The
authoritative stdout payload is the returned `TurnResult`, printed once. So
**no channel event is ever Result** — `classify` only separates Trace from Error
among channel items; the Result payload is printed out-of-band by the run loop.
This dissolves the double-emit.

## The stdout contract (the point of the phase)

```
gait run --task "…"                 > result.json   # stdout = ONE TurnResult JSON
gait run --task "…" --result-schema schema.json     # .result conforms to schema, or non-zero exit
```

- **stdout** = exactly the serialized `TurnResult` (one JSON object), plus the
  `session_id` line for a fresh session. Nothing else.
- **stderr** = everything live: assistant-text deltas, tool calls/results,
  `turn_complete`, errors, `[gait] …` diagnostics.
- A freeform turn still yields a `TurnResult::Done` with `result: null` — stdout
  is always one parseable object.

`TurnResult` serializes as a tagged enum so paused turns compose too:
```json
{"type":"done","message":"…","result":{…},"changes":{…}}
{"type":"clarify","items":[…]}          // turn paused; caller responds next
{"type":"permissions","items":[…]}
{"type":"inform","items":[…]}
```

## 1. Recorder rename

`src/proto/emitter.rs`: `Emitter → Recorder`; `emit → record` (commit + publish),
`delta → publish_delta` (publish only). Pure rename, zero behavior change. ~26
call-sites, almost all in `turn.rs`; plus `chat.rs`, `bin/gait.rs`,
`workflow/exec_prompt.rs`, `proto/mod.rs` re-export. Done as the first commit of
the phase so the routing diff that follows is clean.

## 2. classify + TurnSink + StdioSink

```rust
// src/port/output.rs  (agnostic)
pub enum StreamClass { Trace, Error }                 // Result is never a channel item
pub fn classify(item: &StreamItem) -> StreamClass {   // pure, read-time
    match item {
        Delta(_)                  => Trace,
        Event(e) if is_error(e)   => Error,           // payload::Kind::Error
        Event(_)                  => Trace,           // tool_call/result, text, turn_complete, …
    }
}

pub enum Diag { Info, Error }
pub trait TurnSink {
    fn item(&mut self, item: &StreamItem);             // classify + route
    fn result(&mut self, r: &TurnResult);              // the stdout payload (once)
    fn session_id(&mut self, id: &str);                // stdout, pre-turn
    fn diagnostic(&mut self, level: Diag, msg: &str);  // run-path [gait] … messages
}

pub async fn drive(mut rx: Receiver<StreamItem>, sink: &mut impl TurnSink) {
    while let Some(item) = rx.recv().await { sink.item(&item); }
}
```

```rust
// src/cli/output.rs  (CLI adapter — the only place stdout/stderr is decided)
pub struct StdioSink<O: Write, E: Write> { out: O, err: E, quiet: bool, failed: bool }
// item:    Trace → err (gated by quiet); Error → err + failed=true
// result:  serialize TurnResult → out (one line)
// session_id: {"type":"session_id","id":…} → out
// diagnostic: Error always → err + failed=true; Info → err unless quiet
```

`out`/`err` are generic over `Write` so tests inject `Vec<u8>`. Production:
`StdioSink::stdio(quiet)` over `io::stdout()`/`io::stderr()`. `failed` (any Error
item or `diagnostic(Error)`) drives the process exit code.

Channel type **unchanged** (`StreamItem`); the REPL is untouched (it keeps
consuming the same channel via its own builder). `classify` is read-time, so no
consumer is forced to change.

## 3. The result envelope `{message, result, changes}`

### Types
- `TurnResult::Done { message, result: Option<Value>, changes: Option<Changes> }`
  (was 2-field, `turn.rs:43`).
- `TurnOutput` proto / `turn_complete` payload gains `changes` (was 2-field,
  `transcript.rs:30`). (Proto change — additive, optional field.)
- `pub struct Changes { added: Vec<String>, modified: Vec<String>, deleted: Vec<String> }`
  (paths relative to the codebase root).

### Where `changes` is computed
In `run_turn`, at the turn-complete path where the checkpoint is already taken
(`turn.rs:846,1201`): after `checkpoint_codebase` produces the new checkpoint
SHA, diff it against its parent commit and attach to `TurnResult::Done`. `result`
schema validation/retry is unchanged (`turn.rs:1167-1186`).

### `CheckpointRepo::diff` (new)
```rust
// src/git/checkpoint.rs — extend the trait
fn diff(&self, gait_dir: &Path, session_id: &str, sha: &str) -> anyhow::Result<Option<Changes>>;
// sha = the new checkpoint commit; baseline = its first parent (checkpoint.rs:112).
// git2 diff_tree_to_tree(parent.tree, commit.tree) → classify each delta:
//   Added → added, Modified/Renamed/Typechange → modified, Deleted → deleted.
// No shadow / no parent (turn-1 root with empty diff) → Ok(None) or empty Changes.
```

### Non-git shadow (so `changes` is universal)
Today `init` no-ops for non-git codebases and the shadow borrows the codebase's
objects via `alternates` (`checkpoint.rs:18-25,59`). Make the shadow
self-contained:
- Drop the non-git early-return in `init`; always create the bare shadow.
- Make the `alternates` borrow **conditional** on the codebase being a git repo
  (keep it as the blob-sharing optimization when available; skip it otherwise —
  the shadow then stores its own objects via `add_all`).
- Unborn HEAD already handled (`checkpoint.rs:70-71`): first checkpoint is the
  root commit.
Result: `checkpoint`/`diff` work for any directory; `changes` is `Some` whenever
a checkpoint exists, `None` only if checkpointing is genuinely unavailable.

## 4. `cmd_run → cli/run.rs` + `RunArgs`

```
src/cli/
  mod.rs       pub mod run; pub mod output;
  output.rs    StdioSink: TurnSink
  run.rs       cmd_run + RunArgs::parse
```
- `RunArgs::parse(argv) -> RunArgs` — lift the flag loop (`gait.rs:196-213`) into
  a pure, unit-tested struct/parser.
- `cmd_run` body: parse → resolve config/profile → expand task → build sink
  (`StdioSink::stdio`) → `sink.session_id(id)` → spawn turn → `drive(rx, &mut
  sink)` → `sink.result(&turn_result)` → after-turn coordination → exit code from
  `sink.failed`.
- Run-path `eprintln!("[gait] …")` exits become `sink.diagnostic(Error, …)`.

Suggested commit split: (a) pure `cli/run.rs` extraction (no behavior change),
(b) the routing/envelope behavior change. The other ~28 `cmd_*` handlers stay in
`bin/gait.rs`.

## Out of scope (later phases)
- Tools/hooks publishing live (`cwd`/`hook_run` stay log-only) — Phase 2.
- Workflow stdout (`wire::emit`) — Phase 3. `wire::emit` is **kept**.
- Input port / `TurnSource` — Phase 4.
- This phase **adds a read layer**; no write mechanism is removed yet.

## Tests
- `classify` — table over kinds + delta → Trace/Error.
- `CheckpointRepo::diff` — git fixture: edit/add/delete files across two
  checkpoints, assert `{added, modified, deleted}`; non-git dir → still produces
  a diff after the self-contained-shadow change; no-change turn → empty.
- `StdioSink` over two buffers: Trace→err only (suppressed when `quiet`);
  Error→err + `failed`; `result`→out (one line); `session_id`→out;
  `diagnostic(Error)`→err + `failed` even when quiet.
- `RunArgs::parse` — flags map to fields; absent `--task` → `None`.
- `cmd_run` smoke: a Done run yields one parseable `{type:"done",…,changes:…}` on
  stdout, no Trace mixed in (stderr redirected); `--result-schema` mismatch after
  retries exits non-zero with clean stdout.

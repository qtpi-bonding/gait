# Embedding gait

gait's turn engine is an embeddable library. The CLI (`gait run`) and the REPL
(`gait chat`) are just two consumers of the same in-process API — anything that
implements three small ports can drive a gait turn, with **zero** changes to the
turn, the log, or retrace. This is the I/O boundary the turn engine was shaped for
(the turn-I/O boundary work, Phases 1–5).

Everything you need is under one module: **`gait_ai::embed`**.

## The three ports

- **`TurnSource`** — *input*. `async fn next() -> Option<TurnStart>`. Yield
  `TurnStart::Input(body)` to append an input event (build a user turn with
  `embed::user_turn_body(&repos, codebase, task)`) then fold it, or
  `TurnStart::Resume` to run a turn that folds the existing log with no new input.
  `None` ends the loop.
- **`TurnSink`** — *output*. `item`/`result`/`session_id`/`diagnostic` (+ no-op
  `begin_turn`/`end_turn` lifecycle hooks). Implement it, or use the ready-made
  **`JsonSink`** (below).
- **`TurnDriver`** — *per-turn machinery*. `resolve_profile()` builds the `ResolvedProfile`
  for the turn (use `embed::resolve_profile(...)`), `turn_config()` returns
  `LoopConfig { auto, max_iter, preview_policy }`, `after_turn`/`on_turn_error` are
  policy hooks.

Then call `embed::run_loop(source, &mut sink, session, gait_dir, session_id, &mut driver)`.

## pbjson output for FFI-style consumers (`JsonSink`)

gait is protobuf end-to-end; every event serializes as **pbjson** (the canonical
protobuf→JSON mapping). `JsonSink` is a `TurnSink` that hands each frame to a
callback as a pbjson **string** — the same bytes the CLI writes:

```rust
let mut sink = embed::JsonSink::new(|frame| match frame {
    embed::JsonFrame::Item(json)        => send_to_client(&json),   // pbjson StreamItem
    embed::JsonFrame::Result(json)      => send_to_client(&json),   // pbjson RunResult
    embed::JsonFrame::Diagnostic(_, m)  => eprintln!("{m}"),
});
```

This is **FFI-shaped**: a thin C-ABI shim maps an `extern "C"` fn pointer + opaque
context to the closure, and a consumer (Serverpod / Flutter / any language) parses
the JSON with a model generated from the **same `.proto`** schema. No Rust types
cross the boundary — only strings. (Raw protobuf wire bytes are a trivial
alternative via `prost::Message::encode_to_vec` if binary throughput ever matters.)

## A complete embedder

Condensed from `tests/integration_embed.rs` (the running proof):

```rust
use gait_ai::embed::{self, run_loop, user_turn_body, JsonSink, JsonFrame,
    LoopConfig, TurnDriver, TurnSink, TurnSource, TurnStart};
use std::sync::Arc;

struct OneShot { start: Option<TurnStart> }
#[async_trait::async_trait(?Send)]
impl TurnSource for OneShot {
    async fn next(&mut self) -> Option<TurnStart> { self.start.take() }
}

struct Driver { caps: Option<embed::ResolvedProfile> }
#[async_trait::async_trait(?Send)]
impl TurnDriver for Driver {
    async fn resolve_profile(&mut self) -> anyhow::Result<embed::ResolvedProfile> {
        self.caps.take().ok_or_else(|| anyhow::anyhow!("profile taken"))
    }
    fn turn_config(&self) -> LoopConfig {
        LoopConfig { auto: true, max_iter: None, preview_policy: Default::default() }
    }
    fn on_turn_error(&mut self, _s: &mut dyn TurnSink, e: anyhow::Error)
        -> std::ops::ControlFlow<()> { eprintln!("{e}"); std::ops::ControlFlow::Break(()) }
}

# async fn run(gait_dir: &std::path::Path, codebase: &std::path::Path) -> anyhow::Result<()> {
let repos = embed::Repos::production();
let config = embed::load_config(gait_dir);
let profile = embed::resolve_profile(&config, gait_dir, codebase, "s00000001",
    None /* agent_path */, None /* model override */, &repos).await?;

let source = OneShot { start: Some(TurnStart::Input(
    user_turn_body(&repos, codebase, "summarise the README".into()))) };
let mut driver = Driver { caps: Some(profile) };
let mut sink = JsonSink::new(|f| if let JsonFrame::Result(j) = f { println!("{j}") });
let session = Arc::new(embed::Session {
    id: "s00000001".into(), gait_dir: gait_dir.into(), codebase_path: codebase.into(),
    assigned_by: None, repos: embed::Repos::production(),
});

run_loop(source, &mut sink, session, gait_dir, "s00000001", &mut driver).await
# }
```

## Beyond in-process

A **network transport** (websocket / HTTP / socket) and a **C-ABI / FFI shim** are
not built into gait — each is simply another `TurnSource` + `TurnSink` (+ a thin
runtime) over `embed::run_loop`, addable with **zero** change to the core. The
embedding API above is the stable surface they build on.

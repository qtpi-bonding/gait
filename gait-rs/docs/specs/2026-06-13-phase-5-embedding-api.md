# Phase 5 spec — Embedding API

Parent: [`../plans/2026-06-13-turn-io-architecture.md`](../plans/2026-06-13-turn-io-architecture.md).
Prereq: Phases 1–4 (**all landed**). **Revised** after the transport decision:
Phase 5 is **not** a network transport — gait is driven **in-process, as a Rust
library**, by implementing the I/O ports. (Originally outlined as cloud adapters;
reframed 2026-06-14 — "FFI / rust-calling-it lib style".)

## Goal

Make "drive gait by implementing two ports" a **real, public, ergonomic API**.
A consumer crate constructs a `TurnSource`, a `TurnSink`, and a `TurnDriver`, calls
`run_loop`, and runs gait turns — with **zero** changes to the turn, the log, or
the CLI/REPL/workflow adapters. This is the payoff the whole boundary was shaped
for: the CLI and REPL are now just two of many possible embedders.

## Reality check (what Phases 1–4 already give us)

The machinery is done; Phase 5 is **packaging**, not new mechanism:
- `run_loop(source: impl TurnSource, sink: &mut impl TurnSink, session: Arc<Session>,
  gait_dir, session_id, driver: &mut impl TurnDriver) -> Result<()>` already exists
  and is generic over all three ports (`cli/run_loop.rs:55`).
- `TurnSource { async fn next() -> Option<TurnStart> }` + `TurnStart{Input(event::Body)|Resume}`
  (`port/input.rs`); `TurnSink { item, result, session_id, diagnostic, begin_turn,
  end_turn }` (`port/output.rs:29`); `TurnDriver { caps, turn_config, after_turn,
  on_turn_error }` + `LoopConfig` (`cli/run_loop.rs`).
- `user_turn_body(repos, codebase, task) -> event::Body` builds the input event
  (`cli/run_loop.rs`); `classify`/`StreamClass`/`Diag` + `run_result_proto` are the
  read-time, reusable record (`port/output.rs`).
- The CLI (`CliDriver`, `ArgSource`, `StdioSink`) and REPL (`ReplSource`/`ReplSink`/
  `ReplDriver`) are existence proofs that the ports compose.

**The gaps are purely surface/ergonomics:**
1. The loop + driver live under **`cli::`** (`cli/run_loop.rs`) — an embedder must
   write `gait_ai::cli::run_loop::run_loop`, which wrongly reads as CLI-internal.
   The loop has **no** CLI dependency (verified), so it is misplaced.
2. There is **no curated embedding facade** — the surface is scattered across
   `port::input`, `port::output`, `cli::run_loop`, and crate-root re-exports.
3. Building `caps` (`ResolvedProfile`) requires hand-rolling
   `Profile::from_config_and_agent(...).resolve(...).await` (≈10 lines, as both
   `CliDriver::caps` and `ReplDriver::caps` do) — an embedder shouldn't have to.
4. No worked example proving an *external-style* consumer can drive a full turn
   through only the public surface.

## Design

### 1. Relocate the loop out of `cli`
Move `run_loop`, `TurnDriver`, `LoopConfig`, `user_turn_body` from
`src/cli/run_loop.rs` into a neutral home: **`src/embed.rs`** (`pub mod embed`).
Update the three internal call sites (`cli/run.rs`, `repl_loop.rs`) from
`crate::cli::run_loop::…` → `crate::embed::…`. No behavior change; the suite is the
gate.

### 2. The `gait_ai::embed` facade
`embed` is the one-stop embedding surface — it *owns* the loop and *re-exports*
the rest so a consumer needs exactly one `use`:
```rust
pub mod embed {
    pub use crate::port::input::{TurnSource, TurnStart};
    pub use crate::port::output::{TurnSink, classify, StreamClass, Diag};
    pub use crate::turn::{TurnInput, TurnResult};
    pub use crate::profile::ResolvedProfile;
    pub use crate::session::Session;
    pub use crate::Recorder;
    pub use crate::proto::gait::log::v1::{StreamItem, RunResult};
    // owned here (moved from cli):
    // run_loop, TurnDriver, LoopConfig, user_turn_body, resolve_caps
}
```
The CLI/REPL keep using the same items via `embed` (or directly) — no duplication.

### 3. A caps helper
Add `embed::resolve_caps` so an embedder gets a `ResolvedProfile` without touching
`Profile` internals:
```rust
/// Resolve a live profile (provider + tools + MCP) for one session, from config +
/// optional agent + optional model override. The same resolution the CLI/REPL do.
pub async fn resolve_caps(
    gait_dir: &Path, codebase: &Path, session_id: &str,
    agent: Option<&str>, model_override: Option<String>, repos: &Repos,
) -> anyhow::Result<ResolvedProfile>;
```
`CliDriver::caps`/`ReplDriver::caps` are refactored to call it (dedupe), proving
it's the real path, not a parallel one.

### 4. Worked example (the proof)
`examples/embed_turn.rs` (or `tests/integration_embed.rs`): a minimal embedder that
implements a `TurnSource` yielding one `Input` body, a `TurnSink` that collects
items + the result, and a `TurnDriver` returning `resolve_caps(...)`, then calls
`embed::run_loop` and asserts the turn ran (transcript + a `RunResult`). This is
the "external consumer" proof — it uses only `gait_ai::embed::*`.

## Out of scope (explicit)
- **FFI / C ABI.** A `cdylib` exposing this over a C boundary is a real follow-up,
  but it is a *separate layer* (owns a tokio runtime, marshals the trait callbacks
  through `extern "C"` fn pointers + opaque context, and translates `anyhow::Error`
  to codes). Phase 5 makes the **Rust** embedding API clean and stable; the FFI shim
  then wraps it without touching the core. Documented as the next step, not built
  here. *(Flag for review: if FFI is wanted now, it becomes the bulk of the phase.)*
- **Network transport** (websocket/HTTP/socket) — the original outline; a transport
  is just another `TurnSource`+`TurnSink` pair over `embed::run_loop`, addable later
  with zero core change. Not built here.
- **Tee input to the live channel** — decided **no**: input is already durably
  logged and the embedder constructed it / can read the log, so re-emitting it to
  the channel buys nothing without a separate passive observer (a network concern).

## Tests
- The relocation keeps the full suite green (existing CLI/REPL/workflow tests are
  the regression gate — they now reach the loop via `embed`).
- `resolve_caps` dedupe: CLI + REPL still resolve identically (suite green).
- `embed_turn` example/test: an external-style consumer drives one full turn via
  `gait_ai::embed::*` only — asserts the transcript has the events and a `RunResult`
  is delivered to the sink.

## Why it's cheap
Nothing in the turn/log/retrace changes. Phase 5 is a **move + re-export + one
helper + one example** — the proof that Phases 1–4 actually produced an embeddable
core, and the stable surface future embedders (incl. an FFI shim) build on.

---
status: done
---

# Phase 2 spec — Tools & hooks publish

Parent: [`../plans/2026-06-13-turn-io-architecture.md`](../plans/2026-06-13-turn-io-architecture.md).
Prereq: Phase 1 (the `Recorder` + `classify`/`StdioSink`). **Revised after Phase 1 landed** (commit range `b477ab0..addbe54`); line numbers below are current as of that point.

## Goal

Make the "one write path" invariant real: tool- and hook-originated events go
through the `Recorder` (commit + publish) instead of bypassing the live channel
via `wire::append` (log-only). After this, `cwd` and `hook_run` appear live on
stderr through the Phase-1 `StdioSink`, and in the REPL view, instead of only
landing in the log file.

### What Phase 1 already gives us (no Phase-2 work needed there)
- `classify(&StreamItem)` maps every non-`Error` payload to **Trace**; `cwd` and
  `hook_run` are ordinary payloads, so once published they classify as Trace and
  `StdioSink` already writes them to stderr (`cmd_run` uses `StdioSink::stdio(false)`).
- `Recorder::record(body)` already does commit + publish in lockstep. Phase 2 is
  purely "call `record` where we currently call `append`," plus threading the handle.

So Phase 2 changes **no** sink/classify code — only the `Tool`/`Hook` signatures
and the two bypass sites.

## Current bypass (verified post-Phase-1)

`Tool::execute(&self, args)` has no recorder (`types.rs:197`). Tools/hooks that
record events reconstruct the log path from `ToolCtx { codebase_path, gait_dir,
session_id }` (`tools/ctx.rs`) and call `wire::append`:
- `shell` → `cwd` (`tools/shell.rs:103-104`, log path rebuilt from `ctx`; the
  pre-read `active_cwd` retrace is at `shell.rs:90`).
- `ConfigHook` → `hook_run` (`config_hook.rs:58-61`, via a baked `tx_path` field).
The turn holds the `Recorder` (named `emitter`) at the tool call site
(`turn.rs:200`) and at all hook call sites — only the trait signatures block it.

## Design

Thread the `Recorder` into the call (per-call, not construction — the recorder
exists only once the turn starts, after tools are built in `profile.rs`).

### `Tool::execute` gains `&Recorder`
```rust
// src/types.rs — Tool is dyn-dispatched (Box/Arc<dyn Tool>), so it keeps #[async_trait]
#[async_trait]
pub trait Tool: Send + Sync {
    async fn execute(&self, args: Value, rec: &Recorder) -> anyhow::Result<Value>;
    // … unchanged: name/description/parameters/permission/execution_signal/preview
}
```
- `Recorder` is `Send + Sync` (it holds `UnboundedSender<StreamItem>` + `PathBuf`,
  `emitter.rs:5`), so `&Recorder` threads through the async call and across the
  single await at `turn.rs:200` cleanly — no `Clone`/`'static` needed.
- Call site `turn.rs:200`: `t.execute(tc.args.clone(), emitter).await`.
- **17 `impl Tool` blocks** change signature: 16 concrete tools + the `ToolDef`
  blanket impl (`types.rs:244`). Counting test/fake tools, there are **~29**
  `async fn execute` bodies in `src` — most ignore the new param (`_rec`).
  Mechanical, compiler-driven.

### `Hook::post_tool` gains `&Recorder`
```rust
// src/types.rs:175 — Hook also keeps #[async_trait]
async fn post_tool(&self, name: &str, call_id: &str, args: &Value, result: &str,
                   rec: &Recorder) -> Option<String>;
```
- Only `post_tool` emits an event today (`hook_run`); `pre_tool` (sync, `:183`),
  `pre_step`/`post_step`/`post_turn` emit nothing — leave them unchanged.
- **3 call sites** in `turn.rs` (`:1044`, `:1141`, `:1246`) — all inside the turn
  with `emitter` in scope: `h.post_tool(&tc.tool_name, &tc.tool_call_id, &tc.args, &result_str, emitter).await`.

### Replace the bypasses
- `shell` (`tools/shell.rs:103-104`): drop the `ctx.gait_dir/session_id` → log-path
  reconstruction; emit `rec.record(events::cwd(&dir.to_string_lossy()))?`. `ToolCtx`
  stays (still used by `active_cwd` at `shell.rs:90`); only the append is replaced.
- `ConfigHook` (`config_hook.rs`): `post_tool` does
  `rec.record(events::hook_run("post_tool", &self.def.run, exit_code, &output, Some(call_id.into())))`
  instead of `wire::append(&self.tx_path, …)`. The baked `tx_path` field is then
  dead — **drop it** from the struct and from `ConfigHook::new` (`config_hook.rs:15`),
  and update the single production constructor at `profile.rs:193`.

### Test `Recorder`
`config_hook.rs` has **8 unit tests** that build `ConfigHook::new(..., tx, …)` and
call `post_tool(name, id, args, result)`. They must drop the `tx` arg and pass a
`&Recorder`. Add a `#[cfg(test)] Recorder::discard()` constructor (parallels
Phase 1's `wire::body_to_event_for_test`) that wires an `UnboundedSender` whose
receiver is dropped:
```rust
// src/proto/emitter.rs
#[cfg(test)]
impl Recorder {
    pub fn discard(gait_dir: &Path, session_id: &str) -> Self {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        Self::for_session(gait_dir, session_id, tx)
    }
}
```
Concrete-tool unit tests that call `execute` similarly pass `&Recorder::discard(...)`.

### Open questions to resolve at impl time
- **REPL rendering.** Once `cwd`/`hook_run` publish, the REPL channel consumer
  (`ui/`) sees them too. Confirm it renders or ignores them sanely (it already
  receives every other event kind; likely a no-op, but verify there's no panic or
  ugly line).
- **`ToolDef` (MCP).** The blanket impl returns JSON and emits nothing — it takes
  `_rec`. Confirm against the MCP closure type when implementing.

## Out of scope
- Workflow `hook_run`/events via `wire::emit` (`workflow/runner.rs`) — Phase 3.
  `wire::emit` is kept.
- Cross-session `wire::append` (harness, `spawn_agent` seeding *another* session's
  log) stays `append` — there is no live channel for that other session.

## Tests
- **`shell` cwd-persist** (existing test, updated): with a `Recorder::discard`,
  assert a `cwd` event is recorded; add a variant with a real channel
  (`for_session` + kept `rx`) asserting the `cwd` `StreamItem` arrives on the
  channel — i.e. `StdioSink` would surface it on stderr — not only in the log.
- **`ConfigHook::post_tool`** (8 tests, updated): drop `tx`, pass `&Recorder`;
  one test keeps the `rx` and asserts a `hook_run` `StreamItem` is published.
- **Integration turn that changes cwd**: assert the `cwd` event reaches the channel
  (publish), not only the log file.
- **All ~29 `execute` impls + 3 `post_tool` call sites** updated for the new
  signatures (mechanical; the full suite — 644 tests as of Phase 1 — is the gate).

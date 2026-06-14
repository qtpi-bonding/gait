# Phase 2 spec — Tools & hooks publish

Parent: [`../plans/2026-06-13-turn-io-architecture.md`](../plans/2026-06-13-turn-io-architecture.md).
Prereq: Phase 1 (the `Recorder`). *Revisit this spec after Phase 1 lands.*

## Goal

Make the "one write path" invariant real: tool- and hook-originated events go
through the `Recorder` (commit + publish) instead of bypassing the live channel
via `wire::append`. After this, `cwd` and `hook_run` appear live on stderr (Trace)
through the Phase-1 `StdioSink`, and in the REPL view, instead of only landing in
the log file.

## Current bypass (verified)

`Tool::execute(&self, args)` has no recorder (`types.rs:197`). Tools that record
events reconstruct the log path from `ToolCtx { codebase_path, gait_dir,
session_id }` (`tools/ctx.rs`) and call `wire::append` (log-only):
- `shell` → `cwd` (`shell.rs:98-104`)
- `config_hook` → `hook_run` (`config_hook.rs:58`), via a baked `tx_path`.
The turn already holds the `Recorder` at the call site (`turn.rs:200`), so the
handle is available — only the trait signature blocks it.

## Design

Thread the `Recorder` into the call (per-call, not construction — the recorder
exists only once the turn starts, after tools are built in `profile.rs`).

```rust
// src/types.rs
#[async_trait]
pub trait Tool: Send + Sync {
    async fn execute(&self, args: Value, rec: &Recorder) -> anyhow::Result<Value>;
    // … unchanged: name/description/parameters/permission/execution_signal
}
```

- `Recorder` holds `UnboundedSender<StreamItem>` + `PathBuf` — `Send + Sync`,
  `&Recorder` threads through the async call cleanly (verified: no Clone/'static
  needed; the borrow lives across the single await at `turn.rs:200`).
- Call site `turn.rs:200`: `t.execute(tc.args.clone(), emitter).await`.
- **17 trait impls** edit the signature (16 concrete tools + the `ToolDef` blanket
  impl, `types.rs:248`; ~23 `execute` fns incl. tests). Most ignore the new param
  (`_rec`). Mechanical.

### Replace the bypasses
- `shell`: drop the `gait_dir/session_id`→log-path reconstruction
  (`shell.rs:98-99`); `rec.record(events::cwd(...))`. `ToolCtx` stays (still used
  for `active_cwd` retrace at `shell.rs:88`); only the append is replaced.
- `config_hook`: `Hook` trait methods (`types.rs:174`) gain `&Recorder` the same
  way; `post_tool` does `rec.record(events::hook_run(...))` instead of
  `wire::append(&self.tx_path, …)`. The baked `tx_path` can then be dropped.

### Open question to resolve at impl time
The `Hook` trait surface and whether `ToolDef` (MCP) needs the recorder (it
returns JSON, emits nothing today) — likely takes `_rec`. Confirm against the MCP
closure type when implementing.

## Out of scope
- Workflow `hook_run` via `wire::emit` (`runner.rs:63,102,174`) — that's Phase 3.
- Cross-session `wire::append` (harness, spawn_agent seeding *another* session's
  log) stays `append` — there is no live channel for that other session.

## Tests
- A fake `Recorder` capturing records: `shell` cwd-persist test asserts a `cwd`
  event is recorded (published to channel + committed), not just appended.
- `config_hook` post_tool asserts `hook_run` is recorded.
- An integration turn that changes cwd: assert the `cwd` event arrives on the
  channel (so `StdioSink` would show it on stderr), not only in the log file.
- Existing tool tests updated for the new `execute` signature (mechanical).

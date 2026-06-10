# WASM Plugin System — Idea

**Status:** Not started — idea stage only. Config stub (`WasmPluginDef`) and `LoadableKind::WasmPlugin` variant exist as scaffolding; no runtime, executor, or lifecycle hooks are wired in.

**Goal:** Let users extend gait with native tools written in any language that compiles to WASM — without gait owning or trusting the plugin code.

---

## The Pitch

MCP covers connecting to *existing* tool servers. WASM covers *writing new tools* that ship as a single `.wasm` file, run sandboxed inside gait, and are as fast as a function call. No subprocess, no port, no network.

The security story is the main win: WASM runs in a capability-based sandbox with no filesystem, no network, no syscalls unless explicitly granted by the host. A plugin can't escape, can't read your keys, can't phone home.

---

## Crate: Extism

[Extism](https://extism.org/) is the right Rust host SDK. It wraps Wasmtime with a plugin-friendly ABI designed for exactly this use case:

```toml
extism = "1"
```

---

## Unified `Loadable` Trait

MCP servers, WASM plugins, and skills all follow the same load/unload mechanic. Rather than separate code paths, they should all implement a common trait:

```rust
pub trait Loadable: Send + Sync {
    fn name(&self) -> &str;
    fn kind(&self) -> LoadableKind;   // Mcp | WasmPlugin | Skill
    fn tools(&self) -> Vec<Box<dyn Tool>>;
    fn hooks(&self) -> Vec<Box<dyn Hook>>;
}
```

The existing `mcp(name, context=true/false)` toggle tool works on anything `Loadable`. The index lists all three kinds together. No new toggle surface — just a new kind in the registry.

---

## Tool Interface

A plugin implements required exports (guest side, any language that compiles to WASM):

```rust
fn name() -> String
fn description() -> String
fn schema() -> String      // JSON Schema for the args
fn execute(args: String) -> String  // JSON in, JSON out
```

The host loads it as `Box<dyn Tool>`:

```rust
let plugin = extism::Plugin::new(&wasm_bytes, [], true)?;
// call name(), description(), schema() once at load time
// call execute(args_json) at tool-call time
```

---

## Lifecycle Hooks

Beyond registering tools, plugins can export optional hook functions. gait calls them at key points in the agent loop. All hooks take JSON strings and return JSON strings. Missing exports are silently skipped — all hooks are optional.

Multiple plugins' hooks run in config order. **First block wins**: if a plugin blocks a `hook_tool_call`, later plugins don't run for that call.

### `hook_before_turn(context: String) -> String`

Called once per turn before the LLM call. Input:

```json
{ "system_prompt": "You are...", "cwd": "/path/to/project" }
```

Return to modify the system prompt for this turn:

```json
{ "system_prompt": "You are... [plugin appended text]" }
```

Return `{}` to pass through unchanged.

**Use cases:** inject project-specific context, append skill instructions, add per-turn state.

### `hook_tool_call(event: String) -> String`

Called before any tool executes. Input:

```json
{ "tool_call_id": "tc_abc", "tool_name": "shell", "args": { "cmd": "rm -rf /tmp/foo" } }
```

Return to block or mutate:

```json
{ "block": true, "reason": "blocked by policy plugin" }
// or
{ "args": { "cmd": "rm -rf /tmp/foo_safe" } }
// or
{}  // pass through unchanged
```

**Use cases:** policy enforcement, argument sanitisation, audit logging.

### `hook_tool_result(event: String) -> String`

Called after a tool finishes, before the result is returned to the LLM. Input:

```json
{ "tool_call_id": "tc_abc", "tool_name": "read", "result": "...file contents...", "is_error": false }
```

Return to replace the result:

```json
{ "result": "...redacted...", "is_error": false }
// or
{}  // pass through unchanged
```

**Use cases:** redact secrets in file reads, truncate large results, inject metadata.

### `hook_message_end(message: String) -> String`

Called after the LLM finishes generating an assistant message, before it is stored in the transcript. Input:

```json
{ "role": "assistant", "content": "Here is my answer..." }
```

Return to replace the message:

```json
{ "content": "Here is my answer... [post-processed]" }
// or
{}  // pass through unchanged
```

**Use cases:** content filtering, appending structured tags, telemetry/logging, output format transformation.

### `hook_outgoing_context(messages: String) -> String`

Called before the message payload is sent to the LLM this turn. This is the **outgoing payload only** — modifying it does not touch the stored transcript. Input is the message array about to be sent:

```json
[
  { "role": "user", "content": "..." },
  { "role": "assistant", "content": "..." }
]
```

Return a replacement array, or `null`/`{}` to pass through unchanged.

**Use cases:** inject retrieved documents, trim stale tool results for this call, add a per-turn "remember X" note. Safer than full history rewrite — a bug affects one LLM call, not the transcript.

---

## Host Callbacks

Plugins do not get host callbacks into gait internals. If a plugin needs to read a file or run a command during execution, it shells out to the `gait` CLI (e.g. `gait read path/to/file`) using its granted exec capability. This keeps the host/guest boundary clean and avoids re-entrant execution.

---

## Config shape

Follows the same pattern as `mcp_servers` — explicit list in `gait.jsonc`, no auto-scan:

```jsonc
// gait.jsonc
{
  "wasm_plugins": [
    { "name": "my-linter", "path": "plugins/linter.wasm" },
    { "name": "formatter",  "path": "plugins/fmt.wasm",
      "allow_fs": ["/tmp"] }   // optional capability grants
  ]
}
```

**Capability grants** default to zero — no filesystem, no network, no exec. Grants are explicit per-plugin. When a plugin is installed (added to config), gait should prompt for which capabilities to allow.

---

## Architecture fit

- **Unified with MCP/skills** — all three implement `Loadable`; same toggle machinery, same index.
- **Config-only discovery** — explicit list like `mcp_servers`; no auto-scan of directories.
- **Run-and-done friendly** — plugins load at startup, live for the session, drop at exit. No server lifecycle.
- **Distribution** — a plugin is a single `.wasm` file. Check it in or download it. No npm, no cargo, no runtime deps.
- **Hook ordering** — plugins run in config order; first block wins on `hook_tool_call`.

---

## Mapping from pi's extension system

| pi (TypeScript) | gait WASM equivalent |
|---|---|
| `registerTool(def)` | guest exports `name/description/schema/execute` |
| `on("before_agent_start")` → replace system prompt | `hook_before_turn` |
| `on("tool_call")` → block or mutate args | `hook_tool_call` |
| `on("tool_result")` → replace result | `hook_tool_result` |
| `on("message_end")` → replace assistant message | `hook_message_end` |
| `on("context")` → replace message history | `hook_outgoing_context` (per-turn payload only) |
| `ctx.reload()` | n/a — plugins load once per session |
| host function callbacks | shell out to `gait` CLI instead |
| UI hooks, TUI renderers | not applicable (headless agent) |

---

## Open questions before planning

1. Capability grants UX — prompt at `gait config add-plugin` time, or list in a separate `gait plugin install` command?
2. Should `hook_outgoing_context` be v1 or v2? Lower risk than full history rewrite but still broad.
3. Plugin versioning — should the `.wasm` file embed a version, and should gait enforce a minimum API version?
4. Should plugins be able to call back into gait (e.g. read a file via gait's own tools)? Probably not in v1.

---

## When to plan

When there's a concrete use case — a tool someone actually wants to write as a plugin that doesn't fit as a shell command or MCP server.

---

## Implementation Status (verified 2026-06-10)

**Verdict: Not started. Idea-stage scaffolding only.**

### What exists

| Artifact | Location | Notes |
|---|---|---|
| `WasmPluginDef` config struct | `gait-rs/src/config.rs:55` | Parses `name`, `path`, `enabled`, `allow_exec`, `allow_fs`. Marked `/// Stub for future WASM plugin support.` |
| `GaitConfig::wasm_plugins` field | `gait-rs/src/config.rs:82` | Defaults to empty `vec![]`. Parsed from config but never consumed. |
| `LoadableKind::WasmPlugin` variant | `gait-rs/src/types.rs:244` | Present in the `LoadableKind` enum alongside `Mcp` and `Skill`; maps to the string `"plugin"`. |
| Config parse test | `gait-rs/src/config.rs:354` | Verifies `WasmPluginDef` deserializes correctly. |

### What does NOT exist

- No WASM runtime or executor (`extism`, `wasmtime`, `wasmer` — none in `Cargo.toml`).
- No code that reads `config.wasm_plugins` at startup or loads `.wasm` files.
- No `Loadable` trait.
- No lifecycle hooks (`hook_before_turn`, `hook_tool_call`, `hook_tool_result`).
- No `LoadableEntry` instances of kind `WasmPlugin` are ever constructed (only `Mcp` and `Skill` are used in `turn.rs`).

### Summary

The config surface (struct + field + `LoadableKind` variant) was scaffolded in anticipation of this idea, but nothing beyond deserialization exists. The plan remains at idea stage with no implementation work started.

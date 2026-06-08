# WASM Plugin System — Idea

**Status:** Idea only. Not planned for implementation yet.

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

A plugin implements three exports:

```rust
// guest side (any language that compiles to WASM)
fn name() -> String
fn description() -> String
fn schema() -> String      // JSON Schema for the args
fn execute(args: String) -> String  // JSON in, JSON out
```

The host (gait) loads it like any other `Box<dyn Tool>`:

```rust
let plugin = extism::Plugin::new(&wasm_bytes, [], true)?;
// call name(), description(), schema() once at load time
// call execute(args_json) at tool-call time
```

---

## Config shape

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

---

## Architecture fit

- **Separate from MCP** — MCP = connect to existing servers. WASM = native gait extensions.
- **Same toggle mechanic** — WASM tools participate in the same `mcp(name, context=true/false)` load/unload system and appear in the `<mcp-index>` (or a separate `<plugin-index>`).
- **Run-and-done friendly** — plugins load at startup, live for the session, drop at exit. No server lifecycle to manage.
- **Distribution** — a plugin is a single `.wasm` file. Users check it in or download it. No npm, no cargo, no runtime deps.

---

## Open questions before planning

1. Should WASM plugins share the same toggle tool as MCP (`mcp(name, context=true)`) or get their own (`plugin(name, context=true)`)? Probably separate to keep the index clean.
2. Capability grants — filesystem paths? Network hosts? Or keep it zero-capability by default and document that limitation?
3. Plugin discovery — explicit list in config only, or also scan a `plugins/` directory automatically?
4. Should plugins be able to call back into gait (e.g. read a file via gait's own tools)? Probably not in v1.

---

## When to plan

When there's a concrete use case — a tool someone actually wants to write as a plugin that doesn't fit as a shell command or MCP server.

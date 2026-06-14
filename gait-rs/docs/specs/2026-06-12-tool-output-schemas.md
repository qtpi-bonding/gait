---
status: done
---
# Tool Output Schemas — Programmatic `Returns:` Line

## Goal

Each tool declares its output shape as a Rust struct with `#[derive(JsonSchema)]`.
The provider adapters automatically render it as a compact `Returns: {key: type, …}` line
appended to the tool description. No hand-editing of description strings. Model reads it;
harness does not validate it (that's `structured_done`'s job).

## Motivation

Tools already have typed input schemas (schemars derive → JSON Schema → model reads it).
Output is invisible: the model guesses what `shell` returns, may treat it as a plain string
and miss `exit_code`, or invent a shape for `list_agents`. A `Returns:` line closes the
contract without adding a new feature layer.

## Architecture

```
Tool impl
  → fn output_schema() -> Option<&Value>   (new trait method, default: None)
         ↑ generated from OutputType struct via make_schema::<OutputType>()
         
Provider adapters (anthropic/mod.rs, openai/mod.rs)
  → render_description(tool) = tool.description() + compact_returns(tool.output_schema())
  
compact_returns(schema) → "\n\nReturns: {output: string, exit_code: integer, timed_out: boolean}"
                         or "\n\nReturns: [string]" for array types
                         or nothing for string / opaque returns (no schema needed)
```

## Files

- **`src/types.rs`** — add `fn output_schema(&self) -> Option<&serde_json::Value>` to
  `Tool` trait with default `None`; add `fn render_description(t: &dyn Tool) -> String`
  free function (used by both provider adapters)
- **`src/anthropic/mod.rs`** — replace `t.description().to_string()` with
  `render_description(t.as_ref())`
- **`src/openai/mod.rs`** — same
- **`src/tools/shell.rs`** — add `ShellOutput` struct + `output_schema()`
- **`src/tools/spawn_agent.rs`** — add `SpawnAgentOutput` struct + `output_schema()`
- **`src/tools/list_agents.rs`** — add `AgentEntry` + `ListAgentsOutput` + `output_schema()`
- **`src/tools/glob.rs`** — add output schema (returns `[string]`)
- Other tools (`read`, `grep`, `ls`, `codesearch`, `webfetch`, `edit`, `write`) — string
  return, `output_schema()` stays `None`; the description already says what you get

## The `output_schema` trait method

```rust
// In types.rs, inside the Tool trait:

/// JSON Schema describing what this tool returns.
/// When `Some`, provider adapters append a compact "Returns: {…}" line
/// to the tool description automatically. Default: None (no annotation added).
fn output_schema(&self) -> Option<&serde_json::Value> { None }
```

Alongside it, a free function used by both provider adapters:

```rust
/// Build the description string sent to the provider API.
/// Appends "Returns: {compact shape}" when the tool has an output schema.
pub fn render_description(tool: &dyn Tool) -> String {
    match tool.output_schema() {
        None => tool.description().to_string(),
        Some(schema) => format!("{}\n\nReturns: {}", tool.description(), compact_type(schema)),
    }
}
```

## `compact_type` renderer

Converts a JSON Schema to a one-line human-readable shape. Lives in `types.rs` or a
small `src/schema_render.rs`.

```rust
/// Render a JSON Schema as a compact type annotation (for tool descriptions).
/// e.g. {output: string, exit_code: integer} or [string] or string
pub fn compact_type(schema: &serde_json::Value) -> String {
    match schema.get("type").and_then(|v| v.as_str()) {
        Some("object") => {
            let props = schema.get("properties")
                .and_then(|p| p.as_object())
                .cloned()
                .unwrap_or_default();
            let fields: Vec<String> = props.iter()
                .map(|(k, v)| format!("{}: {}", k, compact_type(v)))
                .collect();
            format!("{{{}}}", fields.join(", "))
        }
        Some("array") => {
            let items = schema.get("items").unwrap_or(&serde_json::Value::Null);
            format!("[{}]", compact_type(items))
        }
        Some(t) => t.to_string(),    // "string", "integer", "boolean", "number"
        None => "any".to_string(),
    }
}
```

This is intentionally naive — it handles the flat structs our tools return. No recursion
depth limit needed since our output types are all one level deep.

## Tools that get output schemas

### `shell` (most important)

```rust
#[derive(JsonSchema)]
struct ShellOutput {
    /// Combined stdout + stderr of the process.
    output: String,
    exit_code: i32,
    timed_out: bool,
}
```

Rendered: `Returns: {output: string, exit_code: integer, timed_out: boolean}`

### `spawn_agent`

```rust
#[derive(JsonSchema)]
struct SpawnAgentOutput {
    /// The agent's final message (what it would have printed).
    message: String,
    /// Structured result if the agent called structured_done with a result field.
    result: Option<serde_json::Value>,
}
```

Rendered: `Returns: {message: string, result: any}`

### `list_agents`

```rust
#[derive(JsonSchema)]
struct AgentEntry {
    id: String,
    status: String,   // "running" | "done" | "failed"
    task: String,
}

#[derive(JsonSchema)]
struct ListAgentsOutput {
    agents: Vec<AgentEntry>,
}
```

Rendered: `Returns: {agents: [{id: string, status: string, task: string}]}`

### `glob`

Returns a list of matched paths. Add output schema so the model knows it's `[string]`,
not a newline-joined blob.

```rust
#[derive(JsonSchema)]
struct GlobOutput {
    paths: Vec<String>,
}
```

Rendered: `Returns: {paths: [string]}`

### `read`, `grep`, `ls`, `codesearch`, `webfetch`, `edit`, `write`

These all return plain strings. The existing description text already explains what the
string contains. Leave `output_schema()` as `None` — no `Returns:` line added. Avoids
noise where prose is clearer than a type annotation.

### `structured_done`

No `output_schema()` — it returns `"recorded"` (a tombstone). Not useful to advertise.

## Provider adapter changes

`src/anthropic/mod.rs` around line 46:
```rust
// Before:
description: t.description().to_string(),

// After:
description: crate::types::render_description(t.as_ref()),
```

`src/openai/mod.rs` around line 219:
```rust
// Before:
description: t.description().to_string(),

// After:
description: crate::types::render_description(t.as_ref()),
```

Both are one-line changes once `render_description` exists.

## How each tool stores its output schema

Each tool struct gets a `output_schema: serde_json::Value` field, populated in `new()`:

```rust
pub struct ShellTool {
    parameters: Value,
    output_schema: Value,  // ← new
}

impl ShellTool {
    pub fn new() -> Self {
        Self {
            parameters: make_schema::<ShellParams>(),
            output_schema: make_schema::<ShellOutput>(),  // ← new
        }
    }
}

impl Tool for ShellTool {
    fn output_schema(&self) -> Option<&Value> { Some(&self.output_schema) }
    // ... rest unchanged
}
```

`make_schema::<ShellOutput>()` is the same helper already used for input schemas —
no new infrastructure.

## What does NOT change

- `parameters()` / input schemas — untouched
- Tool execution logic — untouched
- Proto / transcript — untouched
- MCP tools (`ToolDef`) — `output_schema()` defaults to `None`, no change needed
- Validation — this is purely informational; `structured_done` handles enforcement

## Testing

- `compact_type` unit tests: object, array, string, nested array-of-objects
- `render_description` unit test: tool with schema appends `Returns:`, tool without doesn't
- Each new output struct: `make_schema::<T>()` round-trips via `compact_type` to expected string
- Existing tool description tests: update expected strings to include the new `Returns:` line
  for `shell`, `spawn_agent`, `list_agents`, `glob`

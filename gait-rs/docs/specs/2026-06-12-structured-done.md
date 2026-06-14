---
status: done
---
# `structured_done` — Schema-Validated Turn Completion

## Goal

Rename the `output` tool to `structured_done`. Add optional caller-declared JSON Schema
validation of the `result` field, with automatic retry on mismatch. No schema = existing
freeform behavior. Schema present = harness enforces the shape, model retries up to
`output_schema_retries` times before the turn fails hard.

## Motivation

Orchestrators (workflow nodes, parent agents, shell scripts) often need the model's result
in a specific shape. Today `result` is freeform `Option<Value>` — the caller has to guess
or parse. The caller knows the shape it needs; the model knows how to produce it; the
harness should enforce it. Same pattern as tool input schemas and permission scopes.
`output_schema_retries: 3` already exists in config but nothing validates against it.

## Architecture

```
gait.jsonc / --result-schema CLI / workflow node
    ↓
GaitConfig.result_schema: Option<Value>
    ↓ ResolvedProfile
    ↓ OutputTool::with_schema(schema)   ← mutates tool name, description, parameters
    ↓ prompt.rs: inject <result-schema> block  ← model sees it at turn-start
    ↓ turn.rs: TurnComplete path
        → validate result against schema
        → on fail: inject ToolResult error, decrement retry counter, continue loop
        → on pass (or no schema): emit TurnComplete, return TurnResult::Done
```

## Files

- **`src/tools/output.rs`** — rename tool to `structured_done`; add `with_schema` constructor;
  mutate description and `result` parameter constraints when schema is set
- **`src/tools/mod.rs`** — update pub use alias to `StructuredDoneTool` (keep `OutputTool`
  as a deprecated alias to avoid breaking tests in one shot)
- **`src/config.rs`** — add `result_schema: Option<Value>` to `GaitConfig`
- **`src/profile.rs`** — thread `result_schema` through `ResolvedProfile`; pass to
  `OutputTool::with_schema` when building the default tool list
- **`src/prompt.rs`** — inject `<result-schema>` XML block when `result_schema` is set
- **`src/turn.rs`** — validate at `ExecutionSignal::TurnComplete`; inject retry error as
  ToolResult; use `output_schema_retries` counter
- **`Cargo.toml`** — add `jsonschema` crate for runtime validation
- All tests: `"output"` string literals → `"structured_done"`

## Tool name and description

### No schema (default)

```
name: "structured_done"
description: "Return structured output from this turn. Use when the caller needs to read
specific fields — in a workflow via $node.output.field, or from a parent agent via
spawn_agent. For plain-text completion, just respond — the turn ends automatically."
parameters: { message: String (required), result: Option<Value> }
```

### Schema present

```
name: "structured_done"
description: "Return structured output from this turn. Your `result` field MUST conform
to the schema declared by the caller — see <result-schema> in the system prompt.
Validation is enforced; a mismatch sends the error back for you to retry."
parameters: { message: String (required), result: Value (required, shape from schema) }
```

When a schema is set, `result` changes from `Option<Value>` to required `Value`, and the
tool description notes enforcement. The schema itself is in the system prompt block, not
inlined into the tool parameters (keeps the tool declaration small).

## System prompt injection

`compose_system_prompt` (or its caller) receives `result_schema: Option<&Value>`. When
set, append after the harness block:

```xml
<result-schema>
Your `result` field when calling `structured_done` must match this JSON Schema:
{...schema...}
Validation is enforced. If your call fails, you will receive the validation error and
must retry with a conforming value.
</result-schema>
```

## Validation and retry logic (`turn.rs`)

At `ExecutionSignal::TurnComplete`:

```rust
// Existing: extract message + result from tc.args
let message = tc.args.get("message").and_then(|v| v.as_str()).unwrap_or("").to_string();
let result = tc.args.get("result").cloned();

// New: validate if schema configured
if let Some(schema) = &result_schema {
    let value_to_validate = result.as_ref().cloned().unwrap_or(Value::Null);
    if let Err(errors) = validate_against_schema(schema, &value_to_validate) {
        if schema_retries_remaining > 0 {
            schema_retries_remaining -= 1;
            // Inject validation error as a tool result so the model sees it
            emitter.emit(events::tool_call(
                tc.tool_call_id.clone(), tc.tool_name.clone(), &tc.args,
            ))?;
            let error_msg = format!(
                "result schema validation failed ({} retries remaining):\n{}",
                schema_retries_remaining, errors
            );
            emitter.emit(events::tool_result(
                tc.tool_call_id.clone(), error_msg, None, None,
            ))?;
            // Continue the loop — do NOT return TurnResult::Done
            continue; // (restructure as needed for the loop shape)
        } else {
            return Err(anyhow::anyhow!("result schema validation failed after retries: {}", errors));
        }
    }
}
// ... existing TurnComplete path
```

`schema_retries_remaining` is initialized from `caps.output_schema_retries` at turn-start.

## `validate_against_schema` helper

Add to `turn.rs` (or a new `src/schema_validation.rs`):

```rust
fn validate_against_schema(schema: &Value, value: &Value) -> Result<(), String> {
    let compiled = jsonschema::JSONSchema::compile(schema)
        .map_err(|e| format!("invalid schema: {e}"))?;
    let result = compiled.validate(value);
    if let Err(errors) = result {
        let msgs: Vec<String> = errors.map(|e| format!("  - {e}")).collect();
        return Err(msgs.join("\n"));
    }
    Ok(())
}
```

## Config and profile threading

`GaitConfig`:
```rust
/// JSON Schema that `result` in `structured_done` must conform to.
/// If set, harness validates and retries on mismatch (up to output_schema_retries).
#[serde(default, skip_serializing_if = "Option::is_none")]
pub result_schema: Option<serde_json::Value>,
```

`ResolvedProfile`:
```rust
pub result_schema: Option<serde_json::Value>,
```

In `profile.rs` where tools are assembled:
```rust
let output_tool = match &self.result_schema {
    Some(schema) => OutputTool::with_schema(schema.clone()),
    None => OutputTool::new(),
};
```

Pass `result_schema` into `run_turn` via `ResolvedProfile` so `turn.rs` can read it.

## `OutputTool` constructors

```rust
impl OutputTool {
    pub fn new() -> Self {
        Self { parameters: make_schema::<OutputParams>(), result_schema: None }
    }

    pub fn with_schema(schema: Value) -> Self {
        // Patch the parameters: make `result` required + add schema description
        let mut params = make_schema::<OutputParamsWithResult>();
        // result_schema stored for use by turn.rs (passed through ResolvedProfile)
        Self { parameters: params, result_schema: Some(schema) }
    }
}
```

The `result_schema` field on `OutputTool` is informational for description mutation only;
the actual validation happens in `turn.rs` where it has access to retry counts and emitter.

## Rename scope

All occurrences of the string `"output"` as a tool name:
- `src/tools/output.rs`: `fn name() -> "structured_done"`
- `src/turn.rs`: all `tool_name: "output".into()` in tests → `"structured_done"`
- `src/proto/events.rs`: same
- `src/ui/renderer.rs`: `tool_target` match arm (currently `None` for `"output"`) —
  remove or keep as `"structured_done"` returning `None`

The file `src/tools/output.rs` stays named `output.rs` (file rename is low-value churn).
The struct `OutputTool` stays (rename is low-value, tests use it directly).

## Cargo.toml addition

```toml
jsonschema = { version = "0.18", default-features = false }
```

## What does NOT change

- `output_schema_retries` config key name — already correct, no rename needed
- The `result: Option<Value>` field semantics when no schema — same as today
- The `TurnOutput` proto struct — it already has `result: Option<Value>`; the schema is
  a harness-layer concern, not a protocol concern
- Workflow `$node.output.field` access — works the same; schema just constrains the shape

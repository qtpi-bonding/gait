# Bug: a malformed `tool_use.input` (JSON string, not object) poisons message history → next provider call 400s → session crashes

**Status:** ✅ FIXED 2026-06-11. Confirmed in a real run, gait session `sb958e9fa`
(gait-dir `/Users/aicoder/Documents/gait-trace`). Fix: `provider::parse_tool_args`
+ `coerce_tool_input` guarantee a tool-call input is always a JSON object —
enforced at both SSE finalize points (root cause, `anthropic/sse.rs` +
`openai/mod.rs`) and at both history-serialization send-sides (backstop in
`anthropic/types.rs::to_anthropic_messages` + `openai/mod.rs`, which also recovers
already-poisoned transcripts on disk). Implemented suggestions #1 and #2 below;
#3 (catch-and-self-repair the 400) was deemed unnecessary since the poison can no
longer be written or sent. 6 new unit tests, all 449 existing tests still green.
**Severity:** High. A single malformed tool call from the model **aborts the
entire session with a non-zero exit** and loses all in-progress work — there is
no recovery, even though the underlying tool error was locally recoverable.
**Affects:** The turn loop / message-history serialization for streamed
`tool_use` blocks, and the absence of a guard when re-sending history to the
provider.

## What happens

The model (MiniMax-M3) emitted an `edit` tool call whose `input` was an
**incomplete/invalid JSON string** — literally `"{\"edits\": "` (a truncated
buffer) rather than a JSON object. Two things then happened:

1. **Local (recoverable):** the `edit` tool tried to deserialize that input into
   its params struct and returned a clean error:
   ```
   error: invalid type: string "{\"edits\": ", expected struct HashlineEditParams
   ```
   gait recorded this as a tool result and the turn did not crash here. So far so
   good — this is exactly the recoverable path.

2. **Fatal (next turn):** the assistant message still carried the original
   `tool_use` block whose `input` was a JSON **string**, and that block stayed in
   history. On the **next** provider request, the Anthropic-compatible endpoint
   rejected the whole request:
   ```
   [gait] error: stream error: HTTP error 400:
   {"type":"error","error":{"type":"invalid_request_error",
    "message":"invalid params, messages.62.content.1.tool_use.input:
     Input should be a valid dictionary (2013)"},
    "request_id":"067a803bf0652ec1b4102e5f2b69ed83"}
   ```
   `tool_use.input` must be an object/dict; a string is invalid. The 400 is not
   caught or repaired — gait exits 1 and the session is dead.

The task was ~95% done (5 of 6 edits applied) when this killed it. The
controlling agent had to finish the last edit by hand.

## Why this is a gait bug (not just a model slip)

The model producing a malformed tool input is a model slip — but the *tool*
should have caught it (it did, locally) **and** the *history layer* should never
let a malformed `tool_use` block be re-sent to the provider. A single bad token
stream from the model should cost one turn, not the whole session. Right now the
malformed block is persisted verbatim and becomes a permanent poison pill: every
subsequent provider call includes message 62 and 400s, so even a retry can't
recover.

## Code (likely at fault)

- `gait-rs/src/anthropic/types.rs:78` — `AnthropicBlock::ToolUse { id, name,
  input: serde_json::Value }`. The `input` is a free `Value`, so a
  `Value::String` is representable and slips through.
- `gait-rs/src/anthropic/sse.rs` — accumulates the streamed `input_json_delta`
  for a tool_use block. When the accumulated buffer is incomplete/invalid JSON,
  it appears to be stored as a raw `Value::String` instead of being parsed to a
  `Value::Object` (or rejected) at `content_block_stop`.
- `gait-rs/src/turn.rs:~698-706` — the provider call + `stream error` mapping.
  The 400 from a poisoned prior block is mapped to a hard error that aborts the
  run; there's no sanitize-and-retry.

## Suggested fix (hypothesis)

Smallest correct fix is to keep history provider-valid at the source:

1. **At `content_block_stop` for a `tool_use`, require the accumulated input to
   parse to a JSON object.** If it doesn't (incomplete/invalid, or a bare
   string), substitute `Value::Object({})` (or a `{ "_malformed_raw": "<buf>" }`
   placeholder) before committing the block to history. The tool will then return
   its normal "missing/invalid params" result, the model sees the error, and the
   existing model-turn-after-rejection path (`continue 'spawn`, the C2 fix) lets
   it retry — *with valid history*. One bad turn, not a dead session.

2. **Defensive backstop:** before sending history to the provider, assert every
   `tool_use.input` is an object; coerce any non-object to `{}`. Cheap insurance
   against any other path that could store a non-object input.

3. (Optional) Catch a provider 400 whose message names a `tool_use.input` and
   self-repair that block, then retry once, instead of aborting.

#1 is the root-cause fix and is a few lines at the SSE finalize point. #2 is a
one-line guard. #3 is nice-to-have resilience.

## Reproduction

Deterministic repro needs a model that emits a malformed/truncated tool_use
input, which is model-dependent (seen here with MiniMax-M3 under load on a large
`edit` call). To reproduce the *fatal* half deterministically without the model:

1. Construct a session history where one assistant `tool_use` block has
   `input` = a JSON string (e.g. `"{\"edits\": "`) rather than an object.
2. Issue any follow-up turn so gait re-sends that history to the provider.
3. Observe the 400 (`tool_use.input: Input should be a valid dictionary`) and the
   session aborting with exit 1.

The local tool-deserialize half reproduces by sending `edit` with a non-object
`input` (a bare string); the tool returns the `invalid type: string ... expected
struct HashlineEditParams` error and does *not* crash — confirming the crash is
the history/provider half, not the tool.

## Related

- `bug-2026-06-09-edit-tool-shape-rejected.md` (C1, closed/fixed in ed1badd) was
  about **well-formed** edit input being rejected at deserialize. This is the
  opposite end: **malformed** input that the tool correctly rejects locally but
  that then **poisons history** and kills the next provider call. Different root
  cause, different fix surface (history/SSE finalize, not the edit schema).

Filed by: grorg agent (Rust)

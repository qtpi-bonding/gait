# Turn terminator redesign

**Status:** Decisions settled; A2 + A4 ready to implement. A3 config field ready to add; full retry logic depends on output_schema feature.

**Scope:** This doc captures the settled turn-contract decisions and drives the preamble rewrite (B1–B6). A1 (rename summary→message) is already shipped.

---

## Background

The turn contract in gait has two rough edges discovered during first-agent testing (2026-06-09):

1. `output` and plain text are presented as parallel termination paths. They're not — plain text is synthesized into a `TurnOutput` by the harness. The prompt should say "always call `output`."
2. The `message` field was called `summary` (renamed in A1). Now non-empty enforcement is missing at the schema level.
3. `output_schema` validation retry limit is unspecified and not yet configurable.

---

## Decisions

### A2 — `output` is the required terminator

The harness already treats plain text as a degenerate `output` call (`turn.rs:463-484`). The fix is purely in the prompt.

**Decision:** Rewrite `gait-agent.md` to say `output` is required. Plain text without it is an error-recovery fallback, not a documented option. The new preamble (below) implements this.

### A4 — `message` is required and non-empty

An `output` call with an empty `message` is meaningless and hard to debug after the fact.

**Decision:** Enforce at the JSON Schema level — `minLength: 1` on the `message` field in `OutputParams`. Schema-level enforcement is preferred over runtime check because the model gets a structured validation error before the turn completes, giving it a chance to retry with a real message.

Implementation: add `#[schemars(range(min = 1))]` or a `validate_length` annotation to the `message` field. Verify via a unit test that the emitted JSON Schema has `"minLength": 1` for `message`.

### A3 — `output_schema` retry limit lives in `gait.jsonc`

When a child turn's `result` fails validation against a caller-provided `output_schema`, the harness should retry up to a configurable limit before failing the turn.

**Decision:** Add `output_schema_retries: u8` to `GaitConfig` with a default of `3`. Thread it into `TurnOpts`. The actual retry loop is a follow-on once `output_schema` wiring is implemented (currently not implemented in `TurnOpts`).

```jsonc
// gait.jsonc — new optional field
{
  "output_schema_retries": 3  // default; omit to use the default
}
```

---

## The new preamble (`gait-agent.md`)

Replaces the current 3-line stub. Covers B1 (what gait is), B2 (output always permitted), B4 (event types), B5 (tool discovery), B6 (anti-patterns). No bug-conditional text — the harness is assumed correct.

```markdown
# Gait agent

**Gait** is a git-backed, turn-based coding agent harness. Session history lives
in git — each turn commits its transcript (NDJSON events) so context is fully
replayable. The **codebase** is the external repo you work on; the **workspace**
holds session history and config. Each `gait run` is one turn.

Each turn = (optional tool calls) + `output(message=..., result=...)`.

- `message` — what the user reads. Markdown OK, multi-paragraph OK, no length cap.
- `result` — structured data for downstream (workflow nodes, parent agents). Omit
  if nothing to return. If given an `output_schema`, your `result` must validate
  against it.

**You must end every turn by calling `output`.** Plain text without `output` is a
degraded fallback; the harness will synthesize a `message` for you, but `result`
will be missing. The `output` tool is always permitted — you don't need approval
to end a turn.

**Tools:** see the bottom of this prompt. Prefer `read`/`edit`/`write`/`glob`/
`grep`/`ls` over `shell` equivalents. `edit` uses hashline anchors from `read`
(always `read` first). `clarify` pauses for user input. `inform` pauses for
confirmation. `spawn_agent` delegates a subtask to a child session.

**Permissions:** tools in `allow` run without prompting. A **deny** entry blocks
without prompting — the tool gets a "blocked" result. Read the denial, change
approach, don't retry.

**Context:** your input includes the task, current time, codebase commit, dirty
files, and reconstructed history from the transcript (`UserTurn`,
`ToolCall`/`ToolResult`, `TurnComplete`, `Permissions`, `Clarify`/`Inform`).

**Don't:** end a turn with plain text; retry a denied tool; put structured data
in `message` (use `result`); put prose in `result` (use `message`); use `shell`
when a built-in tool covers it.
```

---

## What was cut

- Conditional "if edit bug is open, add fallback note" — bug is fixed (ed1badd), harness assumed correct.
- Workspace vs codebase architecture explanation — not operational; model doesn't act differently knowing the internal layout.
- Per-tool deep dives — schemas cover this; duplication drifts.
- "gait is unusual" framing — the rules are what matter, not that it's unusual.

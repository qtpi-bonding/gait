---
status: done
---

# Turn terminator implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Land the settled turn-contract changes: new preamble (A2/B1–B6), non-empty message enforcement (A4), and `output_schema_retries` in config (A3 stub).

**Architecture:** Three independent changes — prompt file, schema annotation, config struct. No inter-task dependencies; can be done in any order.

**Tech Stack:** Rust + schemars 0.8 + `gait-rs/prompts/gait-agent.md`

**Design doc:** `docs/superpowers/specs/2026-06-09-turn-terminator-redesign.md`

---

### Task 1: Replace `gait-agent.md` preamble (A2 + B1–B6)

**Files:**
- Modify: `gait-rs/prompts/gait-agent.md`

The current file is 3 lines and teaches nothing about the turn contract. Replace it entirely with the preamble from the design spec.

- [ ] **Step 1: Overwrite `gait-agent.md`**

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

- [ ] **Step 2: Verify it's baked into the binary**

`GAIT_AGENT_PROMPT` is embedded at compile time via `include_str!` in `gait-rs/src/lib.rs` (or wherever the constant is declared). Confirm with:

```bash
grep -rn "GAIT_AGENT_PROMPT\|include_str.*gait-agent" gait-rs/src/
```

- [ ] **Step 3: Commit**

```bash
cd gait-rs
git add prompts/gait-agent.md
git commit -m "prompt: replace 3-line stub with full gait preamble (A2/B1-B6)"
```

---

### Task 2: Enforce non-empty `message` at schema level (A4)

**Files:**
- Modify: `gait-rs/src/tools/output.rs`
- Test: `gait-rs/src/tools/output.rs` (existing `#[cfg(test)]` block)

The `message` field on `OutputParams` must have `minLength: 1` in the emitted JSON Schema. Use `schemars` `length` validation annotation.

- [ ] **Step 1: Write the failing test**

Add to `output.rs` tests:

```rust
#[test]
fn message_schema_requires_non_empty() {
    let tool = OutputTool::new();
    let schema = tool.parameters();
    let msg = &schema["properties"]["message"];
    assert_eq!(
        msg["minLength"].as_u64(),
        Some(1),
        "message field must have minLength:1 in schema, got: {schema}"
    );
}
```

Run: `cd gait-rs && cargo test -p gait-ai -- message_schema_requires_non_empty 2>&1 | tail -5`

Expected: FAIL — `message` currently has no `minLength`.

- [ ] **Step 2: Add the annotation to `OutputParams`**

In `gait-rs/src/tools/output.rs`, change:

```rust
#[derive(Deserialize, JsonSchema)]
struct OutputParams {
    /// What the user reads. Markdown OK, multi-paragraph OK, no length cap. Required, non-empty.
    message: String,
```

to:

```rust
#[derive(Deserialize, JsonSchema)]
struct OutputParams {
    /// What the user reads. Markdown OK, multi-paragraph OK, no length cap. Required, non-empty.
    #[schemars(length(min = 1))]
    message: String,
```

`schemars` 0.8 emits `"minLength": 1` for `#[schemars(length(min = 1))]` — no `validator` crate needed. The schema annotation is enough for the model to see the constraint.

- [ ] **Step 3: Run test to verify it passes**

```bash
cd gait-rs && cargo test -p gait-ai -- message_schema_requires_non_empty 2>&1 | tail -5
```

Expected: PASS.

- [ ] **Step 4: Run the full test suite**

```bash
cd gait-rs && cargo test 2>&1 | tail -5
```

Expected: all passing (the existing `parameters_schema_has_summary_field` test already checks `message` is present — make sure it still passes).

- [ ] **Step 5: Commit**

```bash
cd gait-rs
git add src/tools/output.rs
git commit -m "feat(output): enforce message non-empty at schema level (A4)"
```

---

### Task 3: Add `output_schema_retries` to config (A3 stub)

**Files:**
- Modify: `gait-rs/src/config.rs`
- Modify: `gait-rs/src/turn.rs` (`TurnOpts` struct)
- Modify: `gait-rs/src/bin/gait.rs` (where `TurnOpts` is assembled from config)

Add the config field and thread it into `TurnOpts`. The actual retry loop is a follow-on once `output_schema` wiring is implemented — this task just adds the knob so it's configurable from `gait.jsonc` from the start.

- [ ] **Step 1: Write the failing test**

In `gait-rs/src/config.rs` tests (or a new test module):

```rust
#[test]
fn output_schema_retries_defaults_to_3() {
    let cfg = GaitConfig::default();
    assert_eq!(cfg.output_schema_retries, 3);
}

#[test]
fn output_schema_retries_parses_from_json() {
    let json = r#"{"model":"test","output_schema_retries":5}"#;
    let cfg: GaitConfig = serde_json::from_str(json).unwrap();
    assert_eq!(cfg.output_schema_retries, 5);
}
```

Run: `cd gait-rs && cargo test -p gait-ai -- output_schema_retries 2>&1 | tail -5`

Expected: FAIL — field doesn't exist yet.

- [ ] **Step 2: Add field to `GaitConfig`**

In `gait-rs/src/config.rs`, add to `GaitConfig`:

```rust
#[serde(default = "default_output_schema_retries")]
pub output_schema_retries: u8,
```

And the default fn (near `default_compaction_threshold`):

```rust
fn default_output_schema_retries() -> u8 { 3 }
```

And in `GaitConfig::default()`:

```rust
output_schema_retries: 3,
```

- [ ] **Step 3: Add field to `TurnOpts`**

In `gait-rs/src/turn.rs`, add to `TurnOpts`:

```rust
/// Max retries when result fails output_schema validation. Default 3 (from gait.jsonc).
pub output_schema_retries: u8,
```

- [ ] **Step 4: Wire config → TurnOpts in `bin/gait.rs`**

In `cmd_run` (and `cmd_chat` if it also builds a `TurnOpts`), find where `TurnOpts` is assembled from `cfg: GaitConfig` and add:

```rust
output_schema_retries: cfg.output_schema_retries,
```

Search with: `grep -n "TurnOpts {" gait-rs/src/bin/gait.rs`

- [ ] **Step 5: Fix any test TurnOpts that need the new field**

`TurnOpts` is constructed in many tests with struct literals. Add `output_schema_retries: 3` to all of them, or add a `..Default::default()` if `TurnOpts` derives/impls `Default`.

Check: `grep -n "TurnOpts {" gait-rs/src/turn.rs | wc -l` — each needs the field.

The simplest fix is to `impl Default for TurnOpts` with sensible defaults, then use `..TurnOpts::default()` in test literals that don't care about this field.

- [ ] **Step 6: Run tests**

```bash
cd gait-rs && cargo test 2>&1 | tail -10
```

Expected: all passing.

- [ ] **Step 7: Commit**

```bash
cd gait-rs
git add src/config.rs src/turn.rs src/bin/gait.rs
git commit -m "feat(config): add output_schema_retries (default 3) to GaitConfig and TurnOpts (A3)"
```

---

## After all tasks

Run `make install` from `gait-rs/` to install the updated binary. Verify:

```bash
/Users/aicoder/develop/gait run --task "Call output with message='hello'" --auto
```

The turn should end with `output` called and the preamble visible in the session transcript.

# Using gait as a test harness for itself

## Background

gait is a v0 (~9 days old, 436 commits, single author). The current REPL is
rough — usable for debugging, not for primary driving. To work around this, I
run gait as a *subagent* of Claude Code: Claude Code plans and orchestrates,
gait does the long-running / structured-output / replayable work via
`gait run` over CLI. The plan is to eventually drive from gait's own REPL
once it's solid enough, but right now the CLI-as-subagent surface *is* the
product surface.

The REPL exists for debugging, not as the interface I work from day-to-day.
It's getting better, but it's not there yet.

## What I mean when I ask "what do you think of it?"

I'm not asking for a design review. I'm asking the model to be a *user* of
the harness, from the inside.

Tests can verify the proto schema round-trips, that permissions gate
correctly, that the projection fold is consistent. Tests can't tell me what
it *feels like* to be a model running through gait for ten turns:

- When a tool result is a string that should be a struct, the model notices.
- When a clarification pause comes back as a useless signal, the model notices.
- When a curate step is expensive but valuable, the model notices.
- When a filter does what I expect vs. does something subtle, the model notices.

Those are the questions I can't write a unit test for. I have to put a model
in the system and let it report.

## What I'm probing

Three things, roughly in priority order:

1. **Does the substrate hold a model without surprising it?** Multi-turn,
   multi-tool, permission-gated, curate-filtered, parse-filtered — does the
   experience stay coherent, or does something silently break?
2. **Where are the model-lying primitives?** Tools that return tombstones
   (`"recorded"`, `"clarify_signal"`) where the model gets a placeholder
   instead of the real data. These are fine for replay (the log is truth)
   but they make the model's transcript a lie. I want to know which ones are
   load-bearing and which can be made first-class.
3. **What's the cost shape?** Curate is a sub-agent invocation — that's a
   real cost on every filtered call. Parse is in-process regex — that's
   free. Does the model use them correctly? Does the model notice when
   curate is overkill? Does the model fall back to parse when it should?

## What I'm *not* asking for

- A code review with a prioritized backlog.
- Production-readiness concerns.
- "What should gait be when it grows up?" — that's my problem.
- Architecture criticism at the level of "this should be a method on the
  trait instead of a pattern match in turn.rs." Those are real but they're
  v1 problems, not v0 problems.

## How I want the response to feel

A stress test from the perspective of a model living in the system. Not
admiration of the cleverness. Notice what works, notice what lies, notice
what the cost shape *is* — not what it should be. If something is rough,
say so concretely. If something is good, say so concretely. Skip the framing.

The first time I asked, the model gave me a design review. Useful, but the
wrong shape. The second pass was closer — once it saw the git log, it
started to recalibrate. The third pass, after it understood the role, was
the right shape: shorter, more specific, more "here's what I noticed from
inside" and less "here's a backlog."

That's the bar.

## gait's role: now vs. future

gait has two roles, at different stages of readiness.

### Today: CLI subagent

gait is used as a *subagent* of Claude Code (or any other orchestrator
that can shell out). The CLI surface (`gait run --task ...`) is the
product. It's stable, it's structured, and it's what gets used
day-to-day.

The REPL exists for debugging and inspection, not for primary driving.
It's rough, and using it as the main interface would mean running into
more bugs than the CLI surface.

What gait gets right as a subagent:
- Stable CLI invocation with structured I/O (`message`, `result`).
- Replayable log + checkpointed codebase (the substrate is the
  product).
- Permission gate, output filters (parse + curate), output schema
  retry — the primitives that make long subagent runs reliable.

### Future: Claude Code alternative, driven from gait's own REPL

The plan is for gait to grow a polished enough REPL/UI that it can
*be* the orchestrator — a Claude Code alternative you drive directly,
not just a worker you shell out to. The substrate is already there.
The UI is what's being built.

The future-REPL needs everything the substrate already provides,
*plus* a UX layer that doesn't exist yet:
- Plan/act toggling (Cline has this; gait does not).
- Multi-agent coordination at the orchestrator level (Cline has
  `cline --team-name ...`; gait has `spawn_agent` but no team model).
- IDE surface, or at least a really good terminal UI.
- OS-level sandboxing (Pi, Claude Code, and Cline have this; gait's
  permission model is application-level).

The "what gait is distinctive in" list holds for the future too:
- Log-as-truth substrate (rare).
- Curate/parse output filter split (rare).
- Mutation-aware permission projection (rare).
- Recorded log + checkpointed codebase + `done`-with-structured-result
  as a first-class turn boundary (none of the others have all of
  these).

So the substrate-first strategy is: build the engine well first, then
build the cockpit on top. The CLI subagent era is the engine-tuning
phase. The REPL era is the cockpit-building phase. Both are
deliberate; the order is the design.

### Why the substrate: the co-evolution loop

gait's substrate is shaped to enable a *co-evolution loop* between
the model and the harness. The loop works like this:

1. The substrate captures every byte of model-facing state in the
   recorded log (system prompt, loadables, permissions, mutations,
   curate outputs, model identity, the full projection fold).
2. The same log can be replayed through the projection, emitting the
   same model-facing state to a different model, prompt, or curate
   config. The two runs are diffable. The diff *is* the eval signal.
3. Every gait session produces a structured trajectory in the log,
   exportable as fine-tuning data (in the SWE-agent Trajectories
   format or similar).
4. The fine-tuned model gets better at the *idiom* of gait — when
   to use `parse` vs `curate_prompt`, how to structure a `done`
   result, when to ask `clarify`, how to use hashline anchors
   correctly across session-spanning edits.
5. The better model makes the substrate's harder features more
   useful, which produces higher-quality trajectories, which
   produces a better next fine-tune, which makes the substrate
   even more useful.

This is a flywheel. It runs at one-user scale (your own
trajectories on your own tasks produce a model fine-tuned for
*your* gait usage). It runs faster with more users, because more
usage produces more trajectories and more eval signal.

The FOSS license is the first domino: closed-source gait has one
user; FOSS gait has however many decide to use it. Apache 2.0
keeps the flywheel spinning. Closed-source would have stopped it
before it started.

The substrate's "overhead" features (curate, mutation-aware
permissions, structured `done`, hashline edits) are not arbitrary
— they're what the eval loop and the data flywheel needed. And
once the model is fine-tuned to use them well, removing any of
them would degrade the system. The substrate is load-bearing for
the model just as much as for the harness.

## External landscape (very brief)

For reference, here is where gait sits among the open-source agent
harnesses as of mid-2026. None of these are perfect comparisons —
they're all *products*, gait is currently a *substrate* — but they
define the shape of the space.

- **pi-mono** (62k stars): TypeScript monorepo, unified LLM API +
  agent core + TUI + coding-agent CLI. Self-extensible. No built-in
  permission system — punts to containerization.
- **aider** (46k stars): Python, terminal pair-programming with
  repomap + auto-commit + lint/test loop. Single CLI surface.
- **Claude Code** (132k stars): Python+TS, terminal/IDE, natural
  language, sandboxed. The thing gait might become a competitor to
  when the REPL grows up.
- **Cline** (63k stars): TypeScript monorepo, SDK + CLI + VS Code +
  JetBrains + Kanban (separate repo) + Slack/Telegram. Multi-agent
  teams, plan/act mode, scheduled agents. Closest in spirit to what
  gait wants to be in the future.

gait today overlaps most with **pi-mono**'s agent-core layer
(small, focused, substrate-shaped). It does not yet overlap with
Cline or Claude Code at the *product* level — that's the future-REPL
work, not the substrate work.

## Feature idea: structured `done`

When a subagent finishes a long task, the orchestrator (Claude Code, a
workflow node, a shell script) often wants the result in a specific shape:
*"give me a list of files you changed,"* *"give me a rating per restaurant,"*
*"give me the function names you added."* Right now the model can put
anything in `result` — which is flexible but leaves the orchestrator to
guess the shape.

The idea: a sibling tool, `structured_done`, that takes a JSON Schema as
part of its declaration. When called, the harness validates `result`
against the schema and either accepts the turn or sends the validation
error back to the model for a retry (the retry infrastructure already
exists — `output_schema_retries: 3` is in `ResolvedProfile`).

### Rough sketch

- **Scripter side:** declare the schema in `gait.jsonc`, in a workflow
  node, or as a CLI flag. A normal JSON Schema file, no new syntax.
- **Prompt side:** when a schema is configured, the system prompt gets a
  `<result-schema>` block with the schema embedded, plus a one-line
  instruction to call `structured_done` with `result` matching it.
- **Harness side:** on `structured_done`, parse `result` against the
  schema. If it doesn't match, fail the turn with a clear error and let
  the model retry. The harness doesn't need to *understand* the shape —
  it just needs to validate it.
- **Fallback:** if no schema is configured, the model uses plain `done`
  with freeform `result`. The structured-output feature is opt-in
  per-run, not always-on.

### Why this fits

Everything in gait is *"the caller declares, the model produces, the
harness enforces."* Tool calls already have schemas. Permission scopes
are caller-declared. The curate/parse filter is caller-selected.
`structured_done` extends the same pattern to the turn boundary instead
of the tool boundary. The orchestrator knows what shape it wants; the
model knows how to produce it; the harness validates it. No new mental
model, just a new layer of the existing one.

### Open questions

- Should `done` and `structured_done` be two tools, or one tool with an
  optional `schema` field? Two tools makes the prompt crisper
  ("use this when you have a schema, use that when you don't"); one
  tool keeps the model from having to choose.
- Should the validation error be sent back as a `ToolResult` for the
  model to see, or as a synthetic `UserTurn`? The first keeps the
  turn-end semantic; the second is more natural for "the schema
  rejected your result."
- Is the schema *only* for the model, or does the harness *use* it for
  anything (e.g. typed downstream consumers)? The first keeps the
  feature small; the second makes it load-bearing and harder to change
  later. Defer until a real consumer needs the typed version.

## Open observations from running through the harness

Things the model notices from inside that aren't bugs but are worth
recording. The "fix" for some of these is documentation; for others,
it's a small design change.

### Every tool should expose its input and output shape

Right now the input shape is in the tool's JSON Schema (visible to the
model) but the *output* shape isn't. The model can guess what `shell`
returns from the description, but the real contract lives in
`turn.rs:210-222` as a pattern match on the result value. Same shape
as the `recorded` and `clarify_signal` model-lying primitives: the
real answer is somewhere the model can't see, and getting it wrong is
silent.

The fix is to make the output shape a first-class part of the tool
contract. Either as a method on the `Tool` trait, or as an `output`
field in the tool description that the harness can surface to the
model. Then a model can read "this tool returns `{output: string,
exit_code: int, timed_out: bool}`" instead of guessing.

This also collapses several open questions:
- `list_agents` would have a discoverable return type.
- `read` / `shell` / `codesearch` / `webfetch` could advertise whether
  they support `parse` and `curate_prompt` filters as part of the same
  description, instead of relying on the model to know which tools
  accept which filter args.
- New tools added later would carry their output shape with them
  automatically.

### `clarify` two-way is currently broken

The `clarify` tool returns `"clarify_signal"` to the model, and the
real Q&A happens out of band. The model can't observe what shape the
answers come back in (or even that they came back at all) because the
resumed turn is a fresh context the model doesn't have visibility
into. As of this writing, the resume path is acknowledged broken.

This is the same model-lying primitive as `recorded` / `output` /
`done` — the model emits a tombstone, the real data lives somewhere
else, the transcript lies. The fix is to make the two-way contract
visible: when `clarify` resumes, the answers should be a real
structured event in the log that the model can read, not a synthetic
`UserTurn` the model has to guess the shape of.

### `<status>` cadence is user-controlled, not gait-controlled

The `<status>` block the model sees every turn — codebase commit,
dirty files, cwd — is a *snapshot of the user's codebase*, not of
gait's internals. The user (and the model) control when commits
happen. gait just reads whatever is on disk and in the git index at
turn boundaries.

Implication: the model should not assume `<status>` is fresh after a
`write` or `edit` in the same turn. The next turn's `<status>` may
or may not reflect those changes, depending on whether a commit
happened. If the model needs a known-good snapshot, it should commit
explicitly (via `shell`) or run `git status` itself.

This isn't a bug — it's a contract. But the contract isn't in the
prompt yet, so models guess. Add a one-liner to the system prompt:
*"`<status>` reflects the user's codebase at the last turn boundary,
not gait's internal state. The user controls commit cadence."*

### "Replayable" means checkpointed, not re-executable

gait's "replay" is *snapshot-based*: each turn has a checkpoint of
the codebase state, and you can start from any checkpoint. It does
*not* mean "re-run the model calls and watch what happens again" —
the transcript is a log of what already happened, not a script to
re-execute. Curator sub-sessions, LLM calls, and tool side-effects
are not re-driven on replay.

This matters because the model might assume "I can look at my
transcript and reproduce what I did." You can't — the transcript is
the *evidence*, not the *recipe*. To reproduce, the model would need
to read the codebase at the checkpoint and reason from there.

### Naming note: "recorded" and "checkpointed," not "replayable"

Both the log and the codebase are *checkpointed* — they have
discrete per-turn snapshots, not a continuous stream. Neither is
*replayable* in the sense of "press rewind and watch the model
re-execute its actions." The log is a *record* of what happened
(text in, text out, tool calls, tool results, structured events);
the codebase is a *checkpoint* of the user's code at each turn
boundary. Together they let you audit and restore, but not rerun.

Concretely:

- **The log is recorded.** It says what the model did, said, and
  got back. It does not say "do this again." Curator sub-sessions,
  LLM calls, and tool side-effects are not re-driven by reading the
  log.
- **The codebase is checkpointed.** Each turn has a snapshot. You
  can `gait checkout` (or whatever the command is) to restore the
  codebase to a previous turn's state. The snapshot is the user's
  code as it existed at that boundary, not a script to re-run the
  model's actions against it.

To reproduce what a previous model did, you read the codebase at
the checkpoint and reason from there. The log is the *evidence*
about that state, not the *recipe* to recreate it.

This is the same model-lying primitive class as `recorded` /
`clarify_signal` / `done`: the name implies one thing, the
behavior is another. The fix is to standardize the vocabulary —
"recorded" for the log, "checkpointed" for the codebase, and drop
"replayable" wherever it implies re-executability.

## More observations from running through the harness

### `spawn_agent` is underexplained to the model

The model can call `spawn_agent` to delegate work, but the tool
description (and the system prompt) say almost nothing about:

- What permissions the child inherits (parent's `allow`/`deny`? A
  subset? Empty?).
- Whether the child sees the parent's log or starts fresh.
- What the return shape is when the child finishes.
- How `list_agents` correlates to `spawn_agent` calls.
- Whether the parent can talk to the child mid-run, or only when it
  finishes.
- What the lifecycle of a child session is (does it auto-commit?
  Auto-cleanup? Persist forever?).

A model that doesn't know these things won't use `spawn_agent` even
when it should. Either the tool description needs to spell them out,
or there should be a sibling tool (`list_agents`, plus a `wait` or
`poll` tool) that lets the model observe the agent lifecycle.

If the goal is to make `spawn_agent` a first-class primitive, the
contract needs to be visible. Right now it's a black box the model
won't reach for.

### Hashline anchors are per-session, not per-read (corrected)

Correction to a previous turn: hashline anchors are stable for the
duration of a session, not the duration of a single read. A model
that has read a file in turn 3 can still edit it in turn 8 using
the anchors from the turn-3 read, as long as nothing has modified
the file in between. If the file has been modified, the anchors
shift and the model needs to re-read.

The re-read tax is real but smaller than I originally thought. The
real cost is the model has to *know* that anchors are session-scoped,
and the prompt should make that explicit. A one-liner like *"hashline
anchors are stable for the lifetime of the session unless the file
has been modified; re-read to refresh"* would close the loop.

### `session` means one specific thing

In gait's vocabulary, "session" refers to a single `log.jsonl` (the
recorded transcript) plus its associated state. Not a UI session,
not a network session, not a model-provider session. Just: the
thing you and the model read and write, identified by a session ID.

If a different word is needed for related concepts (e.g. a
worktree, a checkpoint, a model-provider connection), they should
get distinct names. The current overloading of "session" makes it
harder to reason about the system than it needs to be.

### Permission pause is visible, but only on the resume path

Correction to a previous turn: the model *does* see "user approved
[once/agent/global]" in the resumed turn's tool result — the
prefix is added to the tool output before the model sees it
(`turn.rs:556-572`). So the model has *some* visibility into the
permission decision, just on the next turn rather than at the
moment of the gate.

What's still missing is visibility at the gate itself: the model
emits `Permissions` and then the turn ends, with no signal about
*what the user is being asked*. The model learns the decision but
not the question. A `PermissionsPending` event in the log that
records the items under review (visible to anyone reading the log,
even if not to the model in the same turn) would help debug and
audit.

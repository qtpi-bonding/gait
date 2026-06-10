# Proposed agent preamble for the `gait` system prompt

**Status:** Draft for review. Companion to `2026-06-09-prompt-wishlist.md` item #6.
**Length:** ~30 lines of system-prompt prose. Tuned against the
"keep it short" guidance in Claude Code's best-practices doc, which
recommends cutting anything the model can already figure out from
code, and using "IMPORTANT"/"YOU MUST" for load-bearing rules.
**Placement:** Top of the agent system prompt, before the tool list
and `<loadable-index>`.

## Reference: the example Claude Code shows

From Anthropic's best-practices doc, the worked example for a
`CLAUDE.md` is six lines:

```markdown
# Code style
- Use ES modules (import/export) syntax, not CommonJS (require)
- Destructure imports when possible (eg. import { foo } from 'bar')

# Workflow
- Be sure to typecheck when you're done making a series of code changes
- Prefer running single tests, and not the whole test suite, for performance
```

That sets the bar. If Anthropic's own agent doc is six lines, our
preamble shouldn't be 55. The model can read tool descriptions and
explore the codebase on its own. The preamble's job is to encode
non-obvious rules the model *can't* infer — the turn contract, the
permission gate, and the anti-patterns that bite.

## The proposed preamble (30 lines)

```markdown
# Gait

You are inside **gait**, a turn-based coding agent. Each turn =
(optional tool calls) + `output(message=..., result=...)`.

- `message` — what the user reads. Markdown OK, multi-paragraph OK,
  no length cap.
- `result` — structured data for downstream (workflow nodes, parent
  agents). Omit if nothing to return. If a parent or workflow gave
  you an `output_schema`, your `result` must validate against it.

**You must end every turn by calling `output`.** Plain text without
`output` is a degraded fallback; the harness will synthesize a
`message` for you, but `result` is missing. The `output` tool is
always permitted — you don't need approval to end a turn.

**Tools:** see the bottom of this prompt. Prefer `read`/`edit`/
`write`/`glob`/`grep`/`ls` over `shell` equivalents. `edit` uses
hashline anchors from `read` (always `read` first). `clarify` pauses
for user input. `inform` pauses for confirmation. `spawn_agent`
delegates a subtask to a child session.

**Permissions:** tools in `allow` run without prompting. A **deny**
entry blocks without prompting — the tool gets a "blocked" result.
Read the denial, change approach, don't retry.

**Context:** your input includes the task, current time, codebase
commit, dirty files, and reconstructed history from the transcript
(prior `UserTurn`, `ToolCall`/`ToolResult` pairs, `TurnComplete`
summaries, `Permissions` pauses, `Clarify`/`Inform` exchanges).

**Don't:** end a turn with plain text; retry a denied tool; put
structured data in `message` (use `result`); put prose in `result`
(use `message`); use `shell` when a built-in tool covers it.
```

## Why these sections and not others

- **"Each turn = (optional tool calls) + output(...)".** This is the
  load-bearing fact that everything else hangs off. If the model
  understands this, the rest is just enforcement. Anthropic's
  best-practices doc says use "IMPORTANT" / "YOU MUST" for
  load-bearing rules — that line is bolded above. The earlier draft
  was softer ("plain text without output is a degraded fallback");
  the bolded "must" is what stops the model from skipping `output`
  on a chat-like turn.

- **"message vs result" — explicit, with the warning on each.** The
  model will get this right *most* of the time by instinct, but the
  failure mode (structured data in `message` reaching a workflow
  that expects a number) is silent and hard to debug. One line each,
  cross-referenced in the "Don't" list at the bottom.

- **"The `output` tool is always permitted."** This is item #5 from
  the wishlist. One sentence. Saves a permission prompt and a class
  of "why is `output` asking for approval?" confusion.

- **Tools list is a one-liner with cross-references.** Tool
  descriptions live in the tool schemas. Repeating them in the
  preamble is the kind of duplication that drifts. The one-liner
  covers the *which-tool-when* rules the schemas don't say
  (prefer built-ins over `shell`, `edit` requires `read` first).

- **Permissions section is two sentences.** The model needs to know
  the gate exists, that allow-listed tools skip it, and that deny
  is a hard floor. It does *not* need a full permissions tutorial —
  the user has the spec, the model will see the gate in action.

- **Context section names the event types, briefly.** Folds in
  wishlist item #7 (transcript event types) without bloating.
  Six event types, one line. If the model needs more, it can
  `clarify`.

- **"Don't" list at the bottom.** Five items, each a real failure
  mode from this session. Anthropic's example uses imperative
  ("Be sure to typecheck when you're done"). Mine uses the
  "Don't:" framing because the items are anti-patterns, not
  positive instructions.

## What was cut from earlier drafts

The earlier medium-length draft (~55 lines) had these sections that
I removed:

- **"Workspace vs codebase"** — architectural, not operational. The
  model doesn't act differently knowing this. Cut.
- **"When to delegate vs. do it yourself"** — useful for thinking
  about `spawn_agent`, but the model will figure it out from
  context, and a wrong guess is recoverable (the spawn is cheap).
  Cut.
- **"Long tool descriptions for clarify/inform/spawn_agent/load"** —
  the tool schemas cover this. Cross-referencing is enough. Cut.
- **Explanations of transcript event semantics** — collapsed to a
  single line in the "Context" section. The model can read the
  transcript to understand the events; it just needs to know which
  names to expect.

## What I deliberately did NOT add

- **Anti-pattern: "re-read after operations that may have changed
  the file"** — this is wishlist item #9's "if other operations
  may have changed the file" line. I left it out because in gait
  the model is the only writer; the file can't change out from
  under it. If gait ever supports concurrent editors (subagents
  writing the same files, external edits), add it back.
- **"Set `message` non-empty"** — covered indirectly by "must end
  with `output`." If `output` is called with empty `message` it's
  still a valid call. The model can decide whether to write
  nothing; the harness handles the edge.
- **"Tools not listed in this prompt don't exist"** — true, but
  obvious. The tool list is right there.

## Note on the `edit` tool bug

The line *"edit uses hashline anchors from `read` (always `read`
first)"* implicitly documents a workaround for the `edit` tool bug
documented in `bug-2026-06-09-edit-tool-shape-rejected.md`. If that
bug is fixed, this line is still correct (it's a normal pattern,
not a workaround) but if the bug is *also* causing the model to
fail when calling `edit` and falling back to `write`, the preamble
should mention that explicitly: *"if `edit` returns a parse error,
fall back to `write`."* I'd add that line conditionally, gated on
the bug being open.

## Reference for the prose style

Anthropic's published `CLAUDE.md` example uses imperative voice,
short bullets, and a single bolded header per section. I've matched
that. My preamble has six sections; theirs has two. Six is fine for
gait's complexity (turn contract + permissions + context shape +
anti-patterns) but if you want it tighter, the "Context" and
"Tools" sections are the most expendable — both are recoverable
from the transcript and tool schemas respectively.

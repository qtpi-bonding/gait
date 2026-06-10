# Gait — agent feedback, turn 1 (2026-06-09)

This file is the customer/agent side of the conversation that produced the
redesign in `turn-terminator-redesign.md` and the bug report in
`bug-2026-06-09-permission-reject-drops-reply.md`. Read those first.

## What was being tested

A first-time interaction with the `gait` harness, asked to comment on the
experience. The agent is the *user/customer* perspective here, not the
implementer. Discussion is with the harness author.

## What came out of it

Three concrete things:

1. **`output` tool: `summary` is mis-named.** The field carries prose, often
   multi-paragraph. "Summary" implies a one-liner. Rename to `message`.
   Confirmed in `gait-rs/src/tools/output.rs:9` and the dozens of test cases
   that hardcode `"summary": "..."`. Full design in
   `turn-terminator-redesign.md`.

2. **Plain text and `output` are presented as parallel paths; they're not.**
   The current system prompt offers "for plain-text completion, just respond"
   as a valid termination. The actual harness synthesizes a `TurnOutput` from
   plain text and emits a `TurnComplete` event regardless. So plain text is
   a degenerate `output` call, not a co-equal option. The fix is to make
   `output` the documented and required terminator; plain text becomes a
   fallback for the rare case where the model forgot. Confirmed against
   `gait-rs/src/turn.rs:463-484` which has the comment
   *"output() is always the turn-completion path; plain text is just a short-hand"*
   — the code already reflects this; the prompt doesn't.

3. **`[r]` reject path in `gait chat` drops the user's reply.** When the
   user is prompted for tool permission and rejects with `[r]`, the harness
   prints `[gait] permission denied` and exits the spawn loop. The model
   gets no turn. The user's text reply (if any) is silently lost. The
   transcript gets a `PermissionsReply` event but no `TurnComplete`, no
   `AssistantText`, nothing the user-facing renderer would surface as
   "what the agent said about the rejection." Full report in
   `bug-2026-06-09-permission-reject-drops-reply.md`.

## Things the agent got wrong or revised mid-discussion

- Initially recommended keeping `summary` and adding a separate
  `human_message` field to preserve "summary" as a short log line. User
  pushed back: just rename. Agreed. Sometimes a one-letter field-name
  change is the right call.
- Initially said the agent prompt should keep "plain text terminates" as
  a documented option. The author pointed out that the harness already
  treats them as the same event, so there's no real distinction. Revised:
  the prompt should just say "always call `output`" and treat plain text
  as a degraded fallback.
- Initially waffled on whether `output_schema` should apply to
  `spawn_agent` calls. Author said yes, with retry on validation failure.
  Agent agreed and added a retry-limit spec.

## What the agent wanted to test but couldn't in this session

- `spawn_agent` control flow and whether the parent gets back a usable
  `result` for branching. (This is what the `output_schema` redesign
  addresses.)
- `gait workflow` resume after a `[r]` reject. If the agent gets the
  bug fix in #3, the resume path should also surface the agent's
  post-rejection text in the workflow trace.
- Progressive tool loading — the `<loadable-index>` was visible in
  context but the agent didn't exercise it.

## Open questions carried forward

- Hard vs soft retry limit on `output_schema` validation. Agent picked
  `3 retries then fail`; author didn't weigh in. See
  `turn-terminator-redesign.md` §"Retry limit" for the trade-off.
- Whether `message` should be required (non-empty). Agent recommends
  required. The author didn't pick.
- The dirty-files noise in the `<dirty-files>` user-turn context
  (`.worktrees/`, `node_modules/`, `tmp/`, `sessions/`, `dist/`). The
  agent flagged this in turn 1. Still unaddressed; not in scope of
  this redesign.

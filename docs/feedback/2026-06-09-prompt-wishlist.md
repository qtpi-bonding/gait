# System-prompt & agent-UX wish list

Companion to `2026-06-09-turn-1-discussion.md`. Captures issues a first-time
agent (the customer) actually tripped over, with concrete proposed fixes.
Each item is structured: **Issue** → **Solution**. Items that overlap
with the redesign in `turn-terminator-redesign.md` are noted.

## Resolved (by the redesign doc)

### 1. `output` tool: `summary` field is mis-named

**Issue.** The field carries prose — often multi-paragraph — but the name
implies a one-liner. Agents (and other readers of the code) hesitate to
put length into it, and the doc comment in
`gait-rs/src/tools/output.rs:8` actively warns "one-sentence summary."

**Solution.** Rename `summary` → `message`. Update the doc comment to
allow markdown and remove the length suggestion. Hard-migrate the wire
format with a transcript version bump.

**Status.** Resolved in `turn-terminator-redesign.md` §1.

### 2. `output` and plain text are presented as parallel termination paths

**Issue.** The current system prompt offers "for plain-text completion,
just respond — the turn ends automatically" alongside calling `output`.
The actual harness treats them as the same event (`TurnOutput` is
synthesized from plain text). The prompt lies about the structure.

**Solution.** Make the prompt explicitly say: *"End every turn by
calling `output`. Plain text without `output` is a degraded fallback;
prefer the explicit call so `result` is populated."* Keep the synthesis
in the harness as a safety net for misbehaving models.

**Status.** Resolved in `turn-terminator-redesign.md` §2.

---

## Open issues

### 3. Dirty-files list is showing gitignored paths (cause unclear — diagnose first)

**Issue.** The user-turn context dumps a `Dirty files:` block
(`turn.rs:81-92` `build_turn_context`) drawn from
`git::get_dirty_files` (`gait-rs/src/git.rs:25-35`). That function calls
`repo.statuses(None)`, which **respects `.gitignore` by default** — so
gitignored paths like `node_modules/`, `dist/`, `bun.lockb`, `sessions/`,
`tmp/`, `.worktrees/`, `target/` should not appear. The repo's
`.gitignore` already lists all of these (verified at
`/Users/aicoder/Documents/ideation/gait/.gitignore`).

Despite this, the agent in this session saw all of those paths in the
dirty-files list. So one of:

1. The paths are *tracked* (not untracked-ignored) and showing as
   `modified`. Tracked files bypass `.gitignore`.
2. The session worktree-creation code (`git.rs:37-80`, especially the
   `.gitignore` write at line 72 and the initial commit) is leaving
   untracked files visible in the parent repo's status.
3. The session is showing stale state from a prior turn / older gait
   version.

The user is right that `.gitignore` is the right layer for this
concern. The fix is **not** to add a code filter; it's to figure out
*why gitignored paths are showing as dirty despite `.gitignore`
covering them.*

**Solution.** Three steps, in order:

1. **Diagnose first.** From a clean checkout, run `git status
   --porcelain` in the repo and compare to what the harness reports.
   If they match, the issue is in `get_dirty_files` or how the harness
   invokes it. If they don't match, the harness is doing something
   different (e.g. running git in a subdir, or running git with
   `--ignored`).
2. **Fix the cause, not the symptom.** The most likely candidates:
   - If session worktrees are creating files visible in the parent
     status: the harness should `git -C <codebase> status` from the
     codebase root, not the workspace, or the worktree creation should
     add to the parent's `.gitignore`.
   - If tracked files are getting modified by the harness itself
     (e.g. `bun.lock` regenerating on install): gate those
     modifications behind a flag, or add `*.lock` to a per-session
     ignore file.
3. **Only if the diagnosis shows the list is genuinely long for a real
   reason** (user has 30+ source files modified), consider truncation:
   show the first 20 and append *"…and N more (see `git status`)"*.
   The filter layer should not exist — git already does the right
   thing once we stop fighting it.

The dirty-files field is also surfaced in the transcript's
`UserTurnContext.dirty_files` (see `turn.rs:339`) — both call sites
should use the same logic.

**Status.** Item downgraded from "filter in code" to "diagnose why
gitignored paths are visible." If the diagnosis is "they're not, the
agent was looking at stale output," close this item.

### 4. `<loadable-index>` is referenced but not always present

**Issue.** The system prompt header line says: *"Check `<loadable-index>`
in the system prompt for available items."* In a fresh session with no
MCP servers or skills enabled, that block is *absent* from the context.
The agent is told to look for a thing that may not exist, with no
guidance on what to do if it doesn't.

**Solution.** Always emit the `<loadable-index>` block, even when
empty. Empty form: `<loadable-index>\n  (none enabled)\n</loadable-index>`.
This makes the "look here for available items" instruction reliable
instead of "look here, but maybe here is empty in a way I can't tell."

If `<loadable-index>` rendering is in `turn.rs` (the
`build_loadable_index` function — see `turn.rs:123-131`), change it to
return the empty-form string instead of `""` when entries is empty,
and remove the `if loadable_idx.is_empty() { … }` short-circuit in the
prompt assembly (`turn.rs:418-422`).

### 5. `output` tool exemption from permission gating is invisible to the model

**Issue.** Calling `output` is exempted from permission gating
(`docs/superpowers/plans/2026-06-06-phase-4b-remaining-tools.md:160`:
*"We also exempt `output` from permission gating so it's never
blocked"*), but that exemption is at the *gating* layer, not at the
*allow-list* layer visible to the model. From the model's POV it's
invisible that this exemption exists. The model can't tell that
`output` is always permitted, and might hesitate to call it (or call
it expecting a permission prompt that never comes).

**Solution.** The exemption is correct — `output` needs to fire even
when the user hasn't pre-approved anything. The fix is *visibility*,
not behavior. Two options:

1. **Document the exemption in the system prompt.** One line: *"The
   `output` tool is always permitted — you don't need approval to end
   a turn."* This is the smallest change and matches how the
   pre-allowlist works for `read`, `ls`, etc. in the model prompt
   header.
2. **Add a tool-list annotation in the prompt** that calls out
   built-in tools that bypass permission gating. E.g. a column
   `[perm: required | always]` next to the tool list. Cleaner long-term
   but a bigger prompt change.

I'd pick option 1 for the first pass.

### 6. Missing "what is gait" preamble in the system prompt

**Issue.** A model that has never seen gait has to infer a lot from
context: that turns are NDJSON events, that `output` is the canonical
terminator, that `result` is for downstream code, that
`spawn_agent` returns structured data, etc. The current prompt teaches
the model the *tools* but not the *environment*.

**Solution.** Add a short preamble (one paragraph) at the top of the
agent system prompt. Suggested wording:

> *"Gait is a turn-based agent harness. Each turn = (optional tool
> calls) + (final `output` call). The harness wraps your text in JSON
> either way. `output(message=..., result=...)` is the explicit
> terminator — `message` is what humans read, `result` is what
> downstream code (workflow nodes, parent agents) reads. Plain text
> without `output` is a degraded fallback; always prefer the explicit
> call so `result` is populated. Transcript events are NDJSON on
> stdout: `UserTurn`, `ToolCall`/`ToolResult`, `TurnComplete`,
> `Permissions` are the ones you'll see in your context."*

This collapses the agent's mental model of "what is the harness doing"
into a few lines that the model can just read.

### 7. Transcript event types are not documented in the prompt

**Issue.** The model sees transcript events as raw context but has no
guide for which events mean what. A `Permissions` event mid-conversation
might confuse a model that doesn't know what triggered it. A
`TurnComplete` from a previous turn tells the model the previous
turn ended — useful for multi-turn awareness, but only if the model
recognizes the event.

**Solution.** Either fold into the preamble from #6 (one sentence per
event type) or add a dedicated section. My preference: fold it in. The
list is short and the preamble is the right place to set expectations.

### 8. Tool descriptions for `clarify`, `inform`, `spawn_agent`, `list_agents` are discoverable but the model may not look

**Issue.** *(Reclassified from the original "I was confused" feedback —
see the note below.)* These tools *do* have descriptions in their JSON
schema (e.g. `gait-rs/src/tools/clarify.rs:38-39`:
*"Ask the user one or more questions and pause until they answer…"*).
The descriptions are good. The model just has to actually read the
tool schema, which it will when it considers using the tool, but
might not when it's deciding *whether* to use it.

**Solution.** No code change needed. Two optional prompt-level nudges
if we want to be belt-and-suspenders:

- In the preamble from #6, one sentence: *"For asking the user
  questions, use `clarify` (pauses for an answer). For explaining
  something and confirming they understood, use `inform` (pauses for
  confirmation). For delegating a subtask, use `spawn_agent` (returns
  when the subtask finishes; check `result` for its output)."*
- In the tool list itself, group tools by purpose: "interaction with
  user" (clarify, inform), "delegation" (spawn_agent, list_agents),
  "filesystem" (read, write, edit, glob, grep, ls, shell), etc.
  Currently the tool list is alphabetical, which is fine for
  reference but bad for discovery.

I'd start with the preamble sentence and skip the tool-list grouping
unless we have evidence the model is missing the tools.

**Note on this item.** I originally flagged it as "I was confused."
Re-reading the codebase, the confusion was mine — the descriptions
exist, I just didn't read the source. I'm leaving the item here
because the *underlying* point (the model might not look at tool
descriptions proactively) is still worth addressing via the
preamble, but I'm downgrading it from "missing docs" to "discovery
ergonomics."

### 9. No anti-patterns section in the system prompt

**Issue.** Models have general tool-use instincts, but gait has a
few specific things that bite:

- Calling `output` with empty `message` and `result: null` and calling
  it a turn.
- Calling the same tool repeatedly when the error is structural (read
  the error, change the approach).
- Using `shell` for things a built-in tool covers (e.g. `cat` instead
  of `read`, `find` instead of `glob`).
- Retrying a denied tool call after the user said no (the model sees
  the denial; it should adapt, not retry).
- Putting structured data in `message` (it goes to humans, not
  machines) or prose in `result` (the opposite).

**Solution.** Add a short "Don't" list to the system prompt, after
the preamble. Suggested wording:

> *"Don't: end a turn with empty `output`. Retry a tool that the
> user denied. Put structured data in `message` (use `result`). Put
> prose in `result` (use `message`). Use `shell` when a built-in
> tool would do (e.g. `read` over `cat`)."*

Five lines. High leverage, because each one is a failure mode that's
hard to debug after the fact.

---

## Items deliberately NOT included

- **"`gait` is unusual" framing.** I considered suggesting we add
  something like *"this is not a normal agent harness — gait is
  turn-based with NDJSON events"* to the prompt. I don't think
  this helps. The model can't act differently based on knowing the
  harness is unusual; it can only act based on the actual rules
  (terminate with `output`, etc.). Frame the rules, not the framing.

- **Long architectural explanation.** Same reasoning. The model
  benefits from "what to do" not "why it's designed this way." If
  we get the preamble right, we don't need an architecture section.

- **Per-tool deep dives.** The JSON schemas are good. Adding
  prompt-level explanations of `clarify`, `inform`, `spawn_agent`,
  etc. would duplicate the schemas and create drift. A single
  sentence in the preamble pointing at the schemas is enough.

---

## Priority

If I were implementing these I'd ship in this order:

1. **#1, #2** (already in `turn-terminator-redesign.md`) — these are
   the highest-impact, because they change the core turn contract.
2. **#6** (preamble) — single biggest unlock for new models. A few
   sentences of orientation prevent dozens of small confusion costs.
3. **#5** (`output` is always permitted) — one line, prevents a
   class of "the model can't end the turn" failures.
4. **#3** (diagnose dirty-files visibility) — may be a quick fix
   once the cause is identified; not a code-filter task.
5. **#4** (always-emit `<loadable-index>`) — one-line code change,
   removes a "look for a thing that may not be there" instruction.
6. **#7, #8, #9** — combine into the preamble rather than separate
   sections. If the preamble from #6 lands, these can be folded in
   or skipped.

Items #7, #8, #9 individually are nice-to-haves. The combined preamble
that addresses all of them is the actual deliverable.

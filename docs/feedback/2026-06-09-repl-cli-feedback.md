# REPL / CLI feedback: control, replies, and session-scoped permissions

**Status:** Customer feedback from a single `gait chat` session, 2026-06-09.
Mixes real bug reports and feature requests. Some items need design
specs before implementation.
**Affects:** `gait chat` REPL, `gait run` CLI control flow, the
permission gate (`gait-rs/src/permission.rs` and related).

This doc captures five distinct observations, raised together in one
session. Each has its own Issue + Solution section. They are
deliberately kept separate because they have different
implementation sizes and uncertainty levels.

---

## 1. No turn rewind in `gait chat` (and unclear in `gait run`)

**Issue.** When an agent's turn goes off the rails — wrong file
edited, wrong approach taken, hallucinated code that the user only
spots after the fact — there's no way to undo the *agent's last
turn* from inside the REPL. The user can `Ctrl+C` to stop a turn
in progress, but a completed turn is committed to the session
branch and the only way back is `git -C sessions/<id> reset HEAD~1`
(per `docs/superpowers/plans/2026-06-05-session-worktrees.md:848-849`).
That's a manual git operation, easy to mistype, and not discoverable
from the REPL.

**Solution direction.** Use git, not a separate file-snapshot
mechanism. Gait already requires git (sessions live in worktrees)
and the harness already commits per turn (`turn.rs:481-482`). The
move is: *commit more often, not invent new infrastructure.*

Sketch (needs spec before implementing — the author will think
about this later):

- Harness commits the session branch after each tool result that
  could mutate files (`read`/`write`/`edit`/`multiedit`/`shell`).
  Not after pure-read tools. The cost is one extra `git add -A &&
  git commit` per mutating call, bounded by session length.
- `/rewind` (REPL) or `--rewind <commit-sha>` (CLI) wraps
  `git reset` against those commits. The existing
  `git -C sessions/<id> reset HEAD~1` patterns from
  `session-worktrees.md:848-849` are the right primitives.
- Because every file change — including those from `shell` — lands
  in git, rewind covers them all. This sidesteps the "only some
  tools are tracked" limitation that a content-snapshot approach
  would have.
- Conversation-rewind (rewinding the transcript context, not files)
  is a separate question and depends on how the harness handles
  transcript compaction. Out of scope until the commit-frequency
  decision is made.

**Unresolved (to think about later).** Whether to keep the existing
per-turn "session(<id>): turn N" commit at `turn.rs:481-482` and
layer in-turn commits on top, or drop it and rely on in-turn
commits only. Both are workable. Also: should the in-turn commits
use a different branch / reflog namespace so the user can see "tool
step 3" vs "turn 4" in `git log`? This is a UX-of-history question
that probably wants a prototype to evaluate.

**Why not a content-snapshot mechanism (à la Claude Code)?** It
avoids needing git, but gait already requires git for sessions
(and for worktrees, branching, `gait finish`, etc.). Adding a
parallel snapshot system means two sources of truth for "what did
the agent change" and a class of bugs where they disagree. The
git-commit approach reuses the primitive that's already there and
covers more cases (including `shell`).

---

## 2. REPL doesn't number agent output, so replies have no anchor

**Issue.** When the agent produces a long response in `gait chat`,
the user has no way to refer to a specific part of it in their
next message. Saying *"the third paragraph you wrote"* is
ambiguous. *"line 47"* is meaningless because the terminal scrolls.
The user can't say *"respond to what you said about X"* in a way
the model can mechanically pair up.

**Solution.** Number the agent's output lines as they render. Use a
narrow gutter (e.g. `  42|`) on the left margin of the REPL, only
on agent turns. User input gets a different prompt marker (e.g.
`>`) with no line numbers. Then add a reply syntax:

- `>3` — refer to line 3 of the agent's most recent turn. Inserts
  the quoted line into the user's prompt as context.
- `>3-7` — refer to lines 3 through 7. Inserts them as a block.
- `>3 hello` — refer to line 3 *and* add `hello` as a follow-up
  message. Equivalent to a quote-reply in chat apps.
- `>` with no number — just the prompt, no quote.

The model sees the quoted content inline with the user's message,
so it has the same context the user has. The transcript gets a
`UserTurn` event whose content includes the quoted block (with
line-number prefixes stripped, since they're a UI affordance not
content).

**Uncertainty.** Should the line numbers persist across REPL
scrollback, or reset per turn? Reset per turn is much simpler
(gutter always reads "1 to N" for the current turn's output) and
the model doesn't need persistent numbering — it sees the quoted
content directly. Reset-per-turn it is.

**Why not just use the transcript's own `N#XY` anchors?** Because
those are file-internal line numbers, not REPL line numbers. A
chat response has no file context. The REPL gutter is a separate
thing.

---

## 3. No way to stop a turn early in `gait chat`

**Issue.** `Ctrl+C` works in `gait run` to interrupt a turn (the
model is mid-stream, the user wants to abort). In `gait chat`, the
REPL renders the model output as it streams — there's no `Ctrl+C`
equivalent. If the user sees the agent going in the wrong
direction halfway through, the only options are: wait for the
turn to finish, or kill the REPL entirely (which loses session
state).

**Solution.** Add a key binding in the REPL that aborts the
in-progress turn. `Ctrl+C` is the obvious choice (matches `gait
run` and most CLIs), but REPLs historically use `Ctrl+C` to cancel
the current *input line*, not the running process. Two options:

- **Use `Ctrl+C` for the running turn** (steal the binding from
  input-line cancellation). Conflict with normal readline behavior
  — if the user is mid-typing a message, `Ctrl+C` will cancel the
  *typing*, not the running turn. Solution: `Ctrl+C` aborts the
  turn *only when the agent is streaming*; otherwise it falls
  through to readline's normal cancel-line.
- **Use a different key**, e.g. `Esc` twice (matches Claude Code's
  rewind menu) or `Ctrl+\` (sends SIGQUIT, traditionally "hard
  kill"). Less discoverable.

I'd pick option 1 with the conditional behavior. The state machine
is simple: if the REPL is rendering agent output, `Ctrl+C` is
"stop the turn." If the REPL is in the input prompt, `Ctrl+C` is
"cancel this line." A status indicator at the bottom of the REPL
(e.g. `[streaming — Ctrl+C to stop]`) makes the state visible.

The harness needs to support mid-stream cancellation. Currently
`run_turn` consumes the full stream before returning
(`turn.rs:435-450`). Adding a cancellation token is a real
refactor, not a one-liner. The fix should land as: (a) check the
token between `StreamChunk`s, (b) return a new `TurnResult` variant
like `TurnResult::Cancelled`, (c) commit a `turn_cancelled` event
to the transcript (so resume works), (d) the REPL gets a clean
"turn stopped, here's what was done so far" message.

---

## 4. "Add to allow list" is not sticking between turns

**Issue.** When the REPL prompts for a permission decision and the
user picks "always" (or whatever the equivalent verb is — based on
the README, the `--auto` flag and `gait.jsonc` `allow` list are the
intended mechanisms), the model reports that the *next* similar
tool call still prompts. The user expected "always" to mean
"always for this session" but it doesn't appear to be persisting
between prompts, or it's only persisting at the wrong scope.

This is a behavior bug, not a feature request. I haven't reproduced
it against current `main` (I was working from the prompt the
harness gave me, which had a pre-allowlist that didn't include
`shell` or other tools). The observation is from the user's hands-on
testing.

**Solution.** Two sub-fixes, depending on what the bug actually is:

1. **If "always" is being stored in-session but not surviving
   session resume:** the storage layer is the bug. Trace the
   allow-list mutation path: where is the "always" decision
   written? (Probably `gait-rs/src/chat.rs` in the
   `cmd_handle_permissions` function, around line 530-580 based on
   what I read.) Where is it read on resume? (Probably the
   `effective_allow` computation in `turn.rs:244`.) If the write
   doesn't survive a session boundary, fix the write.

2. **If "always" is being silently overridden by a deny list or
   permission tier:** the precedence is wrong. The current model
   (per `docs/superpowers/plans/2026-06-08-permission-overhaul.md`)
   is deny > allow > ask. If "always" is being interpreted as
   "add to allow list" but the user has a deny rule that also
   matches, the deny wins (correctly) but the user expected the
   allow to take effect. Fix: surface the conflict at the
   permission prompt time. *"This pattern is in your deny list,
   so 'always approve' won't help. To allow, edit your deny list
   first."*

**Reproduction plan.** To verify, the user needs to:
1. Start `gait chat` in a fresh session.
2. Trigger a tool call that prompts (e.g. `shell`).
3. Pick "always" / `[a]` / whatever the allow-list-add verb is.
4. Trigger the same tool call again.
5. Observe: does it prompt again, or run?

If it prompts again, this is a real bug. If it runs, the user's
observation was from an earlier state of the code and the fix is
already in.

**Note on uncertainty.** I don't have firsthand evidence this is
broken — the user's report is the only signal. The doc flags it
as a bug, but the fix section says "diagnose first" because there
are two distinct bugs it could be.

---

## 5. Session-scoped permissions (feature request, needs spec)

**Issue.** The current permission model has two scopes:

- **Workspace-level:** `gait.jsonc`'s `allow` / `deny` lists. These
  apply to *every* session in the workspace, forever.
- **Turn-level:** the user approves each tool call interactively, or
  the `--auto` flag approves everything.

There's no middle ground: "approve this pattern for *this* session
only." A user might want to allow `shell:ls` for one debugging
session, without committing that to the global `gait.jsonc` (where
it'd be a permanent security loosening) and without approving
`shell:ls` 30 times in a row.

**Solution.** Add a *session-scoped* allow list. Three possible
shapes:

- **In-REPL command:** `gait chat` reads `/allow shell:ls` (or
  similar) at the prompt. Adds the pattern to a per-session
  allow list that takes precedence over `gait.jsonc`'s allow but
  is discarded when the session ends. User can do
  `/allow shell:rm *` to broaden within the session; gets
  prompted again next session.
- **In-prompt option:** when the user is prompted for a
  permission decision, the menu adds a third option
  *"this session"* alongside *yes* / *no* / *always*. The
  semantics are the same as the `/allow` command but driven
  from the prompt.
- **Auto-derived from session context:** track which tools the
  user *manually* approved during a session. After N approvals
  of the same pattern (e.g. 3), offer to "promote to
  session-scoped" automatically. Less work for the user, but
  adds magic.

**Spec needed before implementing.** This is a feature, not a
bug, and the design space is larger than a one-line fix:

1. **Precedence.** How does session-scoped allow interact with
   workspace-scoped *deny*? My answer: deny still wins, always.
   Session-allow can only *loosen* within a session, never
   *override* a workspace-deny. (Same precedence as the
   three-tier model.)
2. **Persistence.** Where is the session allow list stored?
   Probably in the session directory, e.g.
   `sessions/<id>/allow.json`. That way it survives REPL exits
   within the same session, and is naturally discarded when
   `gait finish` cleans up the session.
3. **Visibility.** How does the user know what's been
   session-allowed? `/allow list` command, and the permission
   prompt should show "this is currently session-allowed" if
   applicable.
4. **CLI parity.** Does `gait run --auto` get the same
   session-scope behavior, or is this REPL-only? My answer:
   REPL-only for the first pass, with the
   `--session-allow <pattern>` flag for CLI users who want
   it.

**Why this is a spec request, not a doc request.** The
implementation has real choices (storage location, precedence
rules, UX of the prompt menu) that need to be made *before* a
plan can be written. This doc captures the user need and three
possible shapes; the actual design pass belongs in a separate
plan in `docs/superpowers/plans/`.

---

## Cross-cutting notes

- Items 1, 2, 3 are all REPL-UX issues. They could ship
  together as a "REPL ergonomics" pass. Item 4 is a bug
  investigation. Item 5 is a feature spec.
- The "session-scoped permissions" idea (item 5) is the
  highest-value, highest-effort item. The REPL items are
  smaller. I'd ship them in order: bug fix #4 first (it's a
  regression check), then 1, 2, 3 as a REPL polish pass, then
  spec out 5 properly.
- None of these depend on the `turn-terminator-redesign.md`
  work. The `summary` → `message` rename, the `output_schema`
  validation, etc. — orthogonal.
- All five items will need updates to the REPL's input parser
  (the `/foo` syntax is currently a single-thread thing in
  `chat.rs`). If the REPL grows more commands, consider
  grouping them in a `chat_commands.rs` module for
  discoverability.

## What I want to flag about this doc

- I haven't reproduced bug #4 myself. I included it because the
  user reported it, but the solution section says "diagnose
  first" rather than committing to a fix. If it turns out the
  bug is already fixed in current main, this whole section
  should be deleted.
- Items 1, 2, 3 have specific design opinions in them (the
  `/rewind` flag names, the `>3` reply syntax, the `Ctrl+C`
  conditional behavior). Those are starting points. If you
  want different ergonomics — e.g. `/undo` instead of
  `/rewind`, or `^3` instead of `>3` — that's a one-line edit.
- Item 5 is intentionally under-specified. I described the
  problem and three possible shapes; I did not pick one. That
  belongs in a separate spec doc where the trade-offs can be
  worked through properly.
- **Item 1 update (added after discussion):** I initially
  proposed a Claude-Code-style content-snapshot mechanism
  (file-content backups indexed by user-message UUID) for the
  rewind feature. The user pushed back: gait already requires
  git, the harness already commits per turn, so the natural
  primitive is just *commit more often*. The revised
  Solution section reflects that direction. The author will
  think through the design choices (commit frequency, branch
  layout, conversation-rewind integration) later — this doc
  captures the direction, not the final design.

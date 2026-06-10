# Bug: `[r]` reject in `gait chat` drops the user's reply

**Status:** Observed during testing, 2026-06-09. Not yet reproduced against
current `main` — based on code reading. See "Reproduction plan" below.
**Severity:** Medium. Loses user input silently; confusing UX.
**Affects:** `gait-rs/src/chat.rs` (the `cmd_handle_permissions` function).

## What happens

A user runs `gait chat`, the agent tries to call a tool that needs
permission, the harness prompts the user. The user types a reply
containing `[r]` (or any reject action). The harness prints
`[gait] permission denied` and returns to the prompt. The agent's
next turn has **no** user-turn text to react to. The chat REPL has
**no** assistant text to render.

## What the user sees

```
[gait] permission requested: shell
  cmd:   rm -rf build
  reason: cleanup
allow? [y/n/r/always]: n because that would delete the worktree
[gait] permission denied

>
```

The user types `n because that would delete the worktree`. The harness
treats `n` as reject (which is what `[r]` does too), prints
`[gait] permission denied`, and returns to the prompt. The agent never
sees the "because that would delete the worktree" part. The user might
reasonably think their reason was delivered, or that the agent now
knows not to try that. Neither is true.

If the user wanted to *explain* a rejection (e.g. "n, please use
`git clean -fdx` instead, that's safer"), there's no way to do that
in the current UI. Any text they type is consumed by the permission
parser and thrown away.

## Why it happens

From `gait-rs/src/chat.rs:594-600`:

```rust
if any_approved {
    continue 'spawn;
} else {
    eprintln!("[gait] permission denied");
    println!();
    break 'spawn;
}
```

The reply-collection loop (somewhere above this in `cmd_handle_permissions`)
parses the user's input into per-item `PermissionChoice::{Approve, Reject}`
decisions. The text the user typed is consumed in that parsing and not
preserved.

Even when `any_approved` is true (mixed approve/deny) the user's text
disappears — `continue 'spawn` re-enters the loop with the same
`Permissions` event still pending, and any user text from the *first*
reply is gone.

The `PermissionsReply` event written to the transcript carries
`text: None` in the deny case (`chat.rs:580`) and only sometimes
`text: Some("shell:rm *")` in the approve case. So even the *transcript*
loses what the user said.

## What the user expected

At minimum: when the user types text alongside their reject, the agent
should see it as a normal user-turn. The agent's next response can
acknowledge the rejection and react to the explanation.

Two reasonable fixes:

1. **Re-emit the rejected text as a `UserTurn` event before the
   `Permissions` event is finalized.** If the user typed text that
   wasn't consumed by the permission decision (i.e. trailing prose
   after `[r]`, or a reject reason), it becomes a free-form user
   turn that the model sees on the next iteration. The transcript
   gets a `UserTurn` event followed by a `PermissionsReply`.

2. **Treat `[r]` as a normal reply action that includes the rest of
   the line as a comment.** Same wire format as above, just a
   different parsing rule. `[r] please use git clean instead` →
   `PermissionsReply{ decision: "deny", text: Some("please use git clean instead") }`
   on the current `Permissions` event, plus a synthetic
   `UserTurn{ content: "please use git clean instead" }` for the
   model's context.

Fix #2 is the smaller change. Fix #1 is the more correct one
(it makes the user's text a first-class event).

## Reproduction plan

I haven't run the repro because the harness denied the `shell` tool
call when I tried to actually invoke `gait chat` (the agent in this
session doesn't have the codebase on its allow-list). To reproduce
manually:

1. `gait chat` in any codebase.
2. Ask the agent to do something that needs a tool requiring permission
   (e.g. shell).
3. When prompted, type `n because X` (or `[r] because X`).
4. Observe: `X` is dropped. The next prompt is empty. The transcript
   shows `PermissionsReply{ decision: "deny", text: None }` with no
   adjacent `UserTurn`.

If the transcript path is `sessions/<id>/transcript.jsonl`, look for
the `UserTurn` event before the `PermissionsReply`. With current code
there isn't one.

## Related observations

- The `Always` path (`[always]`) is a UX win for power users but has
  the same issue — any text after the directive is discarded.
- The fix in `gait-rs/src/turn.rs:303-315` (emit a paired
  `ToolCall`+`ToolResult("permission denied by user")` for denied
  calls) prevents the retry-loop bug but doesn't surface the user's
  reason. Same drop.
- This is one of multiple places where the harness treats user text
  as a control input rather than as a turn-ending message. A more
  general fix would be: *user text is always a `UserTurn` event*;
  control directives (`y`/`n`/`r`/`always`) are parsed *out of* the
  text and the remainder becomes the `UserTurn` content.

## Suggested next step

Open a follow-up plan in `docs/superpowers/plans/` for the permission
flow redesign. This bug is small enough to be a one-liner fix
(preserve the text in `PermissionsReply` and emit a sibling
`UserTurn`), but the broader question of "user text is a turn" is
worth its own design pass.

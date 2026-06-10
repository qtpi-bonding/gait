# gait turn-end test

This file exists to test how the agent harness ends a turn.

## What I'm trying to learn

In this single turn, I want to do all of:

1. Call `shell` to inspect the working directory.
2. Call `write` to create this file.
3. Write a plain-text summary back to the user.

If this works as I expect (tools → text → END), then the harness's
turn-end model is "no more tool calls + final text = done."
If it doesn't, we'll find out why.

## Hints for future-me

- I used `pkill` on the full `gait run` flow, not a sub-shell, so this
  turn had a real tool-call section followed by prose.
- I did NOT call the `output` tool. So this is the "plain text"
  termination path, not the structured-result path.
- I did NOT spawn a sub-agent. So I can't comment yet on whether
  spawn_agent returns control to me in the same turn or not.

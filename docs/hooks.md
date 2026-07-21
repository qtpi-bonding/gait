# Hooks

Hooks run a shell command at fixed points in the turn loop — for validation, logging, or content rewrites. They're configured in the `hooks` array of `gait.jsonc` or in an agent's frontmatter, and they run with no timeout (have them spawn their own background process for long-running work instead of blocking the hook).

## Shape

```jsonc
"hooks": [
  { "event": "post_tool", "tool": "shell", "run": "echo ran a shell tool" }
]
```

| Field | Meaning |
|---|---|
| `event` | One of the six lifecycle events below. |
| `tool` | Optional tool-name filter — only meaningful for `pre_tool` / `post_tool`; omit to match every tool. |
| `run` | Shell command, executed via `sh -c`. |

If the same `event`/`tool` combination appears in both `gait.jsonc` and the agent's frontmatter, the agent's entry runs after the workspace's.

## Events

Source of truth: `crates/gait-state/src/proto/events.rs::hook_phase`.

| Event | Fires | Effect |
|---|---|---|
| `pre_turn` | Once, after the system prompt is built, before the step loop starts. | Exit 0 with non-empty stdout **replaces the system prompt** for the turn. |
| `pre_step` | Before each LLM call. | Observation only — output has no effect on the step. |
| `post_step` | After the LLM response streams in, before its tool calls run. | Observation only. |
| `pre_tool` | Before each tool call (respecting the `tool` filter). | **Non-zero exit blocks the tool call** — its stdout+stderr is returned to the model as the block reason instead of running the tool. |
| `post_tool` | After a tool call executes. | Exit 0 with non-empty stdout **replaces the tool result**; non-zero exit appends the hook's output as an error annotation to the original result. |
| `post_turn` | Once, at the very end of the turn. | Observation only. |

Every hook run is recorded to the session log as a `hook_run` event (with phase, command, exit code, output, and a full env snapshot), so hook activity replays with the rest of the transcript.

## Environment passed to the hook command

Source of truth: `crates/gait-state/src/hook_runner.rs::build_env`. Every variable is set on every hook invocation — fields that don't apply are present as empty strings or `0`.

| Variable | Available on | Value |
|---|---|---|
| `GAIT_EVENT` | all | The event name (`pre_turn`, `pre_tool`, `post_tool`, ...). |
| `GAIT_SESSION_ID`, `GAIT_DIR`, `GAIT_CODEBASE` | all | Session id, gait-dir, codebase path. |
| `GAIT_SYSTEM_PROMPT` | from `pre_turn` onward | Current system prompt text. |
| `GAIT_STEP_INDEX`, `GAIT_STEP_COUNT`, `GAIT_MESSAGE_COUNT`, `GAIT_LAST_ROLE` | all | Turn-loop position. |
| `GAIT_RESPONSE_TEXT`, `GAIT_TOOL_COUNT`, `GAIT_TOOL_NAMES` | from `post_step` onward | The assistant's text and the tools it just called (comma-joined). |
| `GAIT_TOOL_NAME`, `GAIT_TOOL_CALL_ID` | `pre_tool`, `post_tool` | Which tool call this is. |
| `GAIT_ARG_<KEY>` | `pre_tool`, `post_tool` | One var per tool argument — key uppercased, `-`→`_` (e.g. a `file-path` argument becomes `GAIT_ARG_FILE_PATH`). Values are stringified JSON. |
| `GAIT_TOOL_RESULT` | `post_tool` | The tool's raw output, before any hook rewrite. |
| `GAIT_TURN_OUTCOME` | `post_step`, `post_turn` | `done` \| `clarify` \| `inform` \| `permissions` \| empty if still in progress. |
| `GAIT_FINAL_RESPONSE` | `post_turn` | The turn's final response text. |

## Examples

A "no `cargo fmt` failures allowed" pre-tool gate:

```jsonc
"hooks": [
  {
    "event": "pre_tool",
    "tool": "shell",
    "run": "echo \"$GAIT_ARG_FILE_PATH\" | grep -q '\\.rs$' || exit 0; cargo fmt --check -- \"$GAIT_ARG_FILE_PATH\" || { echo 'rustfmt failed'; exit 1; }"
  }
]
```

A pre-turn context injection (the hook's stdout replaces the system prompt):

```jsonc
"hooks": [
  {
    "event": "pre_turn",
    "run": "echo \"Today's date is $(date +%F). Working dir: $GAIT_CODEBASE.\""
  }
]
```

A post-tool that anonymizes a specific output before the model sees it:

```jsonc
"hooks": [
  {
    "event": "post_tool",
    "tool": "shell",
    "run": "sed -E 's/([0-9]{1,3}\\.){3}[0-9]{1,3}/[REDACTED-IP]/g' <<< \"$GAIT_TOOL_RESULT\""
  }
]
```

# Workflows

A workflow is a JavaScript file that runs model turns, shell commands and approval
gates in the order your code says. Every step is written to a run log, so a paused or
interrupted run can be continued.

Workflows need the full `gait` build. `gait-min` has no `workflow` command.

## A first workflow

Scripts live in `<gait-dir>/workflows/<name>.js`:

```js
// @ts-check
await gait.wf.config({ name: "fix-and-check" });

await gait.wf.turn({ id: "fix", prompt: "Fix the failing test.", auto: true });

const test = await gait.wf.shell({
  id: "test",
  script: 'cd "$CODEBASE" && cargo test',
  timeout: 600000,
});

if (test.exit !== 0) {
  await gait.wf.turn({
    id: "fix-again",
    prompt: `The tests still fail:\n${test.output}`,
    auto: true,
  });
}

await gait.wf.gate({ id: "review", message: "Review the changes, then continue." });
```

```bash
gait workflow run fix-and-check --gait-dir <path>
```

The script body runs inside an async function, so top-level `await` and `return` work.
Branches, loops and parallel steps are ordinary JavaScript (`if`, `for`,
`Promise.all`). The script has no filesystem, network or process access of its own:
everything it does outside the script goes through `gait.wf`.

## Commands

```bash
gait workflow run <name> [args...]        # run <gait-dir>/workflows/<name>.js
gait workflow continue <run-id>           # resume a paused or interrupted run
gait workflow list                        # runs and their status
gait workflow log <run-id>                # one JSON object per logged event
gait workflow abort <run-id>              # mark a run failed
gait workflow types                       # write workflows/gait.d.ts
```

In the REPL, `/workflow <name> [args...]` runs a workflow.

`run` and `continue` exit 0 when the run completes, 2 when it pauses at a gate, and 0
when it pauses at a turn without a prompt. `list` and `log` only see workflow runs,
never ordinary tasks or REPL turns.

## The script API

`gait workflow types` writes TypeScript declarations for everything below into
`<gait-dir>/workflows/gait.d.ts`. Add `// @ts-check` at the top of a script to have
your editor check it.

`gait.wf.run` describes the current run: `runId`, `args` (the words after the workflow
name), `codebase` and `workspace` (the gait directory).

### `config`

```js
await gait.wf.config({
  name: "release",            // required
  description: "Cut a release",
  model: "fast",              // default for turns
  auto: false,                // default for turns
  allow: ["read:*"],          // default for turns
  maxSteps: 50,               // default 1000
  record: { commands: [{ id: "tests", command: "make test", timeout: 600000 }] },
});
```

Call it first, await it, and call it only once.

`record` runs each command once when the run starts and writes the results, with
fingerprints of the run worktree at the start and end, to
`workflow-runs/<run-id>/record.json`. The timeout is in milliseconds (default 120000).

### Step ids

`turn`, `shell` and `gate` each take an `id`. It must be unique within the run and
stable from one execution to the next, because `continue` matches logged results to
calls by id. In a loop, put the counter in the id (`` `fix-${i}` ``). Ids starting
with `__now_` or `__random_` are reserved.

### `turn`

One gait turn: its own session, log and permission checks.

| Field | Meaning |
|---|---|
| `id` | Step id (required) |
| `prompt` | What to ask. See below for a turn without one |
| `model` | Name of a gait inference (an entry under `inferences/`), not a provider model string. An unknown name fails the step |
| `auto` | Auto-approve this turn |
| `allow` | Permission patterns; when empty, the `config` list applies |
| `agent` | Agent to run as |
| `codebase` | Directory to work in; a relative path is resolved against the run's codebase |
| `continueSession` | Session id to continue instead of starting a new one |
| `outputSchema` | JSON Schema for a structured result, returned as `json` |
| `retry`, `onFail`, `hooks` | See [Failures](#failures-retries-and-hooks) |

A turn without a `prompt` pauses the run. Gait does not yet have a way to give such a
step its input, so `continue` pauses at the same step again. Use `gate` to wait for a
person.

### `shell`

```js
await gait.wf.shell({ id: "status", argv: ["git", "-C", gait.wf.run.codebase, "status"] });
await gait.wf.shell({ id: "build", script: 'cd "$CODEBASE" && make' });
```

Give exactly one of `argv` (run directly) or `script` (run with `sh -c`). Other fields:
`env`, `timeout` (milliseconds, default 120000), `outputSchema` (the command's stdout
must be JSON matching it), `retry`, `onFail`, `hooks`.

The working directory is a private worktree belonging to the run,
`workflow-runs/<run-id>/worktree`, **not** your codebase. Use `"$CODEBASE"` to work on
the code.

A non-zero exit status is a result, not a failure: check `exit` yourself.

Shell steps, hooks and record commands run as ordinary processes of your user, with no
permission prompt and no sandbox. Read a script before you run it.

Their environment is cleared. Only `PATH`, `HOME`, `SSH_AUTH_SOCK`, `GH_TOKEN`,
`CARGO_HOME`, `RUSTUP_HOME`, `TMPDIR` and `LANG` are passed through. Steps also get
`WORKFLOW_ID`, `SESSION_ID`, `NODE_ID`, `CODEBASE`, `WORKSPACE` and `ARGS`; hooks get
`FAILED_NODE` as well. A step's `env` cannot override these or set `GAIT_*` names.

To pass a value into a command safely, use the `sh` template. Values travel as
environment variables and are never pasted into the command text:

```js
const branch = gait.wf.run.args[0];
await gait.wf.shell({
  id: "checkout",
  ...gait.wf.sh`cd "$CODEBASE" && git checkout ${branch}`,
});
```

### Results

`turn` and `shell` resolve to:

| Field | Meaning |
|---|---|
| `output` | The turn's reply, or the command's stdout followed by its stderr |
| `exit` | Exit status, or `null` |
| `json` | The structured result when `outputSchema` was given |
| `sessionId` | The turn's session, or `null` |
| `error` | Set when the step failed and the script was allowed to go on |

### `gate`

```js
await gait.wf.gate({ id: "ship", message: "OK to ship?" });
```

The run pauses with exit status 2. `gait workflow continue <run-id>` passes the gate
and the call resolves to `null`.

### `now` and `random`

`Date.now()`, `new Date()` without arguments and `Math.random()` throw inside a
workflow, because a continued run must see the same values. Use
`await gait.wf.now()` (Unix time in milliseconds) and `await gait.wf.random()`.

## Failures, retries and hooks

A step fails when its turn errors, when its command cannot run or times out, when
`outputSchema` does not match, or when a `before` hook exits non-zero.

- `retry: { times, delay, backoff }` tries the turn or command again up to `times` more
  times, waiting `delay` milliseconds between tries. `backoff` is `"linear"` (the same
  wait each time, the default) or `"exponential"` (doubling).
- `onFail: "throw"` (the default) throws in the script. Uncaught, it fails the run.
- `onFail: "continue"` resolves the step with `error` set.
- `hooks: { before, after, error }` are shell commands run around the step.

Await every step. A script that ends with a step still running, or with an unhandled
promise rejection, fails the run.

## Continuing a run

`gait workflow continue <run-id>` runs the script again from the top. A step that
already finished returns its logged result without running again. A step that had
failed returns a result with `error` set and is not run again. Steps with no logged
outcome run live.

The script that runs is the one saved in the run log. If the file on disk has changed,
`continue` refuses; `--force` resumes the saved script anyway. Edits take effect only
in a new run.

If gait was interrupted while a step was running, that step may have partly or fully
happened, so `continue` refuses and names it. `--force-rerun <step-id>` (repeatable)
accepts running it again. Effects it already had are **not** undone.

## Limits

| Limit | Value |
|---|---|
| Script file size | 1 MiB |
| Script CPU time (waiting for steps does not count) | 60 s |
| Script memory | 256 MiB |
| Script output | 8 MiB |
| Steps per run | `maxSteps`, default 1000 |

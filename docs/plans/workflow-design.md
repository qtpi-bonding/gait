# gait workflow spec

> Implementation target: Rust port. Do not implement in TypeScript.

---

## Primitives

Five node types. Everything composes from these.

| Node | Purpose |
|------|---------|
| `prompt` | AI turn — full gait toolset available (clarify, inform, spawn_agent, etc.) |
| `shell` | Deterministic bash, no AI, stdout → `$id.output`, exit → `$id.exit` |
| `loop` | Repeat a prompt until a signal string or shell exit-0 |
| `branch` | 1→N routing on an expression — creates a git worktree per active path |
| `gate` | HITL pause — workflow waits for `gait workflow continue <id>` |

---

## Execution Model

### DAG

Nodes form a DAG via `depends_on`. The runner executes nodes in topological order. Independent nodes (no dependency between them) run concurrently. v1 may be sequential; concurrency is additive.

### State persistence

All workflow state lives in the transcript (JSONL). New event types:

| Event | When |
|-------|------|
| `workflow_start` | Workflow begins — records id, yaml path, args |
| `node_start` | Node begins — records node id, type, attempt number |
| `node_complete` | Node succeeds — records output, exit code, routed_to (branch) |
| `node_skip` | Node skipped (inactive branch path) |
| `node_error` | Node fails — records error, attempt number |
| `hook_run` | Any hook fires — records hook type, command, exit |
| `workflow_complete` | Workflow done |
| `workflow_error` | Workflow failed — records failed node |

On resume (after `gate`, crash, or `gait workflow continue`): runner reads transcript, reconstructs completed nodes and their outputs, resumes from the first incomplete node.

### Branch execution and git worktrees

When a `branch` node fires:

1. Evaluate the expression
2. Create a git worktree of the **workspace repo** (not the codebase) at `sessions/<workflow-id>/<branch-node-id>/`, branched from the **current HEAD** of the workflow session (not from `main`)
3. Run the active path in that worktree
4. On completion, merge the path branch back into the main session branch (fast-forward — main hasn't moved)
5. Delete the worktree

```
session/wf-abc (main workflow branch)
  ├── commit: workflow_start
  ├── commit: node test
  ├── commit: node route (branch → fix-loop)
  │   └── session/wf-abc/fix-loop  (worktree, branched from here)
  │       ├── commit: loop iter 1
  │       ├── commit: loop iter 2
  │       └── commit: loop iter 3 (DONE)
  ├── commit: merge fix-loop  (fast-forward)
  ├── commit: node ship
  └── commit: workflow_complete
```

`depends_on: [branch_id]` on downstream nodes resolves to whichever path the branch activated. Inactive paths are written to the transcript as `node_skip` events.

### Workspace vs codebase

- **Workspace** (`gait.jsonc`, `sessions/`, `workflows/`) — gait manages this git repo. Branch worktrees are worktrees of this repo.
- **Codebase** — user's code directory. gait reads it (hash, dirty status) but does not manage its git state. What agents do to the codebase is the user's responsibility.

Parallel branch paths sharing a codebase will conflict if both write code simultaneously. This is user error — design workflows accordingly or use `shell` nodes to set up codebase worktrees manually (e.g. `git -C $CODEBASE worktree add /tmp/variant-a`).

---

## Full Schema

```yaml
# ── Workflow header ──────────────────────────────────────────────────────────

name: fix-and-ship
description: "Run tests, fix failures, get human review, ship PR"
version: "1"

# Default model for all prompt/loop nodes (overridable per-node)
model: minimax/MiniMax-M3

# Default LLM config (all overridable per-node)
allow: [read, write, edit, shell, glob, grep]
auto: false

# Workflow-level hooks — fire around the entire workflow run
hooks:
  before: "ntfy publish my-channel 'workflow $WORKFLOW_ID starting'"
  after:  "ntfy publish my-channel 'workflow $WORKFLOW_ID done'"
  error:  "ntfy publish my-channel 'workflow $WORKFLOW_ID failed at $FAILED_NODE'"

# ── Nodes ────────────────────────────────────────────────────────────────────

nodes:

  # ── shell ──────────────────────────────────────────────────────────────────
  - id: test
    shell:
      run: "bun test 2>&1"
      cwd: codebase              # workspace | codebase (default: codebase)
      timeout: 60000
    on_error: continue
    retry:
      max: 2
      delay: 1000
      backoff: linear            # linear | exponential
    hooks:
      before: "echo 'running tests'"
      after:  "echo 'tests exited $NODE_EXIT'"
      error:  "ntfy publish my-channel 'tests errored'"

  # ── branch ─────────────────────────────────────────────────────────────────
  - id: route
    branch:
      on: "$test.exit == 0"
      then: ship
      else: fix-loop
    depends_on: [test]

  # ── loop ───────────────────────────────────────────────────────────────────
  - id: fix-loop
    loop:
      prompt: |
        Tests are failing:
        $test.output

        Fix the failures. Signal DONE when all tests pass.
      until: "DONE"
      shell_check: "cd $CODEBASE && bun test 2>&1"
      max: 10
      fresh_context: false
      hooks:                     # per-iteration (fires every iteration)
        before: "echo 'iter $LOOP_ITER / $LOOP_MAX'"
        after:  "echo 'iter $LOOP_ITER: $LOOP_SIGNAL'"
        error:  "echo 'iter $LOOP_ITER failed'"
    # node-level hooks fire once around the whole loop
    model: anthropic/claude-opus-4-6
    auto: true
    on_error: goto: give-up
    hooks:
      before: "echo 'fix loop starting'"
      after:  "ntfy publish my-channel 'fixed after $LOOP_ITER iterations'"
      error:  "ntfy publish my-channel 'fix loop gave up at iter $LOOP_ITER'"

  # ── gate ───────────────────────────────────────────────────────────────────
  - id: review
    gate:
      message: |
        Fix complete ($LOOP_ITER iterations).
        Review: $WORKSPACE/sessions/$SESSION_ID
        Continue: gait workflow continue $WORKFLOW_ID
    depends_on: [fix-loop]
    hooks:
      before: "ntfy publish my-channel 'workflow $WORKFLOW_ID needs review'"

  # ── prompt ─────────────────────────────────────────────────────────────────
  - id: ship
    prompt: "Create a PR for the changes. Include a clear description."
    model: anthropic/claude-sonnet-4-6
    output_schema:
      type: object
      properties:
        pr_url:  { type: string }
        summary: { type: string }
    depends_on: [route]          # resolves to whichever branch path completed
    hooks:
      after: "ntfy publish my-channel 'PR ready: $ship.output.pr_url'"
      error: "ntfy publish my-channel 'PR creation failed'"

  # ── goto target (cleanup / escalation) ─────────────────────────────────────
  - id: give-up
    gate:
      message: "Fix loop exhausted after $LOOP_ITER iterations. Manual fix required."
    hooks:
      before: "ntfy publish my-channel 'workflow $WORKFLOW_ID needs manual fix'"
```

---

## Node Base Fields

Every node type accepts these:

```yaml
id: string                    # required, unique within workflow
depends_on: [node_id, ...]    # upstream deps (omit = no deps = runs first)
on_error: abort               # abort (default) | continue | goto: <node_id>
retry:
  max: 3
  delay: 1000                 # ms between attempts
  backoff: linear             # linear | exponential
hooks:
  before: "shell command"     # runs before node executes
  after:  "shell command"     # runs after node succeeds
  error:  "shell command"     # runs after node fails (retries exhausted)
```

Hooks are fire-and-forget shell commands. Exit code is logged but does not affect the workflow.

---

## Node Types

### `shell`

```yaml
shell:
  run: string        # bash command
  cwd: codebase      # workspace | codebase (default: codebase)
  timeout: 120000    # ms, default 2 min
```

Shorthand: `shell: "bun test 2>&1"` → `shell: { run: "bun test 2>&1" }`.

Captures stdout+stderr as `$id.output`. Exit code as `$id.exit`.

### `prompt`

```yaml
prompt: string              # task string, $var substitution applied

# LLM config (all optional — resolution order: node → workflow → gait.jsonc → defaults)
model: string               # model override
codebase: string            # codebase path override (supports $var, e.g. "$setup.output.a")
allow: [tool, ...]          # tool allow list override
auto: bool                  # auto-approve all permissions (default false)
agent: string               # load agents/<name>.md as system prompt
system_prompt: string       # inline system prompt (overrides agent)
files: [path, ...]          # context files injected at turn start (relative to codebase)
output_schema:              # optional — forces structured output() tool call
  type: object
  properties: { ... }
```

Shorthand: `prompt: "do the thing"`.

### `loop`

```yaml
loop:
  prompt: string            # task per iteration, $var substitution applied
  until: string             # completion signal string the model emits
  shell_check: string       # optional bash command — exit 0 = done
  max: 10                   # hard ceiling, exceeding = node error
  fresh_context: false      # true = new LLM session each iteration
  hooks:                    # per-iteration hooks (in addition to node-level hooks)
    before: "..."           # fires before each iteration
    after:  "..."           # fires after each successful iteration
    error:  "..."           # fires after a failed iteration

# LLM config — same fields as prompt, applies to all iterations
model: string
codebase: string
allow: [tool, ...]
auto: bool
agent: string
system_prompt: string
files: [path, ...]
output_schema: { ... }
```

Completion trigger: whichever fires first — `until` signal in model output OR `shell_check` exit 0.

Loop node-level `hooks.after` fires once after the loop completes. Per-iteration `loop.hooks.after` fires after each individual iteration.

### `branch`

```yaml
branch:
  on: string                # expression evaluated against completed node outputs
  then: node_id             # active path if truthy
  else: node_id             # active path if falsy
  # OR multi-way:
  cases:
    value_a: node_id
    value_b: node_id
  else: node_id             # required fallback for multi-way
```

Creates a workspace git worktree for the active path, branched from the current session HEAD. Merges back on completion. Inactive paths are skipped (recorded as `node_skip` in transcript).

Downstream nodes use `depends_on: [branch_id]` — the runner resolves this to the completed path's last node.

`branch` nodes have no LLM config (they do no AI work themselves).

### `gate`

```yaml
gate:
  message: string           # printed to terminal, $var substitution applied
```

Shorthand: `gate: "message string"`.

Pauses the workflow. Resumes on `gait workflow continue <id>`. Use `hooks.before` for notifications.

---

## Error Handling

| `on_error` | Behaviour |
|------------|-----------|
| `abort` (default) | Stop workflow, fire workflow `error` hook, mark run failed |
| `continue` | Mark node skipped, continue to next node |
| `goto: node_id` | Jump to a specific node — use for cleanup, rollback, or escalation |

Node `error` hook fires in all cases before the `on_error` decision. Workflow `error` hook fires only on `abort`.

---

## Variables

Available in hooks, `gate.message`, `prompt`, `loop.prompt`, and `shell.run`:

| Variable | Value |
|----------|-------|
| `$WORKFLOW_ID` | Current workflow run id |
| `$SESSION_ID` | Current gait session id |
| `$NODE_ID` | Current node id |
| `$NODE_EXIT` | Exit code (shell nodes, in hooks) |
| `$LOOP_ITER` | Current iteration, 1-based (loop nodes) |
| `$LOOP_MAX` | Max iterations configured |
| `$LOOP_OUTPUT` | Last iteration's text output |
| `$LOOP_SIGNAL` | Completion signal string if found, else empty |
| `$FAILED_NODE` | Id of failed node (workflow `error` hook only) |
| `$WORKSPACE` | Workspace path |
| `$CODEBASE` | Resolved codebase path for this node |
| `$ARGS` | Arguments passed at invocation |
| `$node_id.output` | Captured stdout of any completed shell/loop node |
| `$node_id.exit` | Exit code of any completed shell node |
| `$node_id.output.field` | Field from structured output of a prompt/loop node |

---

## Invocation

```bash
# Run a workflow
gait workflow run <name> [args...]

# List workflow runs (running, paused, completed)
gait workflow list

# Continue a paused workflow (after gate or permissions prompt)
gait workflow continue <workflow-id>

# Abort a running or paused workflow
gait workflow abort <workflow-id>

# Show transcript of a workflow run
gait workflow log <workflow-id>
```

Workflow files live in `<workspace>/workflows/*.yaml`, discovered automatically.

---

## Progress Tracking

Each workflow run maintains a `progress.yaml` in the session directory. It is a copy of the workflow YAML with a status comment appended to each node id line. Updated atomically in the same git commit as the transcript event — no external state, no separate database.

```yaml
nodes:
  - id: test
    status: done
    exit: 0
    shell: ...

  - id: route
    status: done
    routed_to: fix-loop
    branch: ...

  - id: fix-loop
    status: running
    iter: 3
    max: 10
    loop: ...

  - id: review
    status: pending
    gate: ...

  - id: ship
    status: pending
    prompt: ...
```

Status values: `pending` | `running` | `done` | `skipped` | `error`

Runtime fields added per node type:
- `shell`: `exit` (exit code)
- `loop`: `iter` (current iteration), `max` (limit)
- `branch`: `routed_to` (active path node id)
- `gate`: `paused_at` (ISO timestamp)
- any: `error` (message, on `error` status)

- `cat sessions/<id>/progress.yaml` — instant human-readable status
- `gait workflow status <id>` — reads this file directly (no JSONL parsing required)
- `git log sessions/<id>/progress.yaml` — history of node transitions as git commits
- `git diff` on the session branch — each commit shows exactly which node advanced

The source YAML in `workflows/` is never modified.

---

## Git / Session Model

- Each workflow run is a single gait session on branch `session/<id>`
- Every node transition commits both a transcript event AND an updated `progress.yaml`
- `branch` nodes spawn sub-branches from current HEAD, merge back on completion
- `gait finish <session-id>` merges the completed workflow into `main`
- Resume after any interruption by reading `progress.yaml` (fast path) or the transcript (full reconstruction)

---

## Notes

- **Parallel codebase worktrees**: if branch paths need isolated codebase state, use a `shell` node before the branch to set up worktrees (`git -C $CODEBASE worktree add ...`) and pass paths via `$node.output`. gait is agnostic to the codebase's git state.
- **HITL inside prompt nodes**: `clarify` and `inform` tools work within `prompt` nodes for turn-level human interaction. `gate` is for workflow-level pauses between nodes.
- **Structured output**: `output_schema` on prompt/loop nodes forces the model to call `output({ summary, result })` as its final action. Result available as `$node_id.output.field`.

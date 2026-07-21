# Workflows

Workflows are YAML DAGs — graphs of nodes that execute in dependency order. Each `prompt` node is a full gait turn with its own session; each `shell` node is one OS subprocess; `branch` nodes route; `gate` nodes pause the workflow for human review. State is reconstructed from the run log, so workflows survive interruptions and resumes.

The runner is `crates/gait-workflow/src/runner.rs`. The graph shape and node-type definitions are in `crates/gait-workflow/src/schema.rs`. The full audit and the design history are in `gait-rs/docs/superpowers/specs/`.

## Defining a workflow

Save a `.yaml` file in `workflows/` inside your gait-dir and run it with `gait workflow run <name>`.

```yaml
# workflows/ci.yaml
name: ci-pipeline
description: run tests, branch on result
model: anthropic/claude-sonnet-4-6
auto: true
allow: [read, shell]

hooks:
  before: "echo starting $WORKFLOW_ID"
  after:  "echo done"
  error:  "echo failed at $FAILED_NODE"
  timeout_ms: 120000       # per-hook timeout (kill_on_drop); default 120s

nodes:
  - id: test
    shell:
      run: "cargo test 2>&1"
      cwd: codebase
      timeout: 120000

  - id: route
    branch:
      on: "$test.exit == 0"
      then: ship
      else: fix
    depends_on: [test]

  - id: ship
    prompt: "Tests passed. Output:\n$test.output\nOpen a PR with summary: $test.output"
    depends_on: [route]

  - id: fix
    prompt: "Tests failed. Output:\n$test.output\nFix the failures and commit."
    on_error:
      retry_times: 3
      delay: 1000
      backoff: linear            # linear | exponential
      context: carry             # carry (keep context) | fresh (new session)
      goto: test                 # loop back to re-run tests after fixing
    depends_on: [route]

  - id: review-gate
    gate: "Review the changes above, then run: gait workflow continue $WORKFLOW_ID"
    depends_on: [ship]
```

### Workflow-level fields

| Field | Default | Effect |
|---|---|---|
| `name` | required | Used for run-log naming. |
| `description` | `""` | Free text. |
| `version` | `"1"` | Reserved for future schema migration. |
| `model` | inherits from `gait.jsonc` | The model for every `prompt` node unless overridden on the node. |
| `allow` | inherits from `gait.jsonc` | Pre-approve tool patterns workflow-wide. |
| `auto` | inherits from `gait.jsonc` | If `true`, all `prompt` nodes auto-approve permission requests. |
| `hooks` | none | Workflow-scoped `before`/`after`/`error` commands run at the right point. |

## Node types

Source of truth: `crates/gait-workflow/src/schema.rs::NodeDef`.

### `shell` — run a shell command

```yaml
- id: test
  shell: "cargo test 2>&1"          # short form
- id: deploy
  shell:                            # full form
    run: "scripts/deploy.sh"
    cwd: codebase                   # "codebase" (default) or "workspace"
    timeout: 120000                 # ms; default 120s; kill_on_drop
```

Captures stdout+stderr and exit code. Output is exposed to downstream nodes as `$test.output`. Exit code as `$test.exit`.

A `shell` node enrolled in the scheduler is **not** in the kernel cascade group (prompt nodes are). The runner enforces `kill_on_drop` so a timeout or sibling failure doesn't leak an orphaned process.

### `prompt` — run one LLM turn

```yaml
- id: ship
  prompt: "Tests passed. Open a PR with summary: $test.output"
  model: anthropic/claude-sonnet-4-6    # override workflow-level model
  auto: true                            # override workflow-level auto-approve
  allow: [read, shell]                  # override workflow-level allow
  agent: my-agent                      # load system prompt from agents/my-agent.md
  system_prompt: "You are..."           # literal system-prompt override
  codebase: /path/to/repo              # override gait-dir-relative codebase
  files: [src/foo.rs, src/bar.rs]      # seed the agent's context with these files
  continue_session: other-node-id      # continue a prior node's session instead of starting fresh
  output_schema:                        # force structured JSON output
    type: object
    properties:
      pr_url: { type: string }
```

Each `prompt` node is a complete gait turn — full session, full log, full sandbox.

### `branch` — route on a condition

```yaml
- id: route
  branch:
    on: "$test.exit == 0"
    then: ship
    else: fix
```

`on` is a boolean expression over `$node.<field>` references. The node evaluates it at runtime; inactive paths are skipped. A multi-way form:

```yaml
- id: route
  branch:
    on: "$lint.score"           # the expression — must evaluate to a key
    cases:
      "0":    ship
      "1-2":  fix-minor
      "3-5":  fix-major
    else: abort
```

`eval_bool_expr` matches against the result; if no case matches and no `else` is set, the branch node fails. An unresolvable `$node.*` reference in a branch condition now fails the branch (previously it silently fell through to the `then` arm — fixed).

### `gate` — pause for human review

```yaml
- id: review-gate
  gate: "Review the changes above, then run: gait workflow continue $WORKFLOW_ID"
```

```yaml
- id: review-gate
  gate:
    message: "..."   # same as short form
```

A gate is an unconditional pause — it doesn't evaluate any condition, it just stops the run and prints the message. Resume with `gait workflow continue <run-id>`. Exit code on pause: 2.

## Variable substitution

Inside any string field, these variables are substituted at runtime. Source: `crates/gait-workflow/src/vars.rs`.

| Variable | Value |
|---|---|
| `$WORKFLOW_ID`, `$SESSION_ID` | Current workflow run id. |
| `$WORKSPACE` | Gait-dir path. |
| `$CODEBASE` | Codebase path. |
| `$ARGS` | Arguments passed to `gait workflow run`. |
| `$NODE_ID` | Current node id. |
| `$NODE_EXIT` | Exit code of the *current* node (shell nodes). |
| `$FAILED_NODE` | ID of the failed node (error hooks only). |
| `$node.output` | Text output of node `node`. |
| `$node.output.<field>` | JSON field from node `node`'s structured output. |
| `$node.exit` | Exit code of a *named* node `node` (shell nodes). |

`$node.*` references in `shell` commands are POSIX single-quote-wrapped before splicing (so an LLM's freeform output can't break out of the shell). Well-known vars (`$CODEBASE`, `$WORKSPACE`, etc.) are gait-controlled, never untrusted, and stay unquoted.

## Node options

Every node supports:

```yaml
- id: my-node
  depends_on: [other-node]    # wait for these nodes first
  goto: next-node             # explicit next node after success
  on_error:                   # failure handling
    retry_times: 3            # retry up to N times before the goto/abort applies
    delay: 1000               # ms between retries
    backoff: linear           # linear | exponential
    context: carry            # carry (keep context) | fresh (new session)
    goto: error-handler       # abort (default) | continue | <node-id>
  hooks:
    before: "echo starting $NODE_ID"
    after:  "echo done"
    error:  "echo failed"
```

`on_error.goto` accepts three shapes:

- `abort` — fail the workflow (default).
- `continue` — fail this node but let downstream nodes (which don't depend on it) continue.
- `<node-id>` — recover by routing to a named node; together with `retry_times`, this is how loops work.

## Loops

There is no separate loop node type. Loops are graph cycles — point a node's `goto` (or `on_error.goto`) back to an earlier node. The graph validator does not reject `goto`/`branch` cycles (those are intentional), but it does reject `depends_on` cycles — those would deadlock the scheduler.

## Resume after gate or crash

Workflow state is fully reconstructed from the run log. Three exit modes:

```bash
gait workflow continue <run-id> --gait-dir <path>                    # resume a paused/gated run
gait workflow continue <run-id> --force --gait-dir <path>            # resume the ORIGINAL graph, ignoring on-disk YAML edits
gait workflow continue <run-id> --force-rerun <node-id> --gait-dir <path>  # accept re-running a shell node left "Running" by a crash
```

`--force` is meaningful when you've edited the YAML since the run started. Without it, drift between the on-disk YAML and the originally logged graph is a hard error (never a silent divergence).

`--force-rerun` is the answer to a `SandboxError: <node-id> was still running`. Side effects that node already ran before the crash are **not** undone — this is detect-and-refuse, not detect-and-undo.

## Validation

```bash
gait workflow validate <name> --gait-dir <path>
```

Runs the graph validator (duplicate node ids, dangling `depends_on`/`goto`/`branch`/`on_error.goto` references, `depends_on` cycles) without creating a run, touching the worktree, or writing to the run log. Exits 0 on a valid graph, 1 with a list of every problem found on an invalid one.

Source: `crates/gait-workflow/src/validate.rs`.

## What graphs are NOT validated

- **Branch-condition semantics.** Branch conditions are evaluated at runtime, not at load time. An unresolvable `$node.*` reference in a branch `on:` expression fails the branch node at runtime (not a silent truthy fall-through — that was the historical bug).
- **`goto`/`branch` cycles.** These are intentional — the scheduler supports them (`runner.rs` has explicit comments to that effect). Only `depends_on` cycles are rejected.
- **Per-tool floor / sandbox config compatibility.** The sandbox check happens at hand execution time, not graph-load time.

## Subagents

The `prompt` node is itself a spawned child session. Inside a `prompt` node, the agent can use the `spawn_agent` builtin tool to delegate work to a fresh grandchild session, with its own branch and log.

When a child (`spawn_agent`) or grandchild (workflow → prompt → spawn_agent) session completes, it writes an `agent_update` event to the parent's transcript, which the parent picks up on its next turn.

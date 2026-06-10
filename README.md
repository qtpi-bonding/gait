# gait

Turn-based AI coding agent. No daemon, no server, no UI. State lives in append-only JSONL transcripts committed to git after every turn.

The name is a backronym: **g**AI**t**. Also just a word for the way something moves.

---

## Install

```bash
# Rust (recommended)
cd gait-rs
cargo build --release --target-dir /your/cargo/target
cp /your/cargo/target/release/gait /usr/local/bin/gait
```

---

## Quickstart

```bash
# 1. Initialize a workspace
mkdir my-workspace && cd my-workspace
gait init

# 2. Add an API key
gait auth add anthropic

# 3. Run a task
gait run --codebase /path/to/your/repo --task "add a health check endpoint to src/server.ts"

# 4. Continue the same session
gait run --session s1a2b3c4

# 5. Non-interactive (auto-approve all permissions)
gait run --task "run the test suite and fix any failures" --auto
```

---

## Commands

### `gait init [<repo-url>]`

Sets up the current directory as a gait workspace:

- `gait.jsonc` — config
- `sessions/` — per-session transcripts and metadata
- `AGENTS.md` — system prompt for the agent (edit to add project conventions)
- `.gitignore` — excludes lock files and sessions from workspace commits

Optionally takes a repo URL to add as a `codebase/` git submodule. Without one, point at a codebase via `gait run --codebase`.

### `gait run`

Runs one turn of the agent.

```
gait run [--session <id>] [--task "..."] [--workspace <path>] [--codebase <path>]
         [--auto] [--model <provider/model>] [--container]
```

| Flag | Description |
|---|---|
| `--session <id>` | Resume a session. Auto-generated if omitted. |
| `--task "..."` | User message for this turn. Omit to continue a paused session. |
| `--workspace <path>` | Workspace directory. Defaults to cwd. |
| `--codebase <path>` | Codebase directory. Defaults to `<workspace>/codebase`. |
| `--model <id>` | Override the model for this turn. |
| `--auto` | Auto-approve all permission requests (non-interactive). |
| `--container` | Run the turn inside Docker (pulls the gait image if needed). |

Each completed turn is committed to the session's git branch. Interrupted turns (permissions, questions) are also committed — state is never lost.

### `gait workflow`

Run multi-step workflows defined as YAML DAGs. See [Workflows](#workflows) below.

```
gait workflow run <name> [args...]      # run workflows/<name>.yaml
gait workflow list                      # show active workflow sessions
gait workflow continue <workflow-id>    # resume a paused workflow (after a gate)
gait workflow abort <workflow-id>       # mark a workflow as aborted
gait workflow log <workflow-id>         # print the full transcript as JSONL
```

### `gait finish <session-id>`

Merges a session branch into `main` and removes the worktree.

```bash
gait finish s1a2b3c4 --workspace /path/to/workspace
```

### `gait undo <session-id>`

Restores a session to a past turn — code, transcript, or both.

```
gait undo <session-id> [--turn N | --list] [--code] [--transcript] [--workspace <path>]
```

| Flag | Description |
|---|---|
| `--list` | Print all turns with their checkpoint SHAs. |
| `--turn N` | Target turn number. Required with `--code` or `--transcript`. |
| `--code` | Restore the codebase working tree to what it was after turn N. |
| `--transcript` | Truncate the transcript back to turn N (removes later turns from git history). |

Omitting both `--code` and `--transcript` restores both. The codebase's own git history is never touched — only the shadow checkpoint is used.

### `gait chat`

Interactive REPL for conversational use. Starts a new session (or resumes one) and lets you iterate turn-by-turn without invoking `gait run` each time.

```bash
gait chat [--workspace <path>] [--codebase <path>] [--agent <name>]
```

**REPL commands and syntax**

| Input | What it does |
|---|---|
| `/n` or `/new` | Start a fresh session. |
| `/q` or `/exit` | Quit. |
| `/u [N]` | Undo both code and transcript to turn N (or list turns if N omitted). |
| `/uc [N]` | Undo code only. |
| `/ut [N]` | Undo transcript only. |
| `!cmd` | Run a shell command locally; output is captured into shell history. |
| `$(N)` | Paste shell history entry N into your message. |
| `$(cmd)` | Run `cmd` and inline its output into your message before sending. |
| `>(N)` | Quote line N from the last agent response (supports ranges `>(N-M)`). |

### `gait status <session-id>`

Prints the current state of a session: `running`, `awaiting_permission`, `awaiting_clarify`, `done`, etc.

```bash
gait status s1a2b3c4 [--workspace <path>] [--json]
```

### `gait approve / gait reject`

Approve or reject a pending permission request from outside the session (useful in headless or scripted pipelines).

```bash
gait approve <session-id> [--pattern <pat>] [--scope <global|agent|once>] [--workspace <path>] [--agent <name>]
gait reject  <session-id> [--workspace <path>]
```

`--scope` controls how broadly the approval is saved:

| Scope | Effect |
|---|---|
| `once` (default) | Approves this single tool call only. |
| `agent` | Adds the pattern to the named agent file's `allow` list. |
| `global` | Adds the pattern to `gait.jsonc`. |

### `gait load / unload / enable / disable`

Register and toggle MCP servers and skills in `gait.jsonc`.

```bash
# MCP servers
gait load mcp <name> --spawn <cmd>       # register a stdio MCP server (added disabled)
gait load mcp <name> --connect <url>     # register a network MCP server (added disabled)

# Skills (inline prompt fragments)
gait load skill <name> --description <text> --path <file>
gait load skill <name> --description <text> --content <text>

# Activate / deactivate (toggles the `enabled` flag in gait.jsonc)
gait enable  <mcp|skill> <name>
gait disable <mcp|skill> <name>

# Remove entirely
gait unload <mcp|skill> <name>
```

`gait load` always adds entries with `enabled: false`. Run `gait enable` to activate.

### `gait sessions`

Lists active (unmerged) sessions.

### `gait auth`

```bash
gait auth list              # show stored providers
gait auth add <provider>    # prompt for API key and save
gait auth remove <provider> # delete stored key
```

Keys are stored at `~/.local/share/gait/auth.json`. You can also set them via environment variables (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `MINIMAX_API_KEY`, etc.).

### `gait models`

```bash
gait models              # list all available models
gait models anthropic    # filter by provider
```

---

## Config: `gait.jsonc`

JSONC (JSON with `//` and `/* */` comments).

```jsonc
{
  // Model to use. Format: "provider/model-id"
  "model": "anthropic/claude-sonnet-4-6",

  // Tools the agent can use without a permission prompt.
  // Empty = everything requires approval. Add tools you trust.
  "allow": ["read", "glob", "grep", "ls"],

  // Tool calls matching these patterns are always blocked (never prompted).
  // Patterns are matched against "<tool>:<cmd>" e.g. "shell:rm *", "shell:git push *".
  "deny": ["shell:git push *"],

  // How many times to retry a turn when the model returns invalid structured output.
  // Default: 3.
  "output_schema_retries": 3,

  // Custom shell tools exposed to the agent as callable functions.
  "tools": [
    {
      "name": "test",
      "description": "Run the test suite",
      "cmd": "cargo test",
      "cwd": "codebase"   // "codebase" or "workspace"
    }
  ],

  // MCP servers. Add entries with `gait load mcp`, toggle with `gait enable/disable`.
  "mcp_servers": [
    { "name": "my-server", "spawn": "npx my-mcp-server", "enabled": true }
  ],

  // Inline skill fragments injected into the agent's system prompt.
  // Add with `gait load skill`, toggle with `gait enable/disable`.
  "skills": [
    {
      "name": "conventions",
      "description": "Project coding conventions",
      "content": "Always use 2-space indentation.",
      "enabled": true
    }
  ]
}
```

---

## Built-in Tools

| Tool | Description |
|---|---|
| `read` | Read a file |
| `write` | Write a file (creates or overwrites) |
| `edit` | Replace a string in a file |
| `multiedit` | Multiple find-replace edits in one call |
| `glob` | Glob file paths |
| `grep` | Search file contents with a regex |
| `ls` | List a directory |
| `codesearch` | Semantic code search |
| `webfetch` | Fetch a URL |
| `shell` | Run a shell command |
| `output` | Signal turn completion with a structured result |
| `spawn_agent` | Delegate a subtask to a new child session |
| `list_agents` | List active child sessions and their status |

All tools go through the permission gate. Tools in `allow` run immediately; others prompt the user (or auto-approve with `--auto`).

---

## Workflows

Workflows are YAML DAGs — sequences or graphs of nodes that execute in dependency order. Each workflow run is a single gait session with its state committed to git after every node.

### Defining a workflow

Save a `.yaml` file in `workflows/` and run it with `gait workflow run <name>`.

```yaml
# workflows/ci.yaml
name: ci-pipeline
model: minimax/<model>
auto: true
allow: [read, shell]

hooks:
  before: "echo starting $WORKFLOW_ID"
  after: "echo done"
  error: "echo failed at $FAILED_NODE"

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
    prompt: "Tests passed. Open a PR with summary: $test.output"
    depends_on: [route]

  - id: fix
    loop:
      prompt: "Tests failed on iteration $LOOP_ITER. Output:\n$test.output\nFix the failures."
      until: "DONE"
      max: 5
    depends_on: [route]

  - id: review-gate
    gate: "Review the changes above, then run: gait workflow continue $WORKFLOW_ID"
    depends_on: [ship]
```

### Node types

| Type | What it does |
|---|---|
| `shell` | Runs a shell command. Captures stdout+stderr, exit code. |
| `prompt` | Runs one LLM turn via `gait run`. Can have `output_schema` for structured JSON output. |
| `loop` | Repeats a prompt up to `max` times until an `until` signal appears in the output or a `shell_check` exits 0. |
| `branch` | Routes to one of several nodes based on a boolean expression or multi-way case match. Inactive paths are skipped. |
| `gate` | Pauses the workflow, prints a message, and waits for `gait workflow continue`. |

### Variable substitution

Inside any string field, these variables are substituted at runtime:

| Variable | Value |
|---|---|
| `$WORKFLOW_ID` | Current workflow session ID |
| `$SESSION_ID` | Same as `$WORKFLOW_ID` |
| `$WORKSPACE` | Workspace path |
| `$CODEBASE` | Codebase path |
| `$ARGS` | Arguments passed to `gait workflow run` |
| `$NODE_ID` | Current node ID |
| `$LOOP_ITER` | Current loop iteration (loop nodes) |
| `$LOOP_MAX` | Max iterations (loop nodes) |
| `$LOOP_OUTPUT` | Output from the previous iteration (loop nodes) |
| `$FAILED_NODE` | ID of the failed node (error hooks) |
| `$node.output` | Text output of node `node` |
| `$node.output.field` | JSON field from node `node`'s structured output |
| `$node.exit` | Exit code of node `node` |

### Node options

Every node supports:

```yaml
- id: my-node
  depends_on: [other-node]   # wait for these nodes first
  on_error: abort             # abort (default) | continue | {goto: node-id}
  retry:
    max: 3
    delay: 1000               # ms between retries
  hooks:
    before: "echo starting $NODE_ID"
    after: "echo done"
    error: "echo failed"
  # prompt/loop nodes also accept:
  model: minimax/<model>
  auto: true
  allow: [read, shell]
  agent: my-agent             # load system prompt from agents/my-agent.md
  system_prompt: "You are..."
  codebase: /path/to/repo
  output_schema:              # force structured JSON output
    type: object
    properties:
      pr_url: { type: string }
```

### Resume after gate or crash

Workflow state is fully reconstructed from the JSONL transcript. If a workflow is interrupted (gate, crash, or `gait workflow abort`), resume it with:

```bash
gait workflow continue <workflow-id>
```

Progress is also written to `sessions/<id>/progress.yaml` after every node for fast status reads.

---

## Subagents

The agent can spawn child sessions with `spawn_agent`:

```
spawn_agent(task="...", agent="optional-agent-name")
→ returns { session_id: "s..." }
```

The child session gets its own branch (`session/<id>`) and runs independently. The parent can check on children via `list_agents`, which returns each session's status (`done`, `clarify`, `inform`).

When a child completes, it writes an `agent_update` event to the parent's transcript, which the parent picks up on its next turn.

---

## Agents

The `agents/` folder holds per-agent `.md` files. Each file has optional YAML frontmatter that overrides config for sessions using that agent, followed by a markdown body that is appended to the system prompt.

```markdown
---
model: minimax/<model>
allow: [read, glob, shell]
deny: ["shell:git push *"]
---

You are a code reviewer. Focus on correctness and security.
```

| Frontmatter field | Effect |
|---|---|
| `model` | Override the model for this agent. |
| `allow` | Additional tools this agent may use without prompting. |
| `deny` | Additional patterns this agent can never call. |

Reference an agent with `--agent <name>` in `gait chat`, `gait approve`, or the `agent:` field in workflow nodes.

---

## Permissions

Tools not in `allow` are blocked and queued as a permission request. gait prints each pending tool call and waits for approval before continuing.

With `--auto`, all permission requests are approved without prompting. Useful in scripted or trusted environments.

Three tiers apply in order:

| Tier | How set | Effect |
|---|---|---|
| **Deny** | `deny` in `gait.jsonc` or agent frontmatter | Always blocked — never prompted. |
| **Allow** | `allow` in `gait.jsonc` or agent frontmatter | Runs immediately without prompting. |
| **Ask** | Everything else | Queued and presented to the user (or auto-approved with `--auto`). |

---

## Providers

Format: `provider/model-id`

| Provider | Example model |
|---|---|
| `anthropic` | `anthropic/claude-sonnet-4-6` |
| `openai` | `openai/gpt-4o` |
| `minimax` | `minimax/<model>` |
| `openrouter` | `openrouter/anthropic/claude-3-5-sonnet` |

Any OpenAI-compatible endpoint works via the `openai` provider with a custom base URL.

---

## How It Works

### Three repos

gait manages three distinct git repositories at runtime:

| Repo | Where | What it is |
|---|---|---|
| **Trace repo** | `<workspace>/` | The workspace git. Session history lives here as branches. Never touches your code. |
| **Code repo** | `<codebase>/` | Your project. gait reads it (HEAD hash, dirty files) but never commits to it. |
| **Checkpoint repo** | `<workspace>/sessions/<id>/codebase.git/` | A per-session bare shadow repo. Snapshots the codebase after every turn so the agent can rewind. Borrows objects from the code repo via git alternates — no duplication. |

The trace repo records *what the agent did* (transcripts, permissions, turn history). The checkpoint repo records *what the codebase looked like* at each point. The code repo is yours — gait leaves its git history untouched.

### Workspace vs codebase

The workspace (`gait.jsonc`, `sessions/`, `AGENTS.md`) is separate from the codebase (the repo the agent edits). This lets you keep session history in one place while pointing the agent at any repo. The workspace is itself a git repo.

### Sessions are git branches

Each session gets its own branch (`session/<id>`) off `main`. Every completed turn, permission prompt, and question is committed to that branch. When you're done, `gait finish` merges it back to `main`.

### Transcripts

`sessions/<id>/transcript.jsonl` is the source of truth — an append-only log of every event in a session:

| Event type | When |
|---|---|
| `session_start` | First turn |
| `user_turn` | Each `--task` |
| `assistant_text` | Agent text output |
| `tool_call` / `tool_result` | Each tool execution |
| `permissions` / `permissions_reply` | Permission gate |
| `clarify` / `clarify_reply` / `clarify_convergence` | Clarify exchange |
| `inform` / `inform_reply` / `inform_convergence` | Inform exchange |
| `compaction` | Context summarized |
| `turn_complete` | End of turn (with token usage + summary) |
| `error` | On failure |
| `workflow_start` | Workflow session begins |
| `node_start` / `node_complete` / `node_skip` / `node_error` | Node lifecycle |
| `hook_run` | Before/after/error hook executed |
| `workflow_gate` | Workflow paused at a gate node |
| `workflow_complete` / `workflow_error` | Workflow terminal state |

Transcripts are reconstructed into LLM messages at the start of each turn, so the agent has full history.

---

## Development

```bash
cd gait-rs
cargo test        # run all tests (188 unit + integration)
cargo build       # debug build
cargo build --release --target-dir /your/target
```

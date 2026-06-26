# gait

Turn-based AI coding agent. No daemon, no server, no UI. State lives in append-only JSONL logs committed to git after every turn.

The name is a backronym: **g**AI**t**. Also just a word for the way something moves.

---

## Install

```bash
cd gait-rs
cargo build --release
cp target/release/gait /usr/local/bin/gait
```

Or use `make install` to build and install to a custom path (edit `Makefile` to set `INSTALL_DIR`).

---

## Quickstart

```bash
# 1. Initialize a gait-dir
mkdir my-workspace && cd my-workspace
gait init

# 2. Add an API key
gait auth add anthropic

# 3. Run a task
gait run --gait-dir . --codebase /path/to/your/repo --task "add a health check endpoint to src/server.ts"

# 4. Set GAIT_DIR to skip the flag
export GAIT_DIR=$PWD
gait run --codebase /path/to/your/repo --task "run the test suite and fix any failures"

# 5. Continue the same session
gait run --session s1a2b3c4

# 6. Non-interactive (auto-approve all permissions)
gait run --task "..." --auto
```

---

## Commands

### `gait init [<path>]`

Sets up the current directory (or `<path>`) as a gait-dir:

- `gait.jsonc` — config
- `agents/default/AGENT.md` — the agent's role and instructions (edit to add project conventions)
- `USER.md` — your personal instructions, gitignored
- `skills/` — installed skills
- `sessions/` — per-session logs, gitignored

Safe to re-run on an existing gait-dir — never clobbers.

### `gait run`

Runs one turn of the agent.

```
gait run [--session <id>] [--task "..."] [--gait-dir <path>] [--codebase <path>]
         [--auto] [--model <provider/model>] [--agent <name>]
         [--result-schema <file>] [--allow <pat>]... [--deny <pat>]...
         [--max-iter <n>] [--container]
```

| Flag | Description |
|---|---|
| `--gait-dir <path>` | Gait-dir path. Defaults to `$GAIT_DIR` env var, then cwd. |
| `--codebase <path>` | Codebase directory. Defaults to cwd. |
| `--session <id>` | Resume a session. Auto-generated if omitted. |
| `--task "..."` | User message for this turn. Omit to continue a paused session. |
| `--model <id>` | Override the model for this turn. |
| `--agent <name>` | Load system prompt from `agents/<name>/AGENT.md`. |
| `--auto` | Auto-approve all permission requests (non-interactive). |
| `--result-schema <file>` | JSON Schema file — force structured output for this turn. |
| `--allow <pat>` | Additional allow patterns for this turn only. |
| `--deny <pat>` | Additional deny patterns for this turn only. |
| `--max-iter <n>` | Cap the number of tool-use iterations. |
| `--container` | Run the turn inside Docker (pulls the gait image if needed). |

Each completed turn is committed to the session's git branch. Interrupted turns are also committed — state is never lost.

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
gait finish s1a2b3c4 --gait-dir /path/to/workspace
```

### `gait undo <session-id>`

Restores a session to a past turn — code, transcript, or both.

```
gait undo <session-id> [--turn N | --list] [--code] [--transcript] [--gait-dir <path>]
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
gait chat [--gait-dir <path>] [--codebase <path>] [--agent <name>] [--session <id>]
```

**REPL commands and syntax**

| Input | What it does |
|---|---|
| `/n` or `/new` | Start a fresh session. |
| `/q` or `/exit` | Quit. |
| `/u [N]` | Undo both code and transcript to turn N (or list turns if N omitted). |
| `/uc [N]` | Undo code only. |
| `/ut [N]` | Undo transcript only. |
| `/skill enable <name>` | Enable a skill for this session. |
| `/skill disable <name>` | Disable a skill for this session. |
| `!cmd` | Run a shell command locally; output is captured into shell history. |
| `$(N)` | Paste shell history entry N into your message. |
| `$(cmd)` | Run `cmd` and inline its output into your message before sending. |
| `>(N)` | Quote line N from the last agent response (supports ranges `>(N-M)`). |

### `gait status <session-id>`

Prints the current state of a session: `running`, `awaiting_permission`, `awaiting_clarify`, `done`, etc.

```bash
gait status s1a2b3c4 [--gait-dir <path>] [--json]
```

### `gait cost <session-id>`

Prints token usage and estimated cost for a session.

```bash
gait cost s1a2b3c4 [--gait-dir <path>] [--json]
```

### `gait allow / gait deny`

Approve or reject a pending permission request from outside the session (useful in headless or scripted pipelines).

```bash
gait allow <session-id> [--pattern <pat>] [--scope <global|agent|once>] [--gait-dir <path>] [--agent <name>]
gait deny  <session-id> [--gait-dir <path>]
```

`--scope` controls how broadly the approval is saved:

| Scope | Effect |
|---|---|
| `once` (default) | Approves this single tool call only. |
| `agent` | Adds the pattern to the named agent file's `allow` list. |
| `global` | Adds the pattern to `gait.jsonc`. |

> `gait approve` / `gait reject` still work as aliases.

### `gait register / unregister / enable / disable`

Register and toggle MCP servers and skills in `gait.jsonc`.

```bash
# MCP servers
gait register mcp <name> --spawn <cmd>       # register a stdio MCP server (added disabled)
gait register mcp <name> --connect <url>     # register a network MCP server (added disabled)

# Skills (inline prompt fragments)
gait register skill <name> --description <text> --path <file>
gait register skill <name> --description <text> --content <text>

# Activate / deactivate (toggles the `enabled` flag in gait.jsonc)
gait enable  <mcp|skill> <name>
gait disable <mcp|skill> <name>

# Remove entirely
gait unregister <mcp|skill> <name>
```

`gait register` always adds entries with `enabled: false`. Run `gait enable` to activate.

> `gait load` / `gait unload` still work as aliases.

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

  // MCP servers. Add entries with `gait register mcp`, toggle with `gait enable/disable`.
  "mcp_servers": [
    { "name": "my-server", "spawn": "npx my-mcp-server", "enabled": true }
  ],

  // Inline skill fragments injected into the agent's system prompt.
  // Add with `gait register skill`, toggle with `gait enable/disable`.
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

Workflows are YAML DAGs — graphs of nodes that execute in dependency order. Each prompt node is a full gait turn with its own session. State is reconstructed from the event log, so workflows survive interruptions.

### Defining a workflow

Save a `.yaml` file in `workflows/` inside your gait-dir and run it with `gait workflow run <name>`.

```yaml
# workflows/ci.yaml
name: ci-pipeline
model: anthropic/claude-sonnet-4-6
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
    prompt: "Tests failed. Output:\n$test.output\nFix the failures and commit."
    on_error:
      retry_times: 3
      delay: 1000
      goto: test          # loop back to re-run tests after fixing
    depends_on: [route]

  - id: review-gate
    gate: "Review the changes above, then run: gait workflow continue $WORKFLOW_ID"
    depends_on: [ship]
```

### Node types

| Type | What it does |
|---|---|
| `shell` | Runs a shell command. Captures stdout+stderr and exit code. |
| `prompt` | Runs one LLM turn. Can have `output_schema` for structured JSON output. |
| `branch` | Routes to a node based on a boolean expression or multi-way `cases` map. Inactive paths are skipped. |
| `gate` | Pauses the workflow, prints a message, and waits for `gait workflow continue`. |

**Loops** are graph cycles — point a node's `goto` (or `on_error.goto`) back to an earlier node. There is no separate loop node type.

### Variable substitution

Inside any string field, these variables are substituted at runtime:

| Variable | Value |
|---|---|
| `$WORKFLOW_ID` | Current workflow session ID |
| `$SESSION_ID` | Same as `$WORKFLOW_ID` |
| `$WORKSPACE` | Gait-dir path |
| `$CODEBASE` | Codebase path |
| `$ARGS` | Arguments passed to `gait workflow run` |
| `$NODE_ID` | Current node ID |
| `$FAILED_NODE` | ID of the failed node (error hooks) |
| `$node.output` | Text output of node `node` |
| `$node.output.field` | JSON field from node `node`'s structured output |
| `$node.exit` | Exit code of node `node` (shell nodes) |

### Node options

Every node supports:

```yaml
- id: my-node
  depends_on: [other-node]     # wait for these nodes first
  goto: next-node              # explicit next node after success (overrides DAG order)
  on_error:
    retry_times: 3             # retry up to N times before the goto/abort applies
    delay: 1000                # ms between retries
    backoff: linear            # linear | exponential
    context: carry             # carry (keep context) | fresh (new session)
    goto: error-handler        # abort (default) | continue | <node-id>
  hooks:
    before: "echo starting $NODE_ID"
    after: "echo done"
    error: "echo failed"
  # prompt nodes also accept:
  model: anthropic/claude-sonnet-4-6
  auto: true
  allow: [read, shell]
  agent: my-agent              # load system prompt from agents/my-agent.md
  system_prompt: "You are..."
  codebase: /path/to/repo
  files: [src/foo.rs, src/bar.rs]   # seed the agent's context with specific files
  output_schema:               # force structured JSON output
    type: object
    properties:
      pr_url: { type: string }
```

### Resume after gate or crash

Workflow state is fully reconstructed from the event log. If a workflow is interrupted (gate, crash, or `gait workflow abort`), resume with:

```bash
gait workflow continue <workflow-id>
```

---

## Subagents

The agent can spawn child sessions with `spawn_agent`:

```
spawn_agent(task="...", agent="optional-agent-name")
→ returns { session_id: "s..." }
```

The child session gets its own branch and runs independently. The parent checks on children via `list_agents`, which returns each session's status (`done`, `clarify`, `inform`).

When a child completes, it writes an `agent_update` event to the parent's transcript, which the parent picks up on its next turn.

---

## Agents

The `agents/` folder holds per-agent directories. Each agent has an `AGENT.md` with optional YAML frontmatter (config overrides) followed by markdown that is appended to the system prompt.

```
agents/
  default/
    AGENT.md    ← used when no --agent flag is passed
  reviewer/
    AGENT.md
```

```markdown
---
model: anthropic/claude-sonnet-4-6
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

Reference an agent with `--agent <name>` in `gait run`, `gait chat`, `gait allow`, or the `agent:` field in workflow nodes.

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
| `minimax` | `minimax/MiniMax-M3` |
| `openrouter` | `openrouter/anthropic/claude-3-5-sonnet` |

Any OpenAI-compatible endpoint works via the `openai` provider with a custom base URL.

---

## How It Works

### Three repos

gait manages three distinct git repositories at runtime:

| Repo | Where | What it is |
|---|---|---|
| **Trace repo** | `<gait-dir>/` | The gait-dir git. Session history lives here as branches. Never touches your code. |
| **Code repo** | `<codebase>/` | Your project. gait reads it (HEAD hash, dirty files) but never commits to it. |
| **Checkpoint repo** | `<gait-dir>/sessions/<id>/codebase.git/` | A per-session bare shadow repo. Snapshots the codebase after every turn so the agent can rewind. Borrows objects from the code repo via git alternates — no duplication. |

The trace repo records *what the agent did* (logs, permissions, turn history). The checkpoint repo records *what the codebase looked like* at each point. The code repo is yours — gait leaves its git history untouched.

### Gait-dir vs codebase

The gait-dir (`gait.jsonc`, `sessions/`, `agents/`) is separate from the codebase (the repo the agent edits). This lets you keep session history in one place while pointing the agent at any repo. The gait-dir is itself a git repo.

### Sessions are git branches

Each session gets its own branch (`session/<id>`) off `main`. Every completed turn, permission prompt, and question is committed to that branch. When you're done, `gait finish` merges it back to `main`.

### Session log

`sessions/<id>/log.jsonl` is the source of truth — an append-only log of every event in a session:

| Event type | When |
|---|---|
| `session_start` | First turn |
| `user_turn` | Each `--task` |
| `assistant_text` | Agent text output |
| `tool_call` / `tool_result` | Each tool execution |
| `permissions` / `permissions_reply` | Permission gate |
| `clarify` / `clarify_reply` | Clarify exchange |
| `inform` / `inform_reply` | Inform exchange |
| `compaction` | Context summarized |
| `turn_complete` | End of turn (with token usage) |
| `error` | On failure |

Logs are reconstructed into LLM messages at the start of each turn, so the agent has full history.

---

## Development

```bash
cd gait-rs
cargo test --workspace    # run all tests
cargo build               # debug build
cargo build --release     # release build
make fmt                  # format the workspace
make lint                 # rustfmt --check + clippy -D warnings
make check                # lint + test (what CI/the hook enforce)
```

### Git hooks

Two hooks live in [`.githooks/`](.githooks/):

- **pre-commit** runs `cargo fmt --check` (fast; only when Rust sources or
  manifests are staged).
- **pre-push** runs `cargo clippy -D warnings` (the heavier check — it compiles
  the workspace — so it runs once before pushing rather than on every commit).
  clippy is read-only and never modifies your code; a warning just rejects the
  push.

Activate both once per clone:

```bash
make -C gait-rs hooks      # or: git config core.hooksPath .githooks
```

Bypass for a single action with `git commit --no-verify` / `git push --no-verify`.

---

## License

gait is free software, licensed under the **GNU Affero General Public License,
version 3 or later** (`AGPL-3.0-or-later`). See [LICENSE](LICENSE) for the full
text and [NOTICE](NOTICE) for third-party attributions.

In short: you are free to use, study, modify, and redistribute gait, but
derivative works must remain under the AGPL. Because it is the *Affero* GPL, if
you run a modified version as a network service, you must also offer your users

## Acknowledgements

UX ideas adapted from [aichat](https://github.com/sigoden/aichat) (MIT OR Apache-2.0).
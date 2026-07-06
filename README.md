# gait

Turn-based AI coding agent. No daemon, no server, no UI. State lives in append-only JSONL logs committed to git after every turn.

The name is a backronym: **g**AI**t**. Also just a word for the way something moves.

---

## Prerequisites

- **Rust** (stable toolchain, edition 2021 — built/tested with 1.92) and **Cargo**. Install via [rustup](https://rustup.rs) if you don't have it: `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.org | sh`.
- **macOS (Apple Silicon or Intel) or Linux.** The sandbox is kernel-enforced per platform — Seatbelt on macOS, Landlock on Linux — and works out of the box on Apple Silicon. Windows is untested and not currently supported.

  Note: the `git` CLI is **not** required at runtime — gait talks to git exclusively through `git2`/libgit2. Building from source shells out to `git rev-parse HEAD` to stamp the binary with a build hash, but degrades gracefully to `"unknown"` if `git` isn't on `PATH`.
- An **API key** for at least one model provider (Anthropic, OpenAI, MiniMax, etc. — see [Providers](#providers)). Add it after install with `gait auth add <provider>`.

---

## Install

```bash
cd gait-rs
cargo build --release
cp target/release/gait /usr/local/bin/gait
```

Or, from inside `gait-rs/`, use `make install` to build and install to a custom path (edit `Makefile` to set `INSTALL_DIR`).

---

## Quickstart

```bash
# 1. Initialize a gait-dir
mkdir my-workspace && cd my-workspace
gait init

# 2. Add an API key
gait auth add anthropic

# 3. Run one turn (bare positional task)
gait "add a health check endpoint to src/server.ts" --gait-dir . --codebase /path/to/your/repo

# 4. Set GAIT_DIR to skip the flag
export GAIT_DIR=$PWD
gait "run the test suite and fix any failures" --codebase /path/to/your/repo

# 5. Continue the same session
gait --session s1a2b3c4 "keep going"

# 6. Non-interactive (auto-approve all permissions)
gait "..." --auto-approve

# 7. Interactive REPL instead of one-shot turns
gait -i

# 8. Ephemeral one-off (no session, no git — just an answer or a shell command)
gait -e "what's the current git branch?"
```

Driving gait from another coding agent (as a dispatched subagent) or a human REPL session? See [`skills/gait-agent/SKILL.md`](skills/gait-agent/SKILL.md) — a portable, agent-agnostic skill covering both.

---

## Modes

`gait` has three top-level modes, selected by how you invoke it (mutually exclusive):

| Invocation | Mode |
|---|---|
| `gait "<task>"` (bare positional) | Runs one turn of a session and exits. |
| `gait -i` / `gait --interactive` | Interactive REPL — iterate turn-by-turn in one process. |
| `gait -e "<prompt>"` / `gait --exec "<prompt>"` | Ephemeral one-off: no session, no git commit — prints an answer or a shell command (asks before running it on a TTY). |

Plus subcommands (`gait <subcommand>`) for everything else: session lifecycle, workflows, config, auth, etc. See [Commands](#commands) below.

### Top-level flags (apply to the bare-task and `-i`/`-e` modes)

| Flag | Description |
|---|---|
| `-s, --session <id>` | Continue an existing session by id. |
| `-m, --model <id>` | Model override (`provider/model`). |
| `-a, --agent <name>` | Agent file to load. |
| `--auto-approve` | Auto-approve all tool-permission prompts. The sandbox ceiling still applies (kernel-enforced) — this only skips the human approval prompt. |
| `--allow <pat>` | Pre-approve a tool pattern (repeatable). |
| `--deny <pat>` | Pre-deny a tool pattern (repeatable). |
| `--unlock <path>` | Pre-grant sandbox path access (repeatable). |
| `-n, --max-iter <n>` | Cap the number of agentic tool-use steps. |
| `-R, --result-schema <file>` | JSON Schema file — force structured output for this turn. |

`--gait-dir`/`--codebase` also exist (hidden flags; default to `$GAIT_DIR`/cwd) — most commands below take them explicitly instead.

---

## Commands

### `gait init [<path>]`

Sets up the current directory (or `<path>`) as a gait-dir and makes an initial git commit:

- `gait.jsonc` — config (committed)
- `agents/default/AGENT.md` — the agent's role and instructions (edit to add project conventions; committed)
- `USER.md` — your personal instructions (gitignored, not shared)
- `skills/` — installed skills, `skills/<name>/SKILL.md` (committed once you add skills)
- `.active/` — live, not-yet-persisted session worktrees (gitignored)

Note: `sessions/` is **not** created by `init`. It only comes into existence, as tracked content on `main`, once a session is persisted (see `gait persist`).

Safe to re-run on an existing gait-dir — never clobbers.

### `gait <task>` — run one turn

```
gait "<task>" [--session <id>] [--model <id>] [--agent <name>]
              [--auto-approve] [--allow <pat>]... [--deny <pat>]... [--unlock <path>]...
              [--max-iter <n>] [--result-schema <file>]
```

Each completed turn is committed to the session's git branch. Interrupted turns are also committed — state is never lost.

### `gait -e "<prompt>"` — ephemeral one-off

Runs a single throwaway turn with no session and no git commit. The agent must return either a shell command (printed, and — on a TTY — offered to run after a `y/N` prompt) or a plain text answer. Useful for quick one-liners: `gait -e "what changed in the last commit?"`.

### `gait workflow`

Run multi-step workflows defined as YAML DAGs. See [Workflows](#workflows) below.

```
gait workflow run <name> [args...] --gait-dir <path>    # run workflows/<name>.yaml
gait workflow list --gait-dir <path>                     # show active workflow sessions
gait workflow continue <workflow-id> --gait-dir <path>   # resume a paused workflow (after a gate)
gait workflow abort <workflow-id> --gait-dir <path>      # mark a workflow as aborted
gait workflow log <workflow-id> --gait-dir <path>        # print the full transcript as JSONL
```

### `gait persist [<session-id> | --all]`

Merges a session's worktree onto `main` and tears down its `.active/` mount. This is how session state (transcript, checkpoints) becomes durable, git-tracked content under `sessions/<id>/`.

```bash
gait persist s1a2b3c4 --gait-dir /path/to/workspace
gait persist --all --gait-dir /path/to/workspace   # persist every idle session
```

`--yes`/`-y` skips the confirmation prompt (required non-interactively). `--json` for machine-readable output.

### `gait discard [<session-id> | --all]`

Tears down a session **without** merging it — the inverse of `persist`. Same flags as `persist`.

### `gait undo <session-id>`

Restores a session to a past turn — code, transcript, or both.

```bash
gait undo <session-id> [--turn N | --list] [--code] [--transcript] [--gait-dir <path>] [--json]
```

| Flag | Description |
|---|---|
| `--list` | Print all turns with their checkpoint SHAs. |
| `--turn N` | Target turn number. Required with `--code` or `--transcript`. |
| `--code` | Restore the codebase working tree to what it was after turn N. |
| `--transcript` | Truncate the transcript back to turn N (removes later turns from git history). |

Omitting both `--code` and `--transcript` restores both. The codebase's own git history is never touched — only the shadow checkpoint is used.

### `gait -i` — interactive REPL

Interactive REPL for conversational use. Starts a new session (or resumes one with `--session`) and lets you iterate turn-by-turn in one process.

```bash
gait -i [--session <id>] [--agent <name>]
```

**REPL slash commands**

| Command | Aliases | What it does |
|---|---|---|
| `/help` | `/?` | Show the command guide. |
| `/info` | | Show model, session, gait-dir, codebase. |
| `/model` | | Show or switch model (`provider/id`). |
| `/sh` | `/e` | One-off: describe a shell command, optionally run it. |
| `/new` | `/n` | Start a fresh session. |
| `/workflow` | | Run a workflow. |
| `/undo` | `/u` | Undo the last turn (code + transcript). |
| `/uc` | | Undo checkpoint (code only). |
| `/ut` | | Undo turn (transcript only). |
| `/sandbox` | | Toggle the sandbox. |
| `/unlock` | | Unlock a held (`awaiting-unlock`) session. |
| `/exit` | `/quit`, `/q` | Exit the REPL. |

**Input syntax** (works in any message, not just slash commands):

| Syntax | What it does |
|---|---|
| `!cmd` | Run a shell command locally; output is captured into shell history. |
| `$(N)` | Paste shell history entry N into your message. |
| `$(cmd)` | Run `cmd` and inline its output into your message before sending. |
| `>(N)` | Quote line N from the last agent response (supports ranges `>(N-M)`). |
| `@path` | Inline the contents of a file at `path` into your message. |

### `gait ps` / `gait sessions`

`gait sessions --gait-dir <path> [--json]` lists active (unmerged) session ids.

`gait ps --gait-dir <path> [--json]` lists sessions currently mounted under `.active/` with their live state (`idle`, `awaiting-permission`, `awaiting-clarify`, `awaiting-inform`, `awaiting-unlock`).

### `gait stats <session-id>`

Prints token usage and estimated cost for a session.

```bash
gait stats s1a2b3c4 --gait-dir <path> [--json]
```

### `gait allow / gait deny`

Approve or reject a pending permission request from outside the session (useful in headless or scripted pipelines).

```bash
gait allow <session-id> [--pattern <pat>] [--scope <once|agent|global>] [--agent <name>] --gait-dir <path> [--json]
gait deny  <session-id> --gait-dir <path> [--json]
```

`--scope` controls how broadly the approval is saved:

| Scope | Effect |
|---|---|
| `once` (default) | Approves this single tool call only. |
| `agent` | Adds the pattern to the named agent file's `allow` list (requires `--agent`). |
| `global` | Adds the pattern to `gait.jsonc`. |

A `session` scope value is also accepted on the wire but not yet wired to a persistence target — it currently behaves like `once`.

### `gait unlock <session-id>`

Grants sandbox path/network access requested by a turn that's paused in `awaiting-unlock` state (kernel-enforced sandbox — see [Sandbox](#sandbox)).

```bash
gait unlock <session-id> [--scope once|session|global|agent] [--agent <name>] --gait-dir <path> [--json]
```

`global` writes the grant into `gait.jsonc`; `agent` writes it into the named agent's frontmatter (requires `--agent`); `once`/`session` only record the decision in the session log.

### `gait register / unregister / enable / disable`

Register and toggle MCP servers, skills, and WASM plugins.

```bash
# MCP servers (written to <gait-dir>/mcp/<name>.json, added disabled)
gait register mcp <name> --command <prog> [--args "<args>"]
gait register mcp <name> --url <url>
gait register mcp <name> --spawn <cmd>      # legacy compat form of --command/--args
gait register mcp <name> --connect <url>    # legacy compat form of --url

# Skills (inline prompt fragments, written to gait.jsonc — legacy; prefer skills/<name>/SKILL.md on disk)
gait register skill <name> --description <text> --path <file>
gait register skill <name> --description <text> --content <text>

# WASM plugins (copied into <gait-dir>/plugins/<name>/, added disabled)
gait register plugin <name> --wasm <path> [--description <text>]

# Activate / deactivate
gait enable  <mcp|skill|plugin> <name>
gait disable <mcp|skill|plugin> <name>

# Remove entirely
gait unregister <mcp|skill|plugin> <name>
```

`gait register` always adds entries disabled; run `gait enable` to activate.

### `gait daemons`

List or stop background daemons the agent has spawned and tracked in the gait-dir.

```bash
gait daemons [list] --gait-dir <path> [--json]
gait daemons stop <name> --gait-dir <path>
```

### `gait auth`

```bash
gait auth list              # show stored providers
gait auth add <provider>    # prompt for API key and save
gait auth remove <provider> # delete stored key
```

Keys are stored via the OS credential store abstraction in `gait-core::auth`. You can also set them via environment variables (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `MINIMAX_API_KEY`, etc.).

### `gait models`

```bash
gait models              # list all available models
gait models anthropic    # filter by substring (provider or id)
```

---

## Config: `gait.jsonc`

JSONC (JSON with `//` and `/* */` comments). **Note:** several `gait register`/`enable`/`disable` operations rewrite this file as plain JSON and strip comments in the process.

```jsonc
{
  // Model to use. Format: "provider/model-id"
  "model": "anthropic/claude-sonnet-4-6",

  // Tools the agent can use without a permission prompt.
  // Empty = everything requires approval. Add tools you trust.
  "allow": ["read", "glob", "grep", "ls", "clarify", "inform"],

  // Tool calls matching these patterns are always blocked (never prompted).
  // Patterns are matched against "<tool>:<cmd>" e.g. "shell:rm *", "shell:git push *".
  "deny": ["shell:git push *"],

  // Kernel-enforced write/network sandbox (Seatbelt on macOS, Landlock on Linux).
  // The codebase root is always readable+writable regardless of this list.
  "sandbox": {
    "on": true,
    "readable": ["$CODEBASE"],
    "writable": ["$CODEBASE"],
    "network": false
  },

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

  // MCP servers. Prefer `gait register mcp` (writes to mcp/<name>.json) over
  // editing this array — `spawn`/`connect` below are the legacy compat form.
  "mcp_servers": [
    { "name": "my-server", "spawn": "npx my-mcp-server", "enabled": true }
  ],

  // Inline skill fragments injected into the agent's system prompt.
  // Deprecated in favor of skills/<name>/SKILL.md on disk — kept for compat.
  "skills": [
    {
      "name": "conventions",
      "description": "Project coding conventions",
      "content": "Always use 2-space indentation.",
      "enabled": true
    }
  ],

  // Lifecycle hooks: shell commands run around turns/steps/tools.
  // event: pre_turn | pre_step | post_step | pre_tool | post_tool | post_turn
  "hooks": [
    { "event": "post_tool", "tool": "shell", "run": "echo ran a shell tool" }
  ]
}
```

Other config fields that exist but have no user-facing example above (all optional, with sane defaults): `compaction_threshold` (float, when to summarize old turns), `wasm_plugins` (populated by `gait register plugin`), `max_iter`, `shell_timeout_secs` / `tool_timeout_secs` (default 300s each), `preview` (`on-ask` | `always` | `off`), `result_schema`, `llm_params` (temperature/max_tokens/top_p plus per-provider extras), `show_thinking`.

---

## Sandbox

gait can kernel-enforce filesystem and network access for the whole turn (Seatbelt on macOS, Landlock on Linux), independent of the tool-permission prompts above — permissions gate what the *agent chooses* to do; the sandbox gates what the *process is physically able* to do. Configured via the `sandbox` block in `gait.jsonc` (`on`/`readable`/`writable`/`network`) or per-invocation with `--unlock <path>`.

When a turn hits a sandbox boundary it wasn't granted, the session pauses in `awaiting-unlock` state; resolve it with `gait unlock <session-id>` (or `/unlock` in the REPL).

---

## Built-in Tools

| Tool | Description |
|---|---|
| `read` | Read a file |
| `write` | Write a file (creates or overwrites) |
| `edit_replace` | Replace a string in a file |
| `edit_append` | Append content to a file |
| `edit_prepend` | Prepend content to a file |
| `glob` | Glob file paths |
| `grep` | Search file contents with a regex |
| `ls` | List a directory |
| `codesearch` | Semantic code search |
| `webfetch` | Fetch a URL |
| `shell` | Run a shell command |
| `structured_done` | Signal turn completion with a structured result |
| `spawn_agent` | Delegate a subtask to a new child session |
| `list_agents` | List active child sessions and their status |
| `clarify` | Ask the user a clarifying question and pause the turn |
| `inform` | Tell the user something without expecting a reply |
| `annotate` | Attach an annotation to the transcript |
| `prune` | Drop old context from the working transcript |
| `enable` | Toggle an MCP server/skill/plugin on for the rest of the turn |

All tools go through the permission gate. Tools in `allow` run immediately; others prompt the user (or auto-approve with `--auto-approve`). The sandbox (above) applies underneath the permission gate regardless of `allow`.

---

## Hooks

Hooks run a shell command at fixed points in the turn loop — for validation, logging, or injecting context — configured via the `hooks` array in `gait.jsonc` or agent frontmatter:

```jsonc
"hooks": [
  { "event": "post_tool", "tool": "shell", "run": "echo ran a shell tool" }
]
```

| Field | Meaning |
|---|---|
| `event` | One of the six lifecycle events below. |
| `tool` | Optional tool-name filter — only for `pre_tool`/`post_tool`; omit to match every tool. |
| `run` | Shell command, executed via `sh -c`. |

### Events

| Event | Fires | Effect |
|---|---|---|
| `pre_turn` | Once, after the system prompt is built, before the step loop starts. | Exit 0 with non-empty stdout **replaces the system prompt** for the turn. |
| `pre_step` | Before each LLM call. | Observation only — output has no effect on the step. |
| `post_step` | After the LLM response streams in, before its tool calls run. | Observation only. |
| `pre_tool` | Before each tool call (respecting the `tool` filter). | **Non-zero exit blocks the tool call** — its stdout+stderr is returned to the model as the block reason instead of running the tool. |
| `post_tool` | After a tool call executes. | Exit 0 with non-empty stdout **replaces the tool result**; non-zero exit appends the hook's output as an error annotation to the original result. |
| `post_turn` | Once, at the very end of the turn. | Observation only. |

Every hook run is recorded to the session log as a `hook_run` event, so hook activity replays with the rest of the transcript.

### Environment passed to the hook command

| Variable | Available on | Value |
|---|---|---|
| `GAIT_EVENT` | all | The event name. |
| `GAIT_SESSION_ID`, `GAIT_DIR`, `GAIT_CODEBASE` | all | Session id, gait-dir, codebase path. |
| `GAIT_STEP_INDEX`, `GAIT_STEP_COUNT`, `GAIT_MESSAGE_COUNT`, `GAIT_LAST_ROLE` | all | Turn-loop position. |
| `GAIT_SYSTEM_PROMPT` | from `pre_turn` onward | Current system prompt text. |
| `GAIT_RESPONSE_TEXT`, `GAIT_TOOL_COUNT`, `GAIT_TOOL_NAMES` | from `post_step` onward | The assistant's text and the tools it just called. |
| `GAIT_TOOL_NAME`, `GAIT_TOOL_CALL_ID` | `pre_tool`, `post_tool` | Which tool call this is. |
| `GAIT_ARG_<KEY>` | `pre_tool`, `post_tool` | One var per tool argument — key uppercased, `-`→`_` (e.g. a `file-path` argument becomes `GAIT_ARG_FILE_PATH`). |
| `GAIT_TOOL_RESULT` | `post_tool` | The tool's raw output, before any hook rewrite. |
| `GAIT_TURN_OUTCOME` | `post_step`, `post_turn` | `done` \| `clarify` \| `inform` \| `permissions` \| empty if still in progress. |
| `GAIT_FINAL_RESPONSE` | `post_turn` | The turn's final response text. |

Hooks run with no timeout and inherit gait's working directory — keep them fast, and have long-running work spawn its own background process instead of blocking the hook.

---

## Extending gait

Four ways to add capability without changing gait's source:

| Mechanism | What it adds | Where it lives | Managed with |
|---|---|---|---|
| **MCP servers** | New tools, proxied from an external MCP server. | `<gait-dir>/mcp/<name>.json` | `gait register/enable/disable/unregister mcp` — see [Commands](#gait-register--unregister--enable--disable). |
| **Skills** | Prompt fragments appended to the system prompt (conventions, playbooks). | `<gait-dir>/skills/<name>/SKILL.md` (frontmatter: `name`, `description`; body = markdown injected verbatim) | Auto-loaded from disk — drop a `SKILL.md` in and it's picked up on the next turn. The `gait register skill` / inline `gait.jsonc` `skills` array path still works but is deprecated. |
| **Hooks** | Shell-command side effects and content rewrites at fixed points in the turn loop. | `hooks` array in `gait.jsonc` or agent frontmatter | Edit the config directly — see [Hooks](#hooks) above. |
| **WASM plugins** | *(scaffolding only — not yet executed at runtime.)* | `<gait-dir>/plugins/<name>/PLUGIN.md` (frontmatter: `name`, `wasm`, `enabled`, `allow_exec`, `allow_fs`) | `gait register/enable/disable/unregister plugin` registers and toggles the manifest; there is no plugin execution path yet, so an enabled plugin currently has no runtime effect. |

For deeper custom logic than a hook or skill can express, `spawn_agent` (see [Subagents](#subagents)) lets a running turn delegate work to a whole new gait session with its own agent/model/tool config.

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
| `shell` | Runs a shell command. Captures stdout+stderr and exit code. Accepts a short string form (`shell: "cargo test"`) or the full `{run, cwd, timeout}` form. |
| `prompt` | Runs one LLM turn. Can have `output_schema` for structured JSON output. |
| `branch` | Routes to a node based on a boolean expression or multi-way `cases` map. Inactive paths are skipped. |
| `gate` | Pauses the workflow, prints a message, and waits for `gait workflow continue`. Accepts a short string form or `{message}`. |

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
| `$NODE_EXIT` | Exit code of the *current* node (shell nodes) |
| `$FAILED_NODE` | ID of the failed node (error hooks) |
| `$node.output` | Text output of node `node` |
| `$node.output.field` | JSON field from node `node`'s structured output |
| `$node.exit` | Exit code of a *named* node `node` (shell nodes) |

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
  continue_session: other-node-id   # continue a prior node's session instead of starting fresh
  output_schema:               # force structured JSON output
    type: object
    properties:
      pr_url: { type: string }
```

### Resume after gate or crash

Workflow state is fully reconstructed from the event log. If a workflow is interrupted (gate, crash, or `gait workflow abort`), resume with:

```bash
gait workflow continue <workflow-id> --gait-dir <path>
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

Reference an agent with `-a`/`--agent <name>` in the bare-task/`-i` modes, `gait allow`, `gait unlock`, or the `agent:` field in workflow nodes.

---

## Permissions

Tools not in `allow` are blocked and queued as a permission request. gait prints each pending tool call and waits for approval before continuing.

With `--auto-approve`, all permission requests are approved without prompting. Useful in scripted or trusted environments. This does not bypass the [sandbox](#sandbox).

Three tiers apply in order:

| Tier | How set | Effect |
|---|---|---|
| **Deny** | `deny` in `gait.jsonc` or agent frontmatter | Always blocked — never prompted. |
| **Allow** | `allow` in `gait.jsonc` or agent frontmatter | Runs immediately without prompting. |
| **Ask** | Everything else | Queued and presented to the user (or auto-approved with `--auto-approve`). |

---

## Providers

Format: `provider/model-id`

| Provider | Example model |
|---|---|
| `anthropic` | `anthropic/claude-sonnet-4-6` |
| `openai` | `openai/gpt-4o` |
| `minimax` | `minimax/MiniMax-M3` |
| `openrouter` | `openrouter/anthropic/claude-3-5-sonnet` |
| `groq` | `groq/llama-3.3-70b-versatile` |
| `mistral` | `mistral/mistral-large-latest` |
| `deepinfra` | `deepinfra/...` |
| `xai` | `xai/grok-...` |
| `cerebras` | `cerebras/...` |
| `togetherai` | `togetherai/...` |
| `fireworks` | `fireworks/...` |
| `perplexity` | `perplexity/...` |

Run `gait models` to list what's actually available for your configured credentials.

---

## How It Works

### Three repos

gait manages three distinct git repositories at runtime:

| Repo | Where | What it is |
|---|---|---|
| **Trace repo** | `<gait-dir>/` | The gait-dir git. Session history lives here as branches, and persisted sessions under `sessions/`. Never touches your code. |
| **Code repo** | `<codebase>/` | Your project. gait reads it (HEAD hash, dirty files) but never commits to it. |
| **Checkpoint repo** | `<gait-dir>/.active/<id>/codebase.git/` | A per-session bare shadow repo. Snapshots the codebase after every turn so the agent can rewind. Borrows objects from the code repo via git alternates — no duplication. |

The trace repo records *what the agent did* (logs, permissions, turn history). The checkpoint repo records *what the codebase looked like* at each point. The code repo is yours — gait leaves its git history untouched.

### Gait-dir vs codebase

The gait-dir (`gait.jsonc`, `sessions/`, `agents/`) is separate from the codebase (the repo the agent edits). This lets you keep session history in one place while pointing the agent at any repo. The gait-dir is itself a git repo.

### Sessions are git branches, mounted live under `.active/`

Each session gets its own branch (`session/<id>`) off `main`, checked out live under `<gait-dir>/.active/<id>/` while it's in progress. Every completed turn, permission prompt, and question is committed to that branch. When you run `gait persist`, the branch is merged back to `main` and `sessions/<id>/` becomes the durable, tracked home for that session's transcript; `gait discard` tears the branch down instead, without merging.

### Session log

`<gait-dir>/.active/<id>/sessions/<id>/log.jsonl` (moving to `<gait-dir>/sessions/<id>/log.jsonl` on persist) is the source of truth — an append-only log of every event in a session. Selected event kinds:

| Event type | When |
|---|---|
| `session_start` | First turn |
| `user_text` | Each user message |
| `assistant_text` | Agent text output |
| `tool_call` / `tool_result` | Each tool execution |
| `permissions` / `permissions_reply` | Permission gate |
| `clarify` / `clarify_reply` | Clarify exchange |
| `inform` / `inform_reply` | Inform exchange |
| `unlock` / `unlock_reply` | Sandbox unlock exchange |
| `agent_update` | Status pushed from a child session (`spawn_agent`) |
| `summary` | Older turns summarized to reclaim context |
| `turn_complete` | End of turn (with token usage) |
| `error` | On failure |

Other event kinds exist for narrower purposes (`sandbox_applied`, `hook_run`, `cwd`, `toggle`, `model_change`, `cancellation_signal`, `inject`, `context_status`, `thinking_block`, `daemon_start`/`daemon_stop`/`daemon_update`, `raw`) — see `proto/log.proto` for the full wire format.

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
the opportunity to obtain the source code for that modified version.

## Acknowledgements

UX ideas adapted from [aichat](https://github.com/sigoden/aichat) (MIT OR Apache-2.0).

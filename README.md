# gait

Turn-based AI coding agent. No daemon, no server, no UI. State lives in append-only JSONL transcripts committed to git after every turn.

The name is a backronym: **g**AI**t**. Also just a word for the way something moves.

---

## Install

```bash
# From source (requires Bun)
bun install
bun run build        # → dist/gait
cp dist/gait /usr/local/bin/gait
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
         [--agent <name>] [--system-prompt "..."] [--auto] [files...]
```

| Flag | Description |
|---|---|
| `--session <id>` | Resume a session. Auto-generated if omitted. |
| `--task "..."` | User message for this turn. Omit to continue a paused session (e.g. after a permission prompt or clarifying question). |
| `--workspace <path>` | Workspace directory. Defaults to cwd. |
| `--codebase <path>` | Codebase directory. Defaults to `<workspace>/codebase`. |
| `--agent <name>` | Load system prompt from `agents/<name>.md`. Persisted to the session on first use. |
| `--system-prompt "..."` | Inline system prompt. Overrides `--agent`. Persisted on first use. |
| `--auto` | Auto-approve all permission requests (non-interactive). |
| `[files...]` | Paths relative to `codebase/` injected as context at turn start. |

Each completed turn is committed to the session's git branch. Interrupted turns (permissions, questions) are also committed — state is never lost.

### `gait finish <session-id>`

Merges a session branch into `main` and removes the worktree. Call this when a session is done and you want its history folded into the workspace's main branch.

```bash
gait finish s1a2b3c4 --workspace /path/to/workspace
```

### `gait sessions`

Lists active (unmerged) sessions.

```bash
gait sessions --workspace /path/to/workspace
```

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

  // Custom shell tools exposed to the agent as callable functions.
  "tools": [
    {
      "name": "test",
      "description": "Run the test suite",
      "cmd": "bun test",
      "cwd": "codebase"   // "codebase" or "workspace"
    }
  ],

  // Compact the session when this fraction of the context window is used (default 0.8).
  "compaction_threshold": 0.8,

  // MCP servers to connect to.
  "mcpServers": {
    "my-server": {
      "command": "npx",
      "args": ["-y", "@my-org/mcp-server"]
    }
  }
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
| `apply_patch` | Apply a unified diff |
| `glob` | Glob file paths |
| `grep` | Search file contents with a regex |
| `ls` | List a directory |
| `codesearch` | Semantic code search |
| `webfetch` | Fetch a URL |
| `shell` | Run a shell command |
| `clarify` | Ask the user questions (pauses the turn — see below) |
| `inform` | Teach the user something (pauses the turn — see below) |
| `spawn_agent` | Delegate a subtask to a new child session |
| `list_agents` | List active child sessions and their status |

All tools go through the permission gate. Tools in `allow` run immediately; others prompt the user (or auto-approve with `--auto`).

---

## Clarify and Inform

These are gait's two structured communication tools. Both pause the current turn and require a follow-up `gait run` to continue.

### `clarify` — agent asks the user questions

The agent calls `clarify` when it needs information only the user can provide before it can proceed. Questions are tagged by kind:

- `is` — factual gap ("which database engine does this use?")
- `ought` — value judgment ("should this be postgres or sqlite?")

Each question includes a `why` field explaining the cost of skipping, and optional `links` to other questions that depend on or relate to it.

After the user answers (next `gait run`), the agent proposes its understanding back via `clarify_convergence`. The user confirms or corrects. Only then does the agent proceed — this prevents silent misunderstandings from propagating into code.

```
Turn 1:  agent calls clarify(items)
         → turn pauses, questions printed, committed to session branch

Turn 2:  gait run (no --task needed)
         → user answers each question
         → agent proposes understanding via clarify_convergence

Turn 3:  gait run
         → user confirms or corrects the understanding
         → agent proceeds with the task
```

### `inform` — agent teaches the user something

The agent calls `inform` when it has context the user needs — an explanation, a decision rationale, or a summary of something it found. After conveying the information, it asks the user to paraphrase their understanding back via `inform_convergence`. If the paraphrase is wrong, the agent corrects and tries again.

```
Turn 1:  agent calls inform(items)
         → turn pauses, information printed

Turn 2:  gait run
         → user paraphrases their understanding
         → agent confirms or corrects via inform_convergence
```

Both tools record their full exchange in the transcript so future turns have complete context.

---

## Subagents

The agent can spawn child sessions with `spawn_agent`:

```
spawn_agent(task="...", agent="optional-agent-name", context_files=[...])
→ returns { session_id: "s..." }
```

The child session gets its own branch (`session/<id>`) and runs independently. The parent can check on children via `list_agents`, which returns each session's status (`running`, `clarify`, `inform`, `permissions`, `error`, `done`).

When a child session needs to clarify or inform, it bubbles the request up to the parent session rather than pausing inline. The parent surfaces the question/information to the user on its next turn.

---

## Permissions

Tools not in `allow` are blocked and queued as a permission request. gait prints each pending tool call and waits for approval before continuing:

```
Permission requests:
  [1] write({"filePath":"src/server.ts",...})  — LLM requested write

[1] y/n >
```

- `y` / `yes` — approve and run
- `n` / `no` — deny
- anything else — treated as an instruction appended to the tool result

With `--auto`, all permission requests are approved without prompting. Useful in scripted or trusted environments. The approval is recorded in the transcript.

---

## Providers

Format: `provider/model-id`

| Provider | Example model |
|---|---|
| `anthropic` | `anthropic/claude-sonnet-4-6` |
| `openai` | `openai/gpt-4o` |
| `google` | `google/gemini-2.0-flash` |
| `minimax` | `minimax/MiniMax-M3` |
| `groq` | `groq/llama-3.3-70b-versatile` |
| `mistral` | `mistral/mistral-large-latest` |
| `openrouter` | `openrouter/anthropic/claude-3-5-sonnet` |
| `xai` | `xai/grok-3` |
| `perplexity` | `perplexity/sonar-pro` |
| `cerebras` | `cerebras/llama3.1-70b` |
| `cohere` | `cohere/command-r-plus` |
| `togetherai` | `togetherai/meta-llama/Llama-3-70b-chat-hf` |
| `deepinfra` | `deepinfra/meta-llama/Llama-3-70b-chat-hf` |
| `amazon-bedrock` | `amazon-bedrock/anthropic.claude-3-5-sonnet-20241022-v2:0` |
| `azure` | `azure/gpt-4o` |

---

## How It Works

### Workspace vs codebase

The workspace (`gait.jsonc`, `sessions/`, `AGENTS.md`) is separate from the codebase (the repo the agent edits). This lets you keep session history in one place while pointing the agent at any repo. The workspace is itself a git repo.

### Sessions are git branches

Each session gets its own branch (`session/<id>`) off `main`, in a git worktree at `sessions/<id>/`. Every completed turn, permission prompt, and question is committed to that branch. When you're done, `gait finish` merges it back to `main`.

### Transcripts

`sessions/<id>/transcript.jsonl` is the source of truth — an append-only log of every event in a session:

| Event type | When it's written |
|---|---|
| `session_start` | First turn |
| `user_turn` | Each `--task` |
| `assistant_text` | Agent text output |
| `tool_call` / `tool_result` | Each allowed tool execution |
| `permissions` / `permissions_reply` | Permission requests and responses |
| `clarify` / `clarify_reply` / `clarify_convergence` | Full clarify exchange |
| `inform` / `inform_reply` / `inform_convergence` | Full inform exchange |
| `compaction` | When context is summarized |
| `turn_complete` | End of each successful turn |
| `error` | On failure |

Transcripts are reconstructed into LLM messages at the start of each turn, so the agent has full history.

### Compaction

When the transcript approaches the model's context window (configurable via `compaction_threshold`, default 80%), gait compacts it: the LLM summarizes the session history into a single message, which replaces the raw events from that window. The agent continues without losing meaningful context.

### MCP servers

`mcpServers` in `gait.jsonc` connects to any MCP-compatible tool server. MCP tools are available to the agent alongside built-ins and config tools, subject to the same permission gate.

---

## Development

```bash
bun test          # run tests
bun run dev       # run CLI without building
bun run build     # compile to dist/gait
```

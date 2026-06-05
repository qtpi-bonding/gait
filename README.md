# gait

Turn-based AI coding agent. Each session is a git commit trail. State lives in JSONL transcripts. No daemon, no server, no UI.

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
# 1. Initialize a workspace (adds gait.jsonc, sessions/, .gitignore)
cd my-project
gait init

# 2. Add an API key
gait auth add anthropic

# 3. Run a task
gait run --task "add a health check endpoint to src/server.ts"

# 4. Continue the same session
gait run --session s1a2b3c4

# 5. Run non-interactively (auto-approve all permissions)
gait run --task "run the test suite and fix any failures" --auto
```

---

## Commands

### `gait init`

Sets up the current directory as a gait workspace:

- `gait.jsonc` — config file
- `sessions/` — per-session transcript and session.json
- `.gitignore` — excludes lock files
- Optionally adds a `codebase/` git submodule pointing at another repo

### `gait run`

Runs one turn of the agent.

```
gait run [files...] [--session <id>] [--task <text>] [--auto]
```

| Flag | Description |
|---|---|
| `--session <id>` | Resume or continue a named session. Auto-generated if omitted. |
| `--task <text>` | User message for this turn. If omitted, continues from where the last turn left off (e.g. after a permission prompt). |
| `--workspace <path>` | Path to the gait workspace directory. Defaults to cwd. Useful when invoking gait from inside an external tool's worktree (e.g. archegraph). |
| `--codebase <path>` | Path to the codebase directory. Defaults to `<workspace>/codebase`. Use with `--workspace` to point gait at an archegraph worktree while keeping sessions in the main workspace. |
| `--auto` | Non-interactive mode — approves all permission requests automatically. |
| `[files...]` | Paths relative to `codebase/` to inject as context at the start of the turn. |

Sessions persist across turns. Each completed turn is committed to git (`session(<id>): turn N`). Permissions and questions that interrupt a turn are also committed so state is never lost.

### `gait auth`

```bash
gait auth list              # show stored providers
gait auth add <provider>    # prompt for API key and save
gait auth remove <provider> # delete stored key
```

Keys are stored at `~/.local/share/gait/auth.json`.

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
  "model": "anthropic/claude-opus-4-5",

  // Tools the agent can use without asking for permission.
  // Any tool not listed here triggers a permission prompt.
  "allow": ["read", "glob", "grep", "ls", "codesearch", "webfetch"],

  // Custom shell tools exposed to the agent.
  "tools": [
    {
      "name": "test",
      "description": "Run the test suite",
      "cmd": "bun test",
      "cwd": "codebase"   // or "workspace"
    }
  ],

  // Compact the session context when this fraction of the context window is used.
  "compaction_threshold": 0.8
}
```

---

## Built-in Tools

| Tool | Description |
|---|---|
| `read` | Read a file |
| `write` | Write a file |
| `edit` | Replace a string in a file |
| `multiedit` | Multiple edits in one call |
| `apply_patch` | Apply a unified diff |
| `glob` | Glob file paths |
| `grep` | Grep for a pattern |
| `ls` | List directory |
| `codesearch` | Semantic code search |
| `webfetch` | Fetch a URL |
| `shell` | Run a shell command |
| `questions` | Ask the user clarifying questions (pauses the turn) |
| `spawn_subagent` | Delegate a task to a child session |

All tool calls go through the permission gate. Tools in `allow` run immediately; others prompt the user (or auto-approve with `--auto`).

---

## Providers

Format: `provider/model-id`

| Provider | Example |
|---|---|
| `anthropic` | `anthropic/claude-opus-4-5` |
| `openai` | `openai/gpt-4o` |
| `google` | `google/gemini-2.0-flash` |
| `groq` | `groq/llama-3.3-70b-versatile` |
| `mistral` | `mistral/mistral-large-latest` |
| `openrouter` | `openrouter/anthropic/claude-3-5-sonnet` |
| `minimax` | `minimax/MiniMax-M3` |
| `xai` | `xai/grok-3` |
| `perplexity` | `perplexity/sonar-pro` |
| `cerebras` | `cerebras/llama3.1-70b` |
| `cohere` | `cohere/command-r-plus` |
| `togetherai` | `togetherai/meta-llama/Llama-3-70b-chat-hf` |
| `deepinfra` | `deepinfra/meta-llama/Llama-3-70b-chat-hf` |
| `amazon-bedrock` | `amazon-bedrock/anthropic.claude-3-5-sonnet-20241022-v2:0` |
| `azure` | `azure/gpt-4o` |

API keys can be set via `gait auth add <provider>` or environment variables (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, etc.).

---

## How It Works

Each session is a directory at `sessions/<id>/`:

- `transcript.jsonl` — append-only event log (user turns, tool calls, results, permissions, compactions)
- `session.json` — cumulative token usage
- `lock` — PID file; prevents concurrent access to the same session

When a turn completes, gait commits everything to git. If the context window fills up, gait compacts the transcript using the LLM itself and continues.

Subagents spawned via `spawn_subagent` get their own session ID (`<parent>-sub-<8chars>`) and run the same turn loop recursively.

---

## Development

```bash
bun test          # run tests
bun run dev       # run CLI without compiling
bun run build     # compile to dist/gait
```

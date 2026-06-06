# gait — agent guidelines

## Commands

- **Test**: `bun test`
- **Single file**: `bun test src/tools/mcp.test.ts`
- **Run CLI**: `bun run src/cli/index.ts <command>`
- **Build binary**: `bun run build` → `dist/gait`
- **Typecheck**: `bun run tsc --noEmit` (no explicit typecheck script; use tsc directly)

## Architecture

gait is a **daemonless, git-native agent runner**. Each `gait run` is one turn. Nothing persists in memory between turns.

### Key invariants

- **Transcript-first**: every event (tool call, result, clarify, turn_complete, etc.) is appended to an append-only JSONL file before anything else. If it didn't get written to the transcript, it didn't happen.
- **Git-native**: each session lives on its own branch (`session/{id}`) branched off `main`. Commits happen at every turn boundary. `git branch --merged main` identifies finished sessions.
- **Daemonless**: `runTurn` in `src/cli/cmd/run.ts` is the entire execution unit. It acquires a file lock, runs one LLM turn, commits, releases. No background processes.
- **Per-turn MCP**: MCP server subprocesses are spawned at the start of `runTurn` and closed in the `finally` block. Never persist across turns.
- **Flat agent configs**: agent system prompts live in `agents/{name}.md` at the workspace root. No config hierarchy — one location, flat directory.

### Source layout

```
src/
  cli/cmd/run.ts        — runTurn(): the core turn loop
  config.ts             — gait.jsonc schema (Zod); includes default_agent field
  agent.ts              — loadAgentSystemPrompt(), resolveAgentPrompt()
  system_prompt.ts      — buildSystemPrompt() — gait block + AGENTS.md + agent-instructions
  session/
    transcript.ts       — all event schemas + append/read helpers
    context.ts          — reconstruct JSONL events → AI SDK messages
    compaction.ts       — LLM-based context compression
  tools/
    index.ts            — createBuiltinTools(), createConfigTools()
    mcp.ts              — createMcpTools() — stdio MCP client per turn
    clarify.ts          — clarify/clarify_convergence tools + signals
    inform.ts           — inform/inform_convergence tools + signals
    spawn_subagent.ts   — (deprecated, being replaced by async agent model)
  git.ts                — worktree create/commit/lock helpers
  permissions.ts        — tool call gating + approval flow
  provider/             — AI SDK model resolution across providers
agents/
  default.md            — generic agent system prompt stub; customize per workspace
```

### Transcript event types

`session_start` → `user_turn` → `tool_call` / `tool_result` → `assistant_text` → `turn_complete`

Special exits: `clarify`, `inform`, `permissions` (each commits and exits; next `gait run` resumes from that state).

Compaction: `compaction` event replaces old events with a summary; `reconstructMessages` uses the latest compaction as its starting point.

## Code style

- **Runtime**: Bun + TypeScript ESM
- **Validation**: Zod everywhere for external inputs (config, transcript events, tool parameters)
- **AI SDK**: `ai` package from Vercel — `streamText`, `CoreTool`, `tool()`
- **Tests**: `bun:test` — always TDD; watch each test fail before implementing
- **No mocks** unless unavoidable; test against real implementations
- **No comments** unless the why is non-obvious
- **No error handling** for impossible cases; validate at boundaries only

## Testing patterns

Each tool module exports its signal/state + a `reset*Signal()` for test isolation. Tests call `beforeEach` to reset.

Transcript tests write to a `tmpDir` via `fs.mkdtempSync` and clean up in `afterEach`.

MCP tests use `src/tools/mcp_fake_server.ts` — a real stdio MCP server subprocess.

## Active design work

- **Agent coordination redesign**: `spawn_subagent` (in-process recursive) is being replaced by an async peer-agent model. See `docs/superpowers/specs/2026-06-06-agent-coordination-design.md`.

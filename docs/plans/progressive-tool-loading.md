# Progressive Tool Loading

## Problem

MCP tools today are dumped into the `tools` array on every API call — full schemas, all registered servers, every turn. Token waste, attention dilution, no progressive disclosure. Same problem as Claude Code's skill loading.

## Design

### Three tiers

```
system:
  - gait identity (session, workspace, codebase)
  - MCP index: name + one-liner per tool
  - skill index: name + one-liner per skill

tools (always present):
  - core tools: read, edit, write, shell, grep, glob, clarify, inform, output, ...
  - mcp(name, context: bool)
  - skill(name, context: bool)

messages:
  - conversation history
  - loaded MCP schemas land here as tool results
  - loaded skill content lands here as tool results
```

### `mcp(name, context: bool)`

```typescript
mcp("slack_send", true)   // load
mcp("slack_send", false)  // unload
```

**On load (`true`):**
1. Full MCP schema injected into messages as tool result
2. Harness adds tool to `tools` array for all subsequent API calls
3. Model can now call it with full schema validation

**On unload (`false`):**
1. Harness removes tool from `tools` array next call
2. Schema stays in messages (append-only) but model can't invoke it anymore
3. Effectively a clean slate for that tool

### `skill(name, context: bool)`

```typescript
skill("tdd", true)    // load
skill("tdd", false)   // unload
```

**On load (`true`):**
1. Full skill content injected into messages as tool result
2. No `tools` array change needed — skills are text, not callable tools

**On unload (`false`):**
1. Harness injects a small `skill(tdd) unloaded` marker into messages
2. Model knows to stop referencing it
3. On compaction, content gets summarized away

## Derived state — no separate storage

Loaded tool state is derived from the transcript on every turn. No in-memory tracking needed.

```
scan all tool_call events where name = "mcp" or "skill"
last call per tool name wins
context=true → loaded, context=false → unloaded
```

On every API call, `tools` array = core tools + `mcp()` + `skill()` meta-tools + schemas for all currently-loaded MCP tools.

This is free — gait already reads the transcript to reconstruct messages. Loaded tool state is just one more derived view from the same scan. Resume after crash, mid-workflow, gate pause — always correct, no sync needed.

## What you win

- **Turns before loading** — zero cost for unused tools. 50 MCP tools registered, only 2 used → 48 never touch the context
- **Explicit lifecycle** — model manages its own tool context. Load when needed, unload when done
- **No Claude Code problem** — no "everything jammed in forever." Model unloads tools between tasks without waiting for compaction
- **Tools array stays small and stable** — good for prompt caching on the tools parameter if/when Anthropic supports it

## What you don't win

- Once loaded, schema is in history until compaction. Same token cost as always-on for that portion of the session.
- Progressive loading + compaction together give full efficiency. Neither alone is sufficient.

## Compaction interaction

After compaction, prior `mcp()`/`skill()` tool calls are summarized away. The derived loaded set becomes empty — schemas were compacted out of history. Model reloads only what it still needs. This is the natural long-session reset, no explicit handling required.

## Index format (system prompt injection)

```
<mcp-index>
slack_send — post a message to a Slack channel
notion_create — create a Notion page
github_pr — open a GitHub pull request
</mcp-index>

<skill-index>
tdd — test-driven development workflow
code-review — multi-pass review checklist
debugging — systematic bug isolation
</skill-index>
```

Tiny. Stable. Prompt-cached. Model reads it once and knows what's available.

## Implementation order

1. Add `mcp(name, context)` and `skill(name, context)` tools to `createBuiltinTools`
2. Add `SessionToolState` tracking to `run.ts`
3. On `mcp(name, true)`: inject schema into messages, add to `loadedMcp`
4. On `mcp(name, false)`: remove from `loadedMcp`
5. On `skill(name, true)`: inject skill content into messages, add to `loadedSkills`
6. On `skill(name, false)`: inject unload marker, remove from `loadedSkills`
7. At turn start, scan transcript for `mcp()`/`skill()` calls → derive loaded set → build `tools` array
8. Inject MCP index + skill index into system prompt from `gait.jsonc` registered servers/skills

> Implementation target: Rust port. Do not implement in TypeScript.

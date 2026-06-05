# ainix Core — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build the `ainix` CLI binary — a stateless, turn-based AI coding agent harness that reads JSONL transcript state from disk, runs one LLM turn via Vercel AI SDK, appends results, and exits.

**Architecture:** Single TypeScript/Bun project compiled to a self-contained binary via `bun build --compile`. State lives in an append-only JSONL transcript per session. The turn loop uses Vercel AI SDK `streamText` with a permission gate that wraps tool execute functions; turns end by process exit. Auto mode loops internally until `turn_complete` or `questions`.

**Tech Stack:** Bun 1.x, TypeScript, Vercel AI SDK (`ai` + `@ai-sdk/*` packages), Zod 3, system git

> **Scope:** This plan covers the core `ainix` binary only. The `ainix-acp` adapter (long-lived ACP bridge process) is a separate plan — implement it after this binary is working.

---

## File Map

Every file that will be created, and what it owns:

```
ainix/
├── package.json                          # deps, bun scripts
├── tsconfig.json                         # strict TS config
├── ainix.jsonc                           # default workspace config (committed as example)
├── src/
│   ├── cli/
│   │   ├── index.ts                      # entry: parse top-level command, dispatch
│   │   └── cmd/
│   │       ├── auth.ts                   # ainix auth add|list|remove
│   │       ├── init.ts                   # ainix init [<repo-url>]
│   │       ├── models.ts                 # ainix models [provider]
│   │       └── run.ts                    # ainix run — the turn loop
│   ├── auth/
│   │   └── index.ts                      # file-based credential store (ported from OpenCode)
│   ├── provider/
│   │   ├── models.ts                     # models.dev fetch + hourly cache
│   │   └── provider.ts                   # BUNDLED_PROVIDERS, CUSTOM_LOADERS, getSDK, getLanguage
│   ├── session/
│   │   ├── transcript.ts                 # Zod event schemas + append/read helpers
│   │   ├── context.ts                    # reconstruct CoreMessage[] from JSONL transcript
│   │   └── compaction.ts                 # token estimation + LLM summarize + chunked fallback
│   ├── tools/
│   │   ├── index.ts                      # register all tools, export allTools map
│   │   ├── read.ts                       # read file with line ranges (ported from OpenCode)
│   │   ├── write.ts                      # write file (ported from OpenCode)
│   │   ├── edit.ts                       # string-replacement edit (ported from OpenCode)
│   │   ├── multiedit.ts                  # multi-file edits (ported from OpenCode)
│   │   ├── apply_patch.ts                # apply unified diff (ported from OpenCode)
│   │   ├── glob.ts                       # glob pattern find (ported from OpenCode)
│   │   ├── grep.ts                       # regex search files (ported from OpenCode)
│   │   ├── ls.ts                         # list directory (ported from OpenCode)
│   │   ├── codesearch.ts                 # ripgrep-based code search (ported from OpenCode)
│   │   ├── webfetch.ts                   # HTTP fetch + HTML strip (ported from OpenCode)
│   │   ├── shell.ts                      # permission-gated shell (ainix-specific)
│   │   ├── questions.ts                  # turn-ending questions signal
│   │   └── spawn_subagent.ts             # blocking subagent runner
│   ├── config.ts                         # load + validate ainix.jsonc
│   ├── permissions.ts                    # allow list check + tool wrapping
│   ├── git.ts                            # git add, commit, rev-parse, dirty check
│   ├── lock.ts                           # session PID lock file
│   └── stream.ts                         # DualStream: ANSI stdout + message buffering
└── bin/
    └── ainix.ts                          # bun build --compile entrypoint (re-exports cli/index.ts)
```

Test files live alongside source: `src/session/transcript.test.ts`, `src/session/context.test.ts`, `src/permissions.test.ts`, `src/config.test.ts`, `src/session/compaction.test.ts`.

---

## Task 1: Project scaffold

**Files:**
- Create: `ainix/package.json`
- Create: `ainix/tsconfig.json`
- Create: `ainix/bin/ainix.ts`
- Create: `ainix/src/cli/index.ts`

- [ ] **Step 1: Create project directory and package.json**

```bash
mkdir -p /Users/aicoder/Documents/ideation/ainix/src/cli/cmd
mkdir -p /Users/aicoder/Documents/ideation/ainix/src/auth
mkdir -p /Users/aicoder/Documents/ideation/ainix/src/provider
mkdir -p /Users/aicoder/Documents/ideation/ainix/src/session
mkdir -p /Users/aicoder/Documents/ideation/ainix/src/tools
mkdir -p /Users/aicoder/Documents/ideation/ainix/bin
cd /Users/aicoder/Documents/ideation/ainix
```

Write `package.json`:
```json
{
  "name": "ainix",
  "version": "0.1.0",
  "type": "module",
  "scripts": {
    "dev": "bun run src/cli/index.ts",
    "build": "bun build --compile --minify --target=bun bin/ainix.ts --outfile dist/ainix",
    "test": "bun test"
  },
  "dependencies": {
    "ai": "^4.3.0",
    "@ai-sdk/amazon-bedrock": "^2.2.0",
    "@ai-sdk/anthropic": "^1.2.0",
    "@ai-sdk/azure": "^1.3.0",
    "@ai-sdk/cerebras": "^0.2.0",
    "@ai-sdk/cohere": "^1.2.0",
    "@ai-sdk/deepinfra": "^0.2.0",
    "@ai-sdk/gateway": "^0.1.0",
    "@ai-sdk/google": "^1.2.0",
    "@ai-sdk/google-vertex": "^2.2.0",
    "@ai-sdk/groq": "^1.1.0",
    "@ai-sdk/mistral": "^1.2.0",
    "@ai-sdk/openai": "^1.3.0",
    "@ai-sdk/openai-compatible": "^0.2.0",
    "@ai-sdk/perplexity": "^1.1.0",
    "@ai-sdk/provider": "^1.1.0",
    "@ai-sdk/provider-utils": "^2.2.0",
    "@ai-sdk/togetherai": "^0.2.0",
    "@ai-sdk/vercel": "^1.1.0",
    "@ai-sdk/xai": "^1.1.0",
    "@gitlab/gitlab-ai-provider": "^0.1.0",
    "@openrouter/ai-sdk-provider": "^0.4.0",
    "zod": "^3.23.0"
  },
  "devDependencies": {
    "bun-types": "latest",
    "@types/node": "^20.0.0"
  }
}
```

- [ ] **Step 2: Create tsconfig.json**

```json
{
  "compilerOptions": {
    "target": "ESNext",
    "module": "ESNext",
    "moduleResolution": "bundler",
    "strict": true,
    "noUncheckedIndexedAccess": true,
    "lib": ["ESNext"],
    "types": ["bun-types"],
    "paths": {}
  },
  "include": ["src/**/*", "bin/**/*"]
}
```

- [ ] **Step 3: Create stub CLI entry**

`bin/ainix.ts`:
```typescript
import "./src/cli/index.ts"
```

`src/cli/index.ts`:
```typescript
const [, , cmd, ...args] = process.argv

switch (cmd) {
  case "init":
    console.log("init — not yet implemented")
    break
  case "run":
    console.log("run — not yet implemented")
    break
  case "auth":
    console.log("auth — not yet implemented")
    break
  case "models":
    console.log("models — not yet implemented")
    break
  default:
    console.log(`ainix <init|run|auth|models>`)
    process.exit(cmd ? 1 : 0)
}
```

- [ ] **Step 4: Install dependencies and verify**

```bash
cd /Users/aicoder/Documents/ideation/ainix
bun install
bun run dev -- --help
```

Expected output: `ainix <init|run|auth|models>`

- [ ] **Step 5: Verify bun test runner works**

Create `src/cli/index.test.ts`:
```typescript
import { describe, it, expect } from "bun:test"

describe("cli", () => {
  it("placeholder", () => {
    expect(true).toBe(true)
  })
})
```

Run: `bun test`
Expected: PASS

- [ ] **Step 6: Commit**

```bash
cd /Users/aicoder/Documents/ideation/ainix
git init
git add package.json tsconfig.json bin/ src/
git commit -m "feat: project scaffold"
```

---

## Task 2: Auth module

Port the credential store from OpenCode. Source: `/Users/aicoder/Documents/pocketcoder/.independent_repos/opencode_reference/packages/opencode/src/auth/index.ts`

**Files:**
- Create: `src/auth/index.ts`

The OpenCode auth module stores credentials at `~/.local/share/opencode/auth.json`. ainix uses `~/.local/share/ainix/auth.json`. The credential types and functions (`get`, `all`, `set`, `remove`, `OAUTH_DUMMY_KEY`) are identical.

- [ ] **Step 1: Read the OpenCode auth source**

```bash
cat /Users/aicoder/Documents/pocketcoder/.independent_repos/opencode_reference/packages/opencode/src/auth/index.ts
```

- [ ] **Step 2: Write `src/auth/index.ts`**

Copy the file verbatim, then make two changes:
1. Add the MIT copyright header comment at the top: `// Ported from OpenCode (MIT) — https://github.com/sst/opencode`
2. Change the storage path constant from `opencode` to `ainix`:

```typescript
// Ported from OpenCode (MIT) — https://github.com/sst/opencode
import path from "path"
// ... rest of file verbatim ...
// Change the path: replace "opencode" directory name with "ainix"
// e.g. ~/.local/share/opencode/auth.json → ~/.local/share/ainix/auth.json
```

The exact path constant is in the source file — find the line with `opencode` in the path and change it.

- [ ] **Step 3: Verify it compiles**

```bash
cd /Users/aicoder/Documents/ideation/ainix
bun build src/auth/index.ts --target=bun 2>&1
```

Expected: no errors

- [ ] **Step 4: Commit**

```bash
git add src/auth/
git commit -m "feat: port OpenCode auth module"
```

---

## Task 3: JSONL event schemas (Zod)

All 15 event types validated with Zod. This is the source of truth for the transcript format.

**Files:**
- Create: `src/session/transcript.ts`
- Create: `src/session/transcript.test.ts`

- [ ] **Step 1: Write failing tests first**

`src/session/transcript.test.ts`:
```typescript
import { describe, it, expect } from "bun:test"
import {
  parseEvent,
  SessionStartEvent,
  UserTurnEvent,
  AssistantTextEvent,
  ToolCallEvent,
  ToolResultEvent,
  CommandResultEvent,
  ErrorEvent,
  QuestionsEvent,
  QuestionsReplyEvent,
  PermissionsEvent,
  PermissionsReplyEvent,
  SubagentSpawnedEvent,
  SubagentCompleteEvent,
  TurnCompleteEvent,
  CompactionEvent,
  type AnyEvent,
} from "./transcript"

describe("JSONL event schemas", () => {
  it("parses a valid session_start event", () => {
    const raw = { type: "session_start", schema_version: 1, id: "s1", model: "anthropic/claude-sonnet-4-6", codebase_hash: "abc123", codebase_dirty: false, ts: 1234 }
    const event = parseEvent(raw)
    expect(event.type).toBe("session_start")
  })

  it("parses a permissions_reply with instruct decision", () => {
    const raw = { type: "permissions_reply", from: "user", items: [{ id: "p1", decision: "instruct", text: "use --no-cache" }], ts: 1234 }
    const event = parseEvent(raw) as PermissionsReplyEvent
    expect(event.items[0]!.decision).toBe("instruct")
    expect(event.items[0]!.text).toBe("use --no-cache")
  })

  it("rejects unknown event type", () => {
    expect(() => parseEvent({ type: "unknown_event", ts: 1234 })).toThrow()
  })

  it("rejects permissions_reply with invalid decision", () => {
    const raw = { type: "permissions_reply", from: "user", items: [{ id: "p1", decision: "maybe" }], ts: 1234 }
    expect(() => parseEvent(raw)).toThrow()
  })

  it("parses compaction event with usage", () => {
    const raw = { type: "compaction", summary: "did stuff", turns_covered: 3, from_ts: 100, to_ts: 200, usage: { input: 1000, output: 500 }, ts: 300 }
    const event = parseEvent(raw) as CompactionEvent
    expect(event.usage.input).toBe(1000)
  })

  it("parses subagent_complete with result", () => {
    const raw = { type: "subagent_complete", session_id: "s1-sub-abc", status: "done", result: "auth module extracted", ts: 1234 }
    const event = parseEvent(raw) as SubagentCompleteEvent
    expect(event.result).toBe("auth module extracted")
  })
})
```

Run: `bun test src/session/transcript.test.ts`
Expected: FAIL — "Cannot find module './transcript'"

- [ ] **Step 2: Write `src/session/transcript.ts`**

```typescript
import { z } from "zod"
import fs from "fs"

const base = z.object({ ts: z.number() })

export const SessionStartEvent = base.extend({
  type: z.literal("session_start"),
  schema_version: z.literal(1),
  id: z.string(),
  model: z.string(),
  codebase_hash: z.string(),
  codebase_dirty: z.boolean(),
})

export const UserTurnEvent = base.extend({
  type: z.literal("user_turn"),
  content: z.string(),
})

export const AssistantTextEvent = base.extend({
  type: z.literal("assistant_text"),
  content: z.string(),
})

export const ToolCallEvent = base.extend({
  type: z.literal("tool_call"),
  id: z.string(),
  name: z.string(),
  input: z.record(z.unknown()),
})

export const ToolResultEvent = base.extend({
  type: z.literal("tool_result"),
  id: z.string(),
  output: z.string(),
})

export const CommandResultEvent = base.extend({
  type: z.literal("command_result"),
  call_id: z.string(),
  cmd: z.string(),
  output: z.string(),
  exit_code: z.number(),
  duration_ms: z.number(),
  timed_out: z.boolean(),
})

export const ErrorEvent = base.extend({
  type: z.literal("error"),
  message: z.string(),
  phase: z.enum(["llm", "tool", "git", "init"]),
})

export const QuestionItem = z.object({ id: z.string(), content: z.string() })
export const QuestionsEvent = base.extend({
  type: z.literal("questions"),
  items: z.array(QuestionItem),
})

export const QuestionsReplyEvent = base.extend({
  type: z.literal("questions_reply"),
  from: z.string(), // "user" | "session:<id>" | "auto"
  items: z.array(QuestionItem),
})

export const PermissionItem = z.object({ id: z.string(), cmd: z.string(), reason: z.string() })
export const PermissionsEvent = base.extend({
  type: z.literal("permissions"),
  items: z.array(PermissionItem),
})

export const PermissionsReplyItem = z.object({
  id: z.string(),
  decision: z.enum(["approve", "deny", "instruct"]),
  text: z.string().optional(),
})
export const PermissionsReplyEvent = base.extend({
  type: z.literal("permissions_reply"),
  from: z.string(),
  items: z.array(PermissionsReplyItem),
})

export const SubagentSpawnedEvent = base.extend({
  type: z.literal("subagent_spawned"),
  session_id: z.string(),
  task: z.string(),
})

export const SubagentCompleteEvent = base.extend({
  type: z.literal("subagent_complete"),
  session_id: z.string(),
  status: z.enum(["done", "error"]),
  result: z.string(),
})

export const TurnCompleteEvent = base.extend({
  type: z.literal("turn_complete"),
  usage: z.object({
    input: z.number(),
    output: z.number(),
    cache_read: z.number(),
    cache_write: z.number(),
  }),
  codebase_hash: z.string(),
  codebase_dirty: z.boolean(),
})

export const CompactionEvent = base.extend({
  type: z.literal("compaction"),
  summary: z.string(),
  turns_covered: z.number(),
  from_ts: z.number(),
  to_ts: z.number(),
  usage: z.object({ input: z.number(), output: z.number() }),
})

export const AnyEventSchema = z.discriminatedUnion("type", [
  SessionStartEvent,
  UserTurnEvent,
  AssistantTextEvent,
  ToolCallEvent,
  ToolResultEvent,
  CommandResultEvent,
  ErrorEvent,
  QuestionsEvent,
  QuestionsReplyEvent,
  PermissionsEvent,
  PermissionsReplyEvent,
  SubagentSpawnedEvent,
  SubagentCompleteEvent,
  TurnCompleteEvent,
  CompactionEvent,
])

export type SessionStartEvent = z.infer<typeof SessionStartEvent>
export type UserTurnEvent = z.infer<typeof UserTurnEvent>
export type AssistantTextEvent = z.infer<typeof AssistantTextEvent>
export type ToolCallEvent = z.infer<typeof ToolCallEvent>
export type ToolResultEvent = z.infer<typeof ToolResultEvent>
export type CommandResultEvent = z.infer<typeof CommandResultEvent>
export type ErrorEvent = z.infer<typeof ErrorEvent>
export type QuestionsEvent = z.infer<typeof QuestionsEvent>
export type QuestionsReplyEvent = z.infer<typeof QuestionsReplyEvent>
export type PermissionsEvent = z.infer<typeof PermissionsEvent>
export type PermissionsReplyEvent = z.infer<typeof PermissionsReplyEvent>
export type SubagentSpawnedEvent = z.infer<typeof SubagentSpawnedEvent>
export type SubagentCompleteEvent = z.infer<typeof SubagentCompleteEvent>
export type TurnCompleteEvent = z.infer<typeof TurnCompleteEvent>
export type CompactionEvent = z.infer<typeof CompactionEvent>
export type AnyEvent = z.infer<typeof AnyEventSchema>

export function parseEvent(raw: unknown): AnyEvent {
  return AnyEventSchema.parse(raw)
}

// ── Transcript file helpers ──────────────────────────────────────────────────

export function appendEvent(transcriptPath: string, event: AnyEvent): void {
  fs.appendFileSync(transcriptPath, JSON.stringify(event) + "\n", "utf-8")
}

export function readEvents(transcriptPath: string): AnyEvent[] {
  if (!fs.existsSync(transcriptPath)) return []
  return fs.readFileSync(transcriptPath, "utf-8")
    .split("\n")
    .filter(Boolean)
    .map((line) => AnyEventSchema.parse(JSON.parse(line)))
}

export function lastEvent(transcriptPath: string): AnyEvent | null {
  const events = readEvents(transcriptPath)
  return events[events.length - 1] ?? null
}

export function countTurnComplete(events: AnyEvent[]): number {
  return events.filter((e) => e.type === "turn_complete").length
}

export function lastCompactionIndex(events: AnyEvent[]): number {
  for (let i = events.length - 1; i >= 0; i--) {
    if (events[i]!.type === "compaction") return i
  }
  return -1
}

// Initialize a new session transcript with session_start event
export function initSession(opts: {
  transcriptPath: string
  id: string
  model: string
  codebaseHash: string
  codebaseDirty: boolean
}): void {
  const event: SessionStartEvent = {
    type: "session_start",
    schema_version: 1,
    id: opts.id,
    model: opts.model,
    codebase_hash: opts.codebaseHash,
    codebase_dirty: opts.codebaseDirty,
    ts: Date.now(),
  }
  appendEvent(opts.transcriptPath, event)
}
```

- [ ] **Step 3: Run tests — verify they pass**

```bash
cd /Users/aicoder/Documents/ideation/ainix
bun test src/session/transcript.test.ts
```

Expected: all PASS

- [ ] **Step 4: Commit**

```bash
git add src/session/transcript.ts src/session/transcript.test.ts
git commit -m "feat: JSONL event schemas and transcript helpers"
```

---

## Task 4: Config loading

Load and validate `ainix.jsonc` with Zod. JSONC = JSON with comments (strip comments before parsing).

**Files:**
- Create: `src/config.ts`
- Create: `src/config.test.ts`

- [ ] **Step 1: Write failing tests**

`src/config.test.ts`:
```typescript
import { describe, it, expect } from "bun:test"
import { parseConfig } from "./config"

describe("config", () => {
  it("parses minimal config", () => {
    const cfg = parseConfig(`{ "model": "anthropic/claude-sonnet-4-6" }`)
    expect(cfg.model).toBe("anthropic/claude-sonnet-4-6")
    expect(cfg.allow).toEqual([])
    expect(cfg.tools).toEqual([])
  })

  it("parses allow list", () => {
    const cfg = parseConfig(`{ "model": "anthropic/claude-sonnet-4-6", "allow": ["read", "glob"] }`)
    expect(cfg.allow).toEqual(["read", "glob"])
  })

  it("parses tier 1 config-defined tools", () => {
    const cfg = parseConfig(`{
      "model": "anthropic/claude-sonnet-4-6",
      "tools": [{ "name": "run_tests", "description": "Run tests", "cmd": "bun test 2>&1" }]
    }`)
    expect(cfg.tools[0]!.name).toBe("run_tests")
    expect(cfg.tools[0]!.cwd).toBe("codebase")
  })

  it("strips JSON comments", () => {
    const cfg = parseConfig(`{
      // this is a comment
      "model": "anthropic/claude-sonnet-4-6" // inline comment
    }`)
    expect(cfg.model).toBe("anthropic/claude-sonnet-4-6")
  })

  it("rejects missing model", () => {
    expect(() => parseConfig(`{ "allow": [] }`)).toThrow()
  })

  it("rejects tool without cmd", () => {
    expect(() => parseConfig(`{
      "model": "anthropic/claude-sonnet-4-6",
      "tools": [{ "name": "run_tests", "description": "Run tests" }]
    }`)).toThrow()
  })
})
```

Run: `bun test src/config.test.ts`
Expected: FAIL — cannot find module

- [ ] **Step 2: Write `src/config.ts`**

```typescript
import { z } from "zod"
import fs from "fs"

const ConfigDefinedTool = z.object({
  name: z.string(),
  description: z.string(),
  cmd: z.string(),
  cwd: z.enum(["codebase", "workspace"]).default("codebase"),
})

const ConfigSchema = z.object({
  model: z.string(),
  allow: z.array(z.string()).default([]),
  tools: z.array(ConfigDefinedTool).default([]),
  provider: z.record(z.unknown()).default({}),
  compaction_threshold: z.number().min(0.1).max(1.0).default(0.8),
})

export type Config = z.infer<typeof ConfigSchema>
export type ConfigDefinedTool = z.infer<typeof ConfigDefinedTool>

// Strip // line comments and /* block comments */ from JSONC before parsing
function stripComments(jsonc: string): string {
  return jsonc
    .replace(/\/\/[^\n]*/g, "")
    .replace(/\/\*[\s\S]*?\*\//g, "")
}

export function parseConfig(jsonc: string): Config {
  const json = stripComments(jsonc)
  return ConfigSchema.parse(JSON.parse(json))
}

export function loadConfig(workspacePath: string): Config {
  const configPath = `${workspacePath}/ainix.jsonc`
  if (!fs.existsSync(configPath)) {
    throw new Error(`No ainix.jsonc found at ${configPath}. Run 'ainix init' first.`)
  }
  return parseConfig(fs.readFileSync(configPath, "utf-8"))
}
```

- [ ] **Step 3: Run tests**

```bash
bun test src/config.test.ts
```

Expected: all PASS

- [ ] **Step 4: Commit**

```bash
git add src/config.ts src/config.test.ts
git commit -m "feat: ainix.jsonc config loading with JSONC comment support"
```

---

## Task 5: Context reconstruction

Converts the JSONL transcript into `CoreMessage[]` for the Vercel AI SDK. Handles the compaction breakpoint, tool-call pairing, and all event-to-message mappings.

**Files:**
- Create: `src/session/context.ts`
- Create: `src/session/context.test.ts`

- [ ] **Step 1: Write failing tests**

`src/session/context.test.ts`:
```typescript
import { describe, it, expect, beforeEach, afterEach } from "bun:test"
import fs from "fs"
import os from "os"
import path from "path"
import { reconstructMessages } from "./context"
import { appendEvent } from "./transcript"

let tmpDir: string
let transcriptPath: string

beforeEach(() => {
  tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), "ainix-test-"))
  transcriptPath = path.join(tmpDir, "transcript.jsonl")
})

afterEach(() => {
  fs.rmSync(tmpDir, { recursive: true })
})

describe("reconstructMessages", () => {
  it("returns empty array for empty transcript", () => {
    fs.writeFileSync(transcriptPath, "")
    expect(reconstructMessages(transcriptPath)).toEqual([])
  })

  it("maps user_turn to user message", () => {
    appendEvent(transcriptPath, { type: "session_start", schema_version: 1, id: "s1", model: "m", codebase_hash: "h", codebase_dirty: false, ts: 1 })
    appendEvent(transcriptPath, { type: "user_turn", content: "hello", ts: 2 })
    const msgs = reconstructMessages(transcriptPath)
    expect(msgs).toEqual([{ role: "user", content: "hello" }])
  })

  it("maps assistant_text to assistant message", () => {
    appendEvent(transcriptPath, { type: "assistant_text", content: "world", ts: 1 })
    const msgs = reconstructMessages(transcriptPath)
    expect(msgs).toEqual([{ role: "assistant", content: "world" }])
  })

  it("pairs tool_call and tool_result by id", () => {
    appendEvent(transcriptPath, { type: "tool_call", id: "tc1", name: "read", input: { filePath: "foo.ts" }, ts: 1 })
    appendEvent(transcriptPath, { type: "tool_result", id: "tc1", output: "file content", ts: 2 })
    const msgs = reconstructMessages(transcriptPath)
    expect(msgs).toHaveLength(2)
    expect(msgs[0]!.role).toBe("assistant")
    expect(msgs[1]!.role).toBe("tool")
  })

  it("uses compaction as breakpoint — ignores events before it", () => {
    appendEvent(transcriptPath, { type: "user_turn", content: "old message", ts: 1 })
    appendEvent(transcriptPath, { type: "compaction", summary: "the summary", turns_covered: 1, from_ts: 0, to_ts: 100, usage: { input: 100, output: 50 }, ts: 200 })
    appendEvent(transcriptPath, { type: "user_turn", content: "new message", ts: 300 })
    const msgs = reconstructMessages(transcriptPath)
    // First message should be the compaction summary injected as context
    expect(msgs[0]!.content).toContain("the summary")
    // Old user_turn should be absent
    const hasOldMessage = msgs.some(m => typeof m.content === "string" && m.content.includes("old message"))
    expect(hasOldMessage).toBe(false)
    // New user_turn should be present
    const hasNewMessage = msgs.some(m => typeof m.content === "string" && m.content.includes("new message"))
    expect(hasNewMessage).toBe(true)
  })

  it("maps error event to user message", () => {
    appendEvent(transcriptPath, { type: "error", message: "LLM timed out", phase: "llm", ts: 1 })
    const msgs = reconstructMessages(transcriptPath)
    expect(msgs[0]!.role).toBe("user")
    expect(typeof msgs[0]!.content === "string" && msgs[0]!.content).toContain("error during llm")
  })

  it("groups adjacent tool_call+tool_result pairs into one assistant message", () => {
    appendEvent(transcriptPath, { type: "tool_call", id: "tc1", name: "read", input: {}, ts: 1 })
    appendEvent(transcriptPath, { type: "tool_call", id: "tc2", name: "glob", input: {}, ts: 2 })
    appendEvent(transcriptPath, { type: "tool_result", id: "tc1", output: "r1", ts: 3 })
    appendEvent(transcriptPath, { type: "tool_result", id: "tc2", output: "r2", ts: 4 })
    const msgs = reconstructMessages(transcriptPath)
    // One assistant message with two tool-calls, one tool message with two results
    expect(msgs).toHaveLength(2)
    expect(Array.isArray(msgs[0]!.content)).toBe(true)
    expect(Array.isArray(msgs[1]!.content)).toBe(true)
  })
})
```

Run: `bun test src/session/context.test.ts`
Expected: FAIL — cannot find module

- [ ] **Step 2: Write `src/session/context.ts`**

```typescript
import type { CoreMessage, CoreAssistantMessage, CoreToolMessage } from "ai"
import { readEvents, lastCompactionIndex, type AnyEvent } from "./transcript"

export function reconstructMessages(transcriptPath: string): CoreMessage[] {
  const allEvents = readEvents(transcriptPath)
  if (allEvents.length === 0) return []

  // Find the last compaction breakpoint
  const breakpoint = lastCompactionIndex(allEvents)
  const events = breakpoint >= 0 ? allEvents.slice(breakpoint) : allEvents

  const messages: CoreMessage[] = []

  // Collect all tool_call and tool_result events for pairing
  const toolCalls = new Map<string, { name: string; input: Record<string, unknown>; ts: number }>()
  const toolResults = new Map<string, { output: string; ts: number }>()
  for (const e of events) {
    if (e.type === "tool_call") toolCalls.set(e.id, { name: e.name, input: e.input, ts: e.ts })
    if (e.type === "tool_result") toolResults.set(e.id, { output: e.output, ts: e.ts })
  }

  // Group adjacent tool_calls (same timestamp cluster) into one assistant message + one tool message
  // Strategy: walk events, batch consecutive tool_call/tool_result sequences
  let i = 0
  while (i < events.length) {
    const e = events[i]!

    if (e.type === "compaction") {
      // Inject summary as context
      messages.push({ role: "user", content: `[Session history summary: ${e.summary}]` })
      i++
      continue
    }

    if (e.type === "session_start") { i++; continue }
    if (e.type === "subagent_spawned" || e.type === "subagent_complete") { i++; continue }

    if (e.type === "user_turn") {
      messages.push({ role: "user", content: e.content })
      i++
      continue
    }

    if (e.type === "assistant_text") {
      messages.push({ role: "assistant", content: e.content })
      i++
      continue
    }

    if (e.type === "tool_call") {
      // Gather all consecutive tool_call events (parallel tool calls in one step)
      const callBatch: typeof e[] = []
      while (i < events.length && events[i]!.type === "tool_call") {
        callBatch.push(events[i] as typeof e)
        i++
      }
      // Skip tool_result events for this batch (they follow)
      const resultBatch: Array<{ id: string; output: string }> = []
      while (i < events.length && events[i]!.type === "tool_result") {
        const r = events[i] as { type: "tool_result"; id: string; output: string; ts: number }
        resultBatch.push({ id: r.id, output: r.output })
        i++
      }

      const assistantContent: CoreAssistantMessage["content"] = callBatch.map((c) => ({
        type: "tool-call" as const,
        toolCallId: c.id,
        toolName: c.name,
        args: c.input,
      }))
      messages.push({ role: "assistant", content: assistantContent })

      if (resultBatch.length > 0) {
        const toolContent: CoreToolMessage["content"] = resultBatch.map((r) => ({
          type: "tool-result" as const,
          toolCallId: r.id,
          result: r.output,
        }))
        messages.push({ role: "tool", content: toolContent })
      }
      continue
    }

    if (e.type === "tool_result") {
      // Orphan tool_result (paired call already handled) — skip
      i++
      continue
    }

    if (e.type === "questions") {
      const questionText = e.items.map((q) => `${q.id}: ${q.content}`).join("\n")
      messages.push({ role: "assistant", content: `questions:\n${questionText}` })
      i++
      continue
    }

    if (e.type === "questions_reply") {
      const replyText = e.items.map((q) => `${q.id}: ${q.content}`).join("\n")
      messages.push({ role: "user", content: `[from: ${e.from}] replies:\n${replyText}` })
      i++
      continue
    }

    if (e.type === "permissions") {
      const permText = e.items.map((p) => `${p.id}: ${p.cmd} (${p.reason})`).join("\n")
      messages.push({ role: "assistant", content: `permission request:\n${permText}` })
      i++
      continue
    }

    if (e.type === "permissions_reply") {
      const lines = e.items.map((p) => {
        if (p.decision === "instruct") return `${p.id}: instruct — ${p.text ?? ""}`
        return `${p.id}: ${p.decision}`
      })
      messages.push({ role: "user", content: `[from: ${e.from}] ${lines.join(", ")}` })
      i++
      continue
    }

    if (e.type === "command_result") {
      // Fold into preceding permissions_reply — prepend to last user message if it's a permissions_reply
      const last = messages[messages.length - 1]
      const cmdOut = `\ncommand output (exit ${e.exit_code}):\n${e.output}`
      if (last && last.role === "user" && typeof last.content === "string") {
        last.content += cmdOut
      } else {
        messages.push({ role: "user", content: cmdOut })
      }
      i++
      continue
    }

    if (e.type === "error") {
      messages.push({ role: "user", content: `error during ${e.phase}: ${e.message}` })
      i++
      continue
    }

    if (e.type === "turn_complete") { i++; continue }

    i++
  }

  return messages
}
```

- [ ] **Step 3: Run tests**

```bash
bun test src/session/context.test.ts
```

Expected: all PASS

- [ ] **Step 4: Commit**

```bash
git add src/session/context.ts src/session/context.test.ts
git commit -m "feat: transcript-to-messages reconstruction"
```

---

## Task 6: DualStream

Streams LLM tokens to ANSI stdout live while buffering a complete message for the transcript.

**Files:**
- Create: `src/stream.ts`

- [ ] **Step 1: Write `src/stream.ts`**

No external deps — Bun's `process.stdout` is already available.

```typescript
export class DualStream {
  private buffer = ""

  writeToken(token: string): void {
    process.stdout.write(`\x1b[36m${token}\x1b[0m`)
    this.buffer += token
  }

  // Call when the AI SDK signals message boundary (onStepFinish or stream end)
  // Returns the complete buffered message, resets buffer
  flushMessage(): string {
    const content = this.buffer
    this.buffer = ""
    return content
  }

  // Print a status line in dim gray (not buffered to transcript)
  status(msg: string): void {
    process.stderr.write(`\x1b[2m${msg}\x1b[0m\n`)
  }

  // Print a permission prompt line
  prompt(msg: string): void {
    process.stdout.write(`\x1b[33m${msg}\x1b[0m`)
  }
}
```

- [ ] **Step 2: Verify it compiles**

```bash
bun build src/stream.ts --target=bun 2>&1
```

Expected: no errors

- [ ] **Step 3: Commit**

```bash
git add src/stream.ts
git commit -m "feat: DualStream ANSI stdout + message buffering"
```

---

## Task 7: Git operations

All git interactions wrapped in one module. ainix only commits the parent workspace repo — never the codebase submodule.

**Files:**
- Create: `src/git.ts`

- [ ] **Step 1: Write `src/git.ts`**

```typescript
import { execSync, spawnSync } from "child_process"
import path from "path"

function run(cmd: string, cwd: string): string {
  return execSync(cmd, { cwd, encoding: "utf-8" }).trim()
}

export function checkGitInstalled(): void {
  try {
    run("git --version", process.cwd())
  } catch {
    throw new Error("git is not installed or not in PATH. ainix requires git.")
  }
}

export function getCodebaseHash(codebasePath: string): string {
  try {
    return run("git rev-parse HEAD", codebasePath)
  } catch {
    return "none"
  }
}

export function isCodebaseDirty(codebasePath: string): boolean {
  try {
    return run("git status --porcelain", codebasePath).length > 0
  } catch {
    return false
  }
}

export function commitSession(workspacePath: string, sessionId: string, message: string): void {
  try {
    run(`git add sessions/${sessionId}/`, workspacePath)
    run(`git commit -m "${message.replace(/"/g, '\\"')}"`, workspacePath)
  } catch (err) {
    throw new Error(`git commit failed: ${err}`)
  }
}

// Used by ainix init
export function initWorkspace(workspacePath: string): void {
  try {
    run("git init", workspacePath)
  } catch {
    // already a git repo — fine
  }
}

export function addSubmodule(workspacePath: string, repoUrl: string): void {
  run(`git submodule add "${repoUrl}" codebase`, workspacePath)
}

export function initialCommit(workspacePath: string): void {
  run("git add -A", workspacePath)
  run('git commit -m "ainix: init workspace"', workspacePath)
}
```

- [ ] **Step 2: Verify it compiles**

```bash
bun build src/git.ts --target=bun 2>&1
```

Expected: no errors

- [ ] **Step 3: Commit**

```bash
git add src/git.ts
git commit -m "feat: git operations module"
```

---

## Task 8: Session locking

PID-file lock preventing two concurrent `ainix run` for the same session.

**Files:**
- Create: `src/lock.ts`

- [ ] **Step 1: Write `src/lock.ts`**

```typescript
import fs from "fs"
import path from "path"

function lockPath(sessionDir: string): string {
  return path.join(sessionDir, "lock")
}

function isProcessAlive(pid: number): boolean {
  try {
    process.kill(pid, 0)
    return true
  } catch {
    return false
  }
}

export function acquireLock(sessionDir: string): void {
  const lp = lockPath(sessionDir)
  if (fs.existsSync(lp)) {
    const pid = parseInt(fs.readFileSync(lp, "utf-8").trim(), 10)
    if (!isNaN(pid) && isProcessAlive(pid)) {
      throw new Error(`Session is already running (PID ${pid}). If this is wrong, delete ${lp}`)
    }
    // Stale lock — remove it
    fs.unlinkSync(lp)
  }
  fs.writeFileSync(lp, String(process.pid), "utf-8")
}

export function releaseLock(sessionDir: string): void {
  const lp = lockPath(sessionDir)
  try {
    fs.unlinkSync(lp)
  } catch {
    // already gone — fine
  }
}
```

- [ ] **Step 2: Commit**

```bash
git add src/lock.ts
git commit -m "feat: session PID lock"
```

---

## Task 9: Provider & Models cache

Models.dev cache and the BUNDLED_PROVIDERS extraction from OpenCode.

**Files:**
- Create: `src/provider/models.ts`
- Create: `src/provider/provider.ts`

- [ ] **Step 1: Read the OpenCode provider source**

```bash
head -100 /Users/aicoder/Documents/pocketcoder/.independent_repos/opencode_reference/packages/opencode/src/provider/provider.ts
```

- [ ] **Step 2: Write `src/provider/models.ts`**

```typescript
import fs from "fs"
import path from "path"
import os from "os"

const MODELS_URL = "https://models.dev/api.json"
const CACHE_DIR = path.join(os.homedir(), ".cache", "ainix")
const CACHE_FILE = path.join(CACHE_DIR, "models.json")
const CACHE_TTL_MS = 60 * 60 * 1000 // 1 hour

export interface ModelInfo {
  id: string
  name: string
  provider: string
  contextLength?: number
  [key: string]: unknown
}

async function fetchModels(): Promise<ModelInfo[]> {
  const res = await fetch(MODELS_URL)
  if (!res.ok) throw new Error(`models.dev returned ${res.status}`)
  const data = await res.json() as unknown
  // models.dev returns an array of model objects
  if (!Array.isArray(data)) throw new Error("Unexpected models.dev response shape")
  return data as ModelInfo[]
}

export async function getModels(): Promise<ModelInfo[]> {
  fs.mkdirSync(CACHE_DIR, { recursive: true })

  // Check cache freshness
  if (fs.existsSync(CACHE_FILE)) {
    const stat = fs.statSync(CACHE_FILE)
    const age = Date.now() - stat.mtimeMs
    if (age < CACHE_TTL_MS) {
      return JSON.parse(fs.readFileSync(CACHE_FILE, "utf-8")) as ModelInfo[]
    }
  }

  // Fetch fresh
  try {
    const models = await fetchModels()
    fs.writeFileSync(CACHE_FILE, JSON.stringify(models, null, 2), "utf-8")
    return models
  } catch (err) {
    // Offline fallback: use stale cache
    if (fs.existsSync(CACHE_FILE)) {
      process.stderr.write(`[warn] models.dev unreachable, using cached model list\n`)
      return JSON.parse(fs.readFileSync(CACHE_FILE, "utf-8")) as ModelInfo[]
    }
    throw new Error(
      `Cannot fetch model list and no cache exists. Specify "model" directly in ainix.jsonc. (${err})`
    )
  }
}

export async function getModelContextLength(modelId: string): Promise<number | null> {
  try {
    const models = await getModels()
    const [provider, ...rest] = modelId.split("/")
    const name = rest.join("/")
    const found = models.find(
      (m) => m.id === name || m.id === modelId ||
        (m.provider === provider && (m.id === name || m.name === name))
    )
    return typeof found?.contextLength === "number" ? found.contextLength : null
  } catch {
    return null
  }
}
```

- [ ] **Step 3: Write `src/provider/provider.ts`**

Read the OpenCode source first, then extract:

```bash
grep -n "BUNDLED_PROVIDERS\|CUSTOM_LOADERS\|getSDK\|getLanguage" \
  /Users/aicoder/Documents/pocketcoder/.independent_repos/opencode_reference/packages/opencode/src/provider/provider.ts | head -30
```

Then create `src/provider/provider.ts` with the following structure — paste the `BUNDLED_PROVIDERS` array and `CUSTOM_LOADERS` map verbatim from OpenCode (MIT — add copyright header), then write a slimmed `getSDK` that resolves a `"provider/model"` string to a Vercel AI SDK language model:

```typescript
// Ported from OpenCode (MIT) — https://github.com/sst/opencode
import { createOpenAI } from "@ai-sdk/openai"
import { createAnthropic } from "@ai-sdk/anthropic"
import { createGoogle } from "@ai-sdk/google"
// ... import all @ai-sdk/* factory functions ...
import type { LanguageModel } from "ai"

// Paste BUNDLED_PROVIDERS array from OpenCode verbatim here
// Each entry: { id: string, name: string, env: string[], ... }
export const BUNDLED_PROVIDERS = [
  // ... paste from OpenCode source ...
]

// Paste CUSTOM_LOADERS map from OpenCode verbatim here
// Maps provider id to a factory function (apiKey) => LanguageModel
const CUSTOM_LOADERS: Record<string, (apiKey: string, model: string, extra?: Record<string, unknown>) => LanguageModel> = {
  // ... paste from OpenCode source ...
}

export function getSDK(modelString: string, apiKey?: string): LanguageModel {
  const [providerId, ...rest] = modelString.split("/")
  if (!providerId || rest.length === 0) {
    throw new Error(`Invalid model format "${modelString}". Use "provider/model-id".`)
  }
  const modelId = rest.join("/")

  const loader = CUSTOM_LOADERS[providerId]
  if (!loader) throw new Error(`Unknown provider "${providerId}". Check ainix models.`)

  const key = apiKey ?? process.env[`${providerId.toUpperCase()}_API_KEY`] ?? ""
  return loader(key, modelId)
}
```

The actual BUNDLED_PROVIDERS and CUSTOM_LOADERS content must be read from the OpenCode source — the above is the structure. Copy those sections verbatim.

- [ ] **Step 4: Verify provider compiles**

```bash
bun build src/provider/provider.ts --target=bun 2>&1
```

Expected: no errors (may need to adjust imports based on what's in the OpenCode source)

- [ ] **Step 5: Commit**

```bash
git add src/provider/
git commit -m "feat: provider SDK + models.dev cache (ported from OpenCode)"
```

---

## Task 10: Permission gate

Wraps all tool execute functions to enforce the allow list. Non-allowed tools are collected as pending permissions rather than executed.

**Files:**
- Create: `src/permissions.ts`
- Create: `src/permissions.test.ts`

- [ ] **Step 1: Write failing tests**

`src/permissions.test.ts`:
```typescript
import { describe, it, expect } from "bun:test"
import { isAllowed, wrapTools } from "./permissions"
import { tool } from "ai"
import { z } from "zod"

const mockTool = tool({
  description: "test tool",
  parameters: z.object({ x: z.string() }),
  execute: async ({ x }) => `executed: ${x}`,
})

describe("isAllowed", () => {
  it("returns true when tool is in allow list", () => {
    expect(isAllowed("read", ["read", "glob"])).toBe(true)
  })

  it("returns false when tool is not in allow list", () => {
    expect(isAllowed("write", ["read", "glob"])).toBe(false)
  })

  it("returns false for empty allow list", () => {
    expect(isAllowed("read", [])).toBe(false)
  })
})

describe("wrapTools", () => {
  it("executes allowed tools immediately", async () => {
    const pending: any[] = []
    const wrapped = wrapTools({ read: mockTool }, ["read"], pending, "codebase/")
    const result = await wrapped["read"]!.execute!({ x: "hello" }, {} as any)
    expect(result).toBe("executed: hello")
    expect(pending).toHaveLength(0)
  })

  it("collects non-allowed tools into pending without executing", async () => {
    const pending: any[] = []
    const wrapped = wrapTools({ write: mockTool }, [], pending, "codebase/")
    const result = await wrapped["write"]!.execute!({ x: "hello" }, {} as any)
    expect(result).toContain("[permission required")
    expect(pending).toHaveLength(1)
    expect(pending[0].name).toBe("write")
  })
})
```

Run: `bun test src/permissions.test.ts`
Expected: FAIL — cannot find module

- [ ] **Step 2: Write `src/permissions.ts`**

```typescript
import { tool, type CoreTool } from "ai"
import crypto from "crypto"

export interface PendingPermission {
  id: string
  name: string
  input: Record<string, unknown>
  reason: string
}

export function isAllowed(toolName: string, allowList: string[]): boolean {
  return allowList.includes(toolName)
}

// Wraps all tools so that non-allowed ones are batched into `pending` and
// return a sentinel value, while allowed ones execute normally.
// The turn loop aborts streamText when pending becomes non-empty.
export function wrapTools(
  tools: Record<string, CoreTool>,
  allowList: string[],
  pending: PendingPermission[],
  codebasePath: string,
): Record<string, CoreTool> {
  return Object.fromEntries(
    Object.entries(tools).map(([name, t]) => [
      name,
      tool({
        description: t.description ?? "",
        parameters: t.parameters,
        execute: async (args, opts) => {
          if (isAllowed(name, allowList)) {
            // Allowed: delegate to original execute
            return await (t as any).execute(args, opts)
          } else {
            // Not allowed: add to pending batch, return sentinel
            const id = crypto.randomUUID()
            pending.push({
              id,
              name,
              input: args as Record<string, unknown>,
              reason: `LLM requested ${name}`,
            })
            return `[permission required for ${name} — awaiting approval]`
          }
        },
      }),
    ])
  )
}
```

- [ ] **Step 3: Run tests**

```bash
bun test src/permissions.test.ts
```

Expected: all PASS

- [ ] **Step 4: Commit**

```bash
git add src/permissions.ts src/permissions.test.ts
git commit -m "feat: permission gate — allow list wrapping"
```

---

## Task 11: Port OpenCode tools to Vercel AI SDK format

OpenCode tools use `Tool.define()` with their own context system (`Instance`, `LSP`, etc.). Port the core file-system logic to Vercel AI SDK's `tool({ description, parameters, execute })` format. Each tool's `execute` returns a string.

**Files:**
- Create: `src/tools/read.ts`, `write.ts`, `edit.ts`, `multiedit.ts`, `apply_patch.ts`, `glob.ts`, `grep.ts`, `ls.ts`, `codesearch.ts`, `webfetch.ts`
- Create: `src/tools/index.ts`

Source to read first:
```bash
ls /Users/aicoder/Documents/pocketcoder/.independent_repos/opencode_reference/packages/opencode/src/tool/
```

For each tool, the porting pattern is:
1. Copy the `parameters` Zod schema verbatim
2. Copy the core file-system logic from `execute`
3. Replace `Instance.directory` → `codebasePath` parameter (passed at tool-creation time)
4. Remove `ctx.ask()` calls (permission gate handles this in ainix)
5. Remove `LSP`, `FileTime`, `InstructionPrompt` references
6. Return `result.output` as a plain string

- [ ] **Step 1: Port `read.ts`**

Read the OpenCode source:
```bash
cat /Users/aicoder/Documents/pocketcoder/.independent_repos/opencode_reference/packages/opencode/src/tool/read.ts
```

Write `src/tools/read.ts`:
```typescript
// Ported from OpenCode (MIT) — https://github.com/sst/opencode
import { tool } from "ai"
import { z } from "zod"
import fs from "fs"
import path from "path"

const DEFAULT_READ_LIMIT = 2000
const MAX_LINE_LENGTH = 2000
const MAX_BYTES = 50 * 1024

export function createReadTool(codebasePath: string) {
  return tool({
    description: "Read a file from the codebase, optionally with line range. Paths are relative to the codebase root.",
    parameters: z.object({
      filePath: z.string().describe("Path to the file (relative to codebase root, or absolute)"),
      offset: z.coerce.number().optional().describe("Line number to start reading from (0-based)"),
      limit: z.coerce.number().optional().describe("Number of lines to read (default 2000)"),
    }),
    execute: async ({ filePath, offset = 0, limit = DEFAULT_READ_LIMIT }) => {
      const resolved = path.isAbsolute(filePath) ? filePath : path.join(codebasePath, filePath)

      if (!fs.existsSync(resolved)) {
        const dir = path.dirname(resolved)
        const base = path.basename(resolved)
        const nearby = fs.existsSync(dir)
          ? fs.readdirSync(dir).filter(e => e.toLowerCase().includes(base.toLowerCase())).slice(0, 3)
          : []
        const suggestions = nearby.length > 0 ? `\nDid you mean:\n${nearby.map(e => path.join(dir, e)).join("\n")}` : ""
        throw new Error(`File not found: ${resolved}${suggestions}`)
      }

      const lines = fs.readFileSync(resolved, "utf-8").split("\n")
      const slice = lines.slice(offset, offset + limit)
      const content = slice.map((line, i) => {
        const lineNum = (i + offset + 1).toString().padStart(5, "0")
        const truncated = line.length > MAX_LINE_LENGTH ? line.slice(0, MAX_LINE_LENGTH) + "..." : line
        return `${lineNum}| ${truncated}`
      }).join("\n")

      const hasMore = lines.length > offset + slice.length
      const footer = hasMore
        ? `\n\n(File has more lines. Use 'offset' to read beyond line ${offset + slice.length})`
        : `\n\n(End of file — total ${lines.length} lines)`

      return `<file>\n${content}${footer}\n</file>`
    },
  })
}
```

- [ ] **Step 2: Port remaining tools**

For each tool below, read the OpenCode source then write the ainix port using the same pattern as `read.ts`.

```bash
cat /Users/aicoder/Documents/pocketcoder/.independent_repos/opencode_reference/packages/opencode/src/tool/write.ts
cat /Users/aicoder/Documents/pocketcoder/.independent_repos/opencode_reference/packages/opencode/src/tool/edit.ts
cat /Users/aicoder/Documents/pocketcoder/.independent_repos/opencode_reference/packages/opencode/src/tool/multiedit.ts
cat /Users/aicoder/Documents/pocketcoder/.independent_repos/opencode_reference/packages/opencode/src/tool/apply_patch.ts
cat /Users/aicoder/Documents/pocketcoder/.independent_repos/opencode_reference/packages/opencode/src/tool/glob.ts
cat /Users/aicoder/Documents/pocketcoder/.independent_repos/opencode_reference/packages/opencode/src/tool/grep.ts
cat /Users/aicoder/Documents/pocketcoder/.independent_repos/opencode_reference/packages/opencode/src/tool/ls.ts
cat /Users/aicoder/Documents/pocketcoder/.independent_repos/opencode_reference/packages/opencode/src/tool/codesearch.ts
cat /Users/aicoder/Documents/pocketcoder/.independent_repos/opencode_reference/packages/opencode/src/tool/webfetch.ts
```

Each becomes `createXxxTool(codebasePath: string)` returning a Vercel AI SDK `tool()`. The description text files (`.txt` sidecar) can be used as description strings.

Rules for each port:
- `write.ts`: create parent dirs, write content, return "wrote N bytes to path"
- `edit.ts`: string replacement edit — read file, replace `oldString` with `newString`, throw if not found or ambiguous
- `multiedit.ts`: apply multiple edits in sequence using `edit` logic
- `apply_patch.ts`: parse unified diff, apply hunks
- `glob.ts`: use Bun's `Bun.Glob` or Node's `fs` + recursive walk with pattern matching
- `grep.ts`: recursive file search with `ripgrep` if available, fallback to Node fs walk + regex
- `ls.ts`: `fs.readdirSync` with stat info
- `codesearch.ts`: ripgrep-based, fallback to grep
- `webfetch.ts`: `fetch()`, strip HTML tags for readability

- [ ] **Step 3: Write `src/tools/index.ts`**

```typescript
import { createReadTool } from "./read"
import { createWriteTool } from "./write"
import { createEditTool } from "./edit"
import { createMultieditTool } from "./multiedit"
import { createApplyPatchTool } from "./apply_patch"
import { createGlobTool } from "./glob"
import { createGrepTool } from "./grep"
import { createLsTool } from "./ls"
import { createCodesearchTool } from "./codesearch"
import { createWebfetchTool } from "./webfetch"
import { createShellTool } from "./shell"
import { createQuestionsTool } from "./questions"
import type { CoreTool } from "ai"

export function createBuiltinTools(codebasePath: string): Record<string, CoreTool> {
  return {
    read: createReadTool(codebasePath),
    write: createWriteTool(codebasePath),
    edit: createEditTool(codebasePath),
    multiedit: createMultieditTool(codebasePath),
    apply_patch: createApplyPatchTool(codebasePath),
    glob: createGlobTool(codebasePath),
    grep: createGrepTool(codebasePath),
    ls: createLsTool(codebasePath),
    codesearch: createCodesearchTool(codebasePath),
    webfetch: createWebfetchTool(),
    shell: createShellTool(codebasePath),
    questions: createQuestionsTool(),
  }
}
```

- [ ] **Step 4: Verify compilation**

```bash
bun build src/tools/index.ts --target=bun 2>&1
```

Expected: no errors

- [ ] **Step 5: Commit**

```bash
git add src/tools/
git commit -m "feat: port OpenCode tools to Vercel AI SDK format"
```

---

## Task 12: Shell tool and questions signal

**Files:**
- Create: `src/tools/shell.ts`
- Create: `src/tools/questions.ts`

- [ ] **Step 1: Write `src/tools/shell.ts`**

```typescript
// ainix-specific: permission-gated shell command runner
import { tool } from "ai"
import { z } from "zod"
import { execSync } from "child_process"

export function createShellTool(codebasePath: string) {
  return tool({
    description: "Run a shell command in the codebase directory. All shell commands require permission approval unless pre-allowed in ainix.jsonc.",
    parameters: z.object({
      cmd: z.string().describe("Shell command to execute"),
      reason: z.string().describe("Why this command is needed"),
      timeout_ms: z.number().optional().default(30_000).describe("Timeout in milliseconds (default 30s)"),
    }),
    execute: async ({ cmd, timeout_ms = 30_000 }) => {
      const start = Date.now()
      try {
        const output = execSync(cmd, {
          cwd: codebasePath,
          timeout: timeout_ms,
          encoding: "utf-8",
          stdio: ["ignore", "pipe", "pipe"],
        })
        const duration = Date.now() - start
        return `exit 0 (${duration}ms)\n${output}`
      } catch (err: any) {
        const duration = Date.now() - start
        const output = (err.stdout ?? "") + (err.stderr ?? "")
        const timed_out = err.code === "ETIMEDOUT"
        const exit_code = err.status ?? 1
        if (timed_out) return `TIMED OUT after ${timeout_ms}ms\n${output}`
        return `exit ${exit_code} (${duration}ms)\n${output}`
      }
    },
  })
}
```

- [ ] **Step 2: Write `src/tools/questions.ts`**

The `questions` tool is a turn-ending signal. Its execute sets a flag — the turn loop aborts after the step.

```typescript
import { tool } from "ai"
import { z } from "zod"

export interface QuestionsSignal {
  items: Array<{ id: string; content: string }>
}

// Shared mutable state per turn — reset at turn start
let _signal: QuestionsSignal | null = null

export function resetQuestionsSignal(): void {
  _signal = null
}

export function getQuestionsSignal(): QuestionsSignal | null {
  return _signal
}

export function createQuestionsTool() {
  return tool({
    description: "Ask the user one or more questions to clarify the task. Use this when you need information before proceeding. This ends the current turn — the user will reply in the next invocation.",
    parameters: z.object({
      items: z.array(z.object({
        id: z.string().describe("Short identifier for this question, e.g. 'q1'"),
        content: z.string().describe("The question text"),
      })).describe("Questions to ask the user"),
    }),
    execute: async ({ items }) => {
      _signal = { items }
      return `Questions queued. Turn will end after this step.`
    },
  })
}
```

- [ ] **Step 3: Commit**

```bash
git add src/tools/shell.ts src/tools/questions.ts
git commit -m "feat: shell tool and questions turn-ending signal"
```

---

## Task 13: Compaction

Token estimation + LLM summarize call + chunked fallback for overflow.

**Files:**
- Create: `src/session/compaction.ts`
- Create: `src/session/compaction.test.ts`

- [ ] **Step 1: Write failing tests**

`src/session/compaction.test.ts`:
```typescript
import { describe, it, expect } from "bun:test"
import { estimateTokens, needsCompaction, chunkEvents } from "./compaction"
import type { AnyEvent } from "./transcript"

const makeEvent = (content: string): AnyEvent => ({
  type: "user_turn",
  content,
  ts: Date.now(),
})

describe("estimateTokens", () => {
  it("estimates 1 token per ~3.5 chars", () => {
    const tokens = estimateTokens("hello world") // 11 chars
    expect(tokens).toBeGreaterThan(2)
    expect(tokens).toBeLessThan(5)
  })

  it("handles empty string", () => {
    expect(estimateTokens("")).toBe(0)
  })
})

describe("needsCompaction", () => {
  it("returns false when under threshold", () => {
    const events: AnyEvent[] = [makeEvent("short")]
    expect(needsCompaction(events, 100_000, 0.8)).toBe(false)
  })

  it("returns true when over threshold", () => {
    const longContent = "x".repeat(100_000)
    const events: AnyEvent[] = [makeEvent(longContent)]
    expect(needsCompaction(events, 10_000, 0.8)).toBe(true)
  })
})

describe("chunkEvents", () => {
  it("splits events into chunks by token limit", () => {
    const events = Array.from({ length: 10 }, (_, i) => makeEvent(`message ${i}`))
    const chunks = chunkEvents(events, 20) // tiny limit forces splits
    expect(chunks.length).toBeGreaterThan(1)
    expect(chunks.flat()).toHaveLength(events.length)
  })

  it("returns single chunk when events fit", () => {
    const events = [makeEvent("short")]
    const chunks = chunkEvents(events, 10_000)
    expect(chunks).toHaveLength(1)
  })
})
```

Run: `bun test src/session/compaction.test.ts`
Expected: FAIL — cannot find module

- [ ] **Step 2: Write `src/session/compaction.ts`**

```typescript
import type { LanguageModel } from "ai"
import { generateText } from "ai"
import type { AnyEvent, CompactionEvent } from "./transcript"

// Rough token estimation: ~3.5 chars per token (average across code + prose)
export function estimateTokens(text: string): number {
  return Math.ceil(text.length / 3.5)
}

export function estimateEventsTokens(events: AnyEvent[]): number {
  return estimateTokens(events.map((e) => JSON.stringify(e)).join("\n"))
}

export function needsCompaction(
  events: AnyEvent[],
  contextWindow: number,
  threshold: number,
): boolean {
  const tokens = estimateEventsTokens(events)
  return tokens > contextWindow * threshold
}

// Split events into chunks that each fit within maxTokens
export function chunkEvents(events: AnyEvent[], maxTokens: number): AnyEvent[][] {
  const chunks: AnyEvent[][] = []
  let current: AnyEvent[] = []
  let currentTokens = 0

  for (const event of events) {
    const t = estimateTokens(JSON.stringify(event))
    if (currentTokens + t > maxTokens && current.length > 0) {
      chunks.push(current)
      current = []
      currentTokens = 0
    }
    current.push(event)
    currentTokens += t
  }
  if (current.length > 0) chunks.push(current)
  return chunks
}

async function summarizeChunk(model: LanguageModel, events: AnyEvent[]): Promise<string> {
  const transcript = events.map((e) => JSON.stringify(e)).join("\n")
  const { text } = await generateText({
    model,
    messages: [
      {
        role: "user",
        content: `Summarize the following AI agent session transcript into a dense, factual paragraph. Focus on: what was being worked on, what code changes were made, what decisions were reached, and any open issues. Do not include meta-commentary. Transcript:\n\n${transcript}`,
      },
    ],
  })
  return text
}

// Returns the compaction summary and token usage
export async function compact(opts: {
  model: LanguageModel
  events: AnyEvent[]         // events to summarize (since last compaction)
  contextWindow: number      // model's declared context window in tokens
}): Promise<{ summary: string; usage: { input: number; output: number } }> {
  const { model, events, contextWindow } = opts

  // If events fit in one call (leave 25% headroom for the summarize prompt)
  const maxTokensPerCall = Math.floor(contextWindow * 0.75)
  const totalTokens = estimateEventsTokens(events)

  if (totalTokens <= maxTokensPerCall) {
    const summary = await summarizeChunk(model, events)
    return { summary, usage: { input: totalTokens, output: estimateTokens(summary) } }
  }

  // Overflow fallback: chunked summarization
  const chunks = chunkEvents(events, maxTokensPerCall)
  const chunkSummaries = await Promise.all(chunks.map((chunk) => summarizeChunk(model, chunk)))

  // Summarize the summaries
  const combinedSummaries = chunkSummaries.join("\n\n---\n\n")
  const finalSummary = await summarizeChunk(model, [
    { type: "user_turn", content: `Combine these partial summaries into one:\n\n${combinedSummaries}`, ts: Date.now() },
  ])

  const inputTokens = estimateTokens(combinedSummaries)
  const outputTokens = estimateTokens(finalSummary)
  return { summary: finalSummary, usage: { input: inputTokens, output: outputTokens } }
}
```

- [ ] **Step 3: Run tests**

```bash
bun test src/session/compaction.test.ts
```

Expected: all PASS

- [ ] **Step 4: Commit**

```bash
git add src/session/compaction.ts src/session/compaction.test.ts
git commit -m "feat: compaction — token estimation + chunked summarization"
```

---

## Task 14: Tier 1 config-defined tools

Shell commands declared in `ainix.jsonc "tools"` array become named tools.

**Files:**
- Modify: `src/tools/index.ts` (add `createConfigTools`)

- [ ] **Step 1: Write `createConfigTools` function**

Add to `src/tools/index.ts`:

```typescript
import type { ConfigDefinedTool } from "../config"
import { execSync } from "child_process"
import { tool } from "ai"
import { z } from "zod"

export function createConfigTools(
  configTools: ConfigDefinedTool[],
  workspacePath: string,
  codebasePath: string,
): Record<string, CoreTool> {
  const result: Record<string, CoreTool> = {}
  for (const def of configTools) {
    const cwd = def.cwd === "workspace" ? workspacePath
              : def.cwd === "codebase" ? codebasePath
              : def.cwd  // absolute path
    result[def.name] = tool({
      description: def.description,
      parameters: z.object({}), // config tools take no LLM-provided args
      execute: async () => {
        const start = Date.now()
        try {
          const output = execSync(def.cmd, { cwd, encoding: "utf-8", stdio: ["ignore", "pipe", "pipe"] })
          return `exit 0 (${Date.now() - start}ms)\n${output}`
        } catch (err: any) {
          const output = (err.stdout ?? "") + (err.stderr ?? "")
          return `exit ${err.status ?? 1} (${Date.now() - start}ms)\n${output}`
        }
      },
    })
  }
  return result
}
```

- [ ] **Step 2: Check for name collisions in `createBuiltinTools`**

Name collisions between built-in tools and config-defined tools should fail at startup with a clear error. Add collision check to `cmd/run.ts` (implemented in Task 16).

- [ ] **Step 3: Commit**

```bash
git add src/tools/index.ts
git commit -m "feat: Tier 1 config-defined tools"
```

---

## Task 15: Spawn subagent tool

`spawn_subagent` runs a full ainix turn loop inline (same process) and blocks until the subagent reaches `turn_complete` or `error`.

**Files:**
- Create: `src/tools/spawn_subagent.ts`

- [ ] **Step 1: Write `src/tools/spawn_subagent.ts`**

```typescript
import { tool } from "ai"
import { z } from "zod"
import crypto from "crypto"
import fs from "fs"
import path from "path"

// Injected at turn-loop time so spawn_subagent can call runTurn recursively
let _runTurn: ((opts: SubagentRunOpts) => Promise<SubagentResult>) | null = null

export interface SubagentRunOpts {
  sessionId: string
  task: string
  workspacePath: string
  contextFiles: string[]
}

export interface SubagentResult {
  status: "done" | "error"
  result: string
}

export function registerSubagentRunner(
  fn: (opts: SubagentRunOpts) => Promise<SubagentResult>
): void {
  _runTurn = fn
}

export function createSpawnSubagentTool(parentSessionId: string, workspacePath: string) {
  return tool({
    description: "Spawn a subagent to complete a specific subtask. The subagent runs in its own session and cannot ask questions (v1). Returns the subagent's final output.",
    parameters: z.object({
      task: z.string().describe("The complete task description for the subagent"),
      context_files: z.array(z.string()).default([]).describe("Files to pass as context to the subagent (relative to codebase)"),
    }),
    execute: async ({ task, context_files }) => {
      if (!_runTurn) throw new Error("subagent runner not registered")

      const subId = `${parentSessionId}-sub-${crypto.randomUUID().slice(0, 8)}`
      const result = await _runTurn({
        sessionId: subId,
        task,
        workspacePath,
        contextFiles: context_files,
      })

      return JSON.stringify({ session_id: subId, status: result.status, result: result.result })
    },
  })
}
```

- [ ] **Step 2: Commit**

```bash
git add src/tools/spawn_subagent.ts
git commit -m "feat: spawn_subagent tool"
```

---

## Task 16: Core turn loop

The heart of ainix. `runTurn` orchestrates all the pieces: lock, pending resolution, compaction, reconstruction, streamText, permissions, git commit.

**Files:**
- Create: `src/cli/cmd/run.ts`

This is the largest single file — read it fully before implementation.

- [ ] **Step 1: Write `src/cli/cmd/run.ts`**

```typescript
import fs from "fs"
import path from "path"
import crypto from "crypto"
import { streamText, type CoreTool } from "ai"
import { loadConfig } from "../../config"
import { getSDK } from "../../provider/provider"
import { getModelContextLength } from "../../provider/models"
import { acquireLock, releaseLock } from "../../lock"
import { commitSession, getCodebaseHash, isCodebaseDirty } from "../../git"
import {
  readEvents, appendEvent, lastEvent, countTurnComplete, lastCompactionIndex,
  type AnyEvent, type PermissionsEvent, type PermissionsReplyEvent,
  type QuestionsEvent, type TurnCompleteEvent, type CompactionEvent,
  type SubagentSpawnedEvent, type SubagentCompleteEvent,
} from "../../session/transcript"
import { reconstructMessages } from "../../session/context"
import { needsCompaction, compact } from "../../session/compaction"
import { wrapTools, type PendingPermission } from "../../permissions"
import { createBuiltinTools, createConfigTools } from "../../tools/index"
import { createSpawnSubagentTool, registerSubagentRunner, type SubagentResult } from "../../tools/spawn_subagent"
import { getQuestionsSignal, resetQuestionsSignal } from "../../tools/questions"
import { DualStream } from "../../stream"
import readline from "readline"

interface RunArgs {
  sessionId: string
  task?: string
  auto: boolean
  files: string[]
  workspacePath: string
  isSubagent?: boolean
}

function ensureSessionDir(workspacePath: string, sessionId: string): string {
  const dir = path.join(workspacePath, "sessions", sessionId)
  fs.mkdirSync(dir, { recursive: true })
  return dir
}

function transcriptPath(workspacePath: string, sessionId: string): string {
  return path.join(workspacePath, "sessions", sessionId, "transcript.jsonl")
}

function sessionJsonPath(workspacePath: string, sessionId: string): string {
  return path.join(workspacePath, "sessions", sessionId, "session.json")
}

function updateSessionJson(workspacePath: string, sessionId: string, model: string, usageDelta: { input: number; output: number; cache_read: number; cache_write: number }): void {
  const p = sessionJsonPath(workspacePath, sessionId)
  let existing: any = { id: sessionId, model, created_at: Date.now(), token_totals: { input: 0, output: 0, cache_read: 0, cache_write: 0 } }
  if (fs.existsSync(p)) {
    try { existing = JSON.parse(fs.readFileSync(p, "utf-8")) } catch {}
  }
  existing.token_totals.input += usageDelta.input
  existing.token_totals.output += usageDelta.output
  existing.token_totals.cache_read += usageDelta.cache_read
  existing.token_totals.cache_write += usageDelta.cache_write

  const tmp = p + ".tmp"
  fs.writeFileSync(tmp, JSON.stringify(existing, null, 2), "utf-8")
  fs.renameSync(tmp, p)
}

async function promptLine(question: string): Promise<string> {
  const rl = readline.createInterface({ input: process.stdin, output: process.stdout })
  return new Promise((resolve) => rl.question(question, (ans) => { rl.close(); resolve(ans.trim()) }))
}

async function resolvePermissions(
  pending: PendingPermission[],
  txPath: string,
  codebasePath: string,
  auto: boolean,
  sessionId: string,
): Promise<void> {
  const items: PermissionsEvent["items"] = pending.map((p) => ({
    id: p.id,
    cmd: `${p.name}(${JSON.stringify(p.input)})`,
    reason: p.reason,
  }))
  appendEvent(txPath, { type: "permissions", items, ts: Date.now() })

  if (auto) {
    // Auto-approve: run all commands via shell tool's execute
    const replyItems: PermissionsReplyEvent["items"] = []
    for (const p of pending) {
      replyItems.push({ id: p.id, decision: "approve" })
      // Execute the tool directly (already have the tool registered, but easier to just dispatch)
      // For non-shell tools, the execute is already wired — auto mode just re-enters the LLM
      // The command_result is appended by the tool's execute wrapper in the next LLM call
    }
    appendEvent(txPath, { type: "permissions_reply", from: "auto", items: replyItems, ts: Date.now() })
    return
  }

  // Interactive prompt
  process.stdout.write("\n")
  const replyItems: PermissionsReplyEvent["items"] = []
  for (let i = 0; i < pending.length; i++) {
    const p = pending[i]!
    process.stdout.write(`[${i + 1}] y/n  ${p.name}(${JSON.stringify(p.input)})  — ${p.reason}\n`)
    const answer = await promptLine(`  > `)
    if (answer === "y" || answer === "yes") {
      replyItems.push({ id: p.id, decision: "approve" })
    } else if (answer === "n" || answer === "no" || answer === "") {
      replyItems.push({ id: p.id, decision: "deny" })
    } else {
      replyItems.push({ id: p.id, decision: "instruct", text: answer })
    }
  }
  appendEvent(txPath, { type: "permissions_reply", from: "user", items: replyItems, ts: Date.now() })
}

async function resolveQuestions(txPath: string, sessionId: string): Promise<void> {
  const events = readEvents(txPath)
  const last = events[events.length - 1]
  if (!last || last.type !== "questions") return

  process.stdout.write("\n")
  const replyItems: Array<{ id: string; content: string }> = []
  for (const q of last.items) {
    const answer = await promptLine(`${q.content}\n> `)
    replyItems.push({ id: q.id, content: answer })
  }
  appendEvent(txPath, { type: "questions_reply", from: "user", items: replyItems, ts: Date.now() })
}

// Core turn execution — also used by spawn_subagent
export async function runTurn(args: RunArgs): Promise<SubagentResult | null> {
  const { sessionId, task, auto, files, workspacePath, isSubagent = false } = args
  const codebasePath = path.join(workspacePath, "codebase")
  const sessionDir = ensureSessionDir(workspacePath, sessionId)
  const txPath = transcriptPath(workspacePath, sessionId)

  // 1. Acquire lock
  acquireLock(sessionDir)

  try {
    const config = loadConfig(workspacePath)
    const model = getSDK(config.model)

    // 2. Handle pending events from last turn
    const lastEvt = lastEvent(txPath)
    if (lastEvt?.type === "questions") {
      if (isSubagent) {
        throw new Error("subagents cannot ask questions in v1")
      }
      await resolveQuestions(txPath, sessionId)
    } else if (lastEvt?.type === "permissions") {
      await resolvePermissions(
        [], // pending already in transcript — re-read below
        txPath,
        codebasePath,
        auto,
        sessionId,
      )
    }

    // 3. Append user_turn if --task provided
    if (task) {
      appendEvent(txPath, { type: "user_turn", content: task, ts: Date.now() })
    }

    // 4. File context is injected as system message during reconstruction
    // (context.ts handles compaction breakpoint)

    // 5. Git status
    const codebaseHash = getCodebaseHash(codebasePath)
    const codebaseDirty = isCodebaseDirty(codebasePath)

    // 5b. Token estimation + compaction
    const events = readEvents(txPath)
    const breakpoint = lastCompactionIndex(events)
    const relevantEvents = breakpoint >= 0 ? events.slice(breakpoint) : events
    const contextWindow = (await getModelContextLength(config.model)) ?? 128_000
    if (needsCompaction(relevantEvents, contextWindow, config.compaction_threshold)) {
      process.stderr.write("[ainix] compacting session...\n")
      const { summary, usage } = await compact({ model, events: relevantEvents, contextWindow })
      const compactionEvent: CompactionEvent = {
        type: "compaction",
        summary,
        turns_covered: countTurnComplete(relevantEvents),
        from_ts: relevantEvents[0]!.ts,
        to_ts: relevantEvents[relevantEvents.length - 1]!.ts,
        usage,
        ts: Date.now(),
      }
      appendEvent(txPath, compactionEvent)
      commitSession(workspacePath, sessionId, `session(${sessionId}): compaction`)
    }

    // 6. Reconstruct messages
    const messages = reconstructMessages(txPath)
    if (files.length > 0) {
      // Prepend file context as system-style user message
      const fs_ = require("fs")
      const fileContents = files.map((f) => {
        const p = path.join(codebasePath, f)
        const content = fs_.existsSync(p) ? fs_.readFileSync(p, "utf-8") : `(file not found: ${f})`
        return `<file path="${f}">\n${content}\n</file>`
      }).join("\n\n")
      messages.unshift({ role: "user", content: `Context files:\n\n${fileContents}` })
    }

    // 7. Build tools
    const builtins = createBuiltinTools(codebasePath)
    const configTools = createConfigTools(config.tools, workspacePath, codebasePath)

    // Check name collisions
    for (const name of Object.keys(configTools)) {
      if (name in builtins) {
        throw new Error(`Config tool name "${name}" collides with a built-in tool name.`)
      }
    }

    const allTools = { ...builtins, ...configTools }

    // Register subagent runner
    registerSubagentRunner(async (opts) => {
      const result = await runTurn({
        sessionId: opts.sessionId,
        task: opts.task,
        auto: true, // subagents always run in auto mode
        files: opts.contextFiles,
        workspacePath: opts.workspacePath,
        isSubagent: true,
      })
      return result ?? { status: "error", result: "subagent returned null" }
    })

    const spawnSubagentTool = createSpawnSubagentTool(sessionId, workspacePath)
    const allToolsWithSubagent = { ...allTools, spawn_subagent: spawnSubagentTool }

    // 7. Stream LLM
    const stream = new DualStream()
    const pendingPermissions: PendingPermission[] = []
    resetQuestionsSignal()

    const ac = new AbortController()

    const wrappedTools = wrapTools(allToolsWithSubagent, config.allow, pendingPermissions, codebasePath)

    // We need to intercept tool_call + tool_result events for allowed tools
    // Add event-appending wrappers on top of permission wrappers
    const trackedTools: Record<string, CoreTool> = {}
    for (const [name, t] of Object.entries(wrappedTools)) {
      trackedTools[name] = {
        ...t,
        execute: async (args: any, opts: any) => {
          const isPermGated = !config.allow.includes(name) && name !== "questions" && name !== "spawn_subagent"
          if (isPermGated) {
            // Permission gate: execute handles batching to pendingPermissions
            return await (t as any).execute(args, opts)
          }
          // Allowed tool or signal: append tool_call, execute, append tool_result
          const id = crypto.randomUUID()
          appendEvent(txPath, { type: "tool_call", id, name, input: args, ts: Date.now() })
          const result = await (t as any).execute(args, opts)
          appendEvent(txPath, { type: "tool_result", id, output: String(result), ts: Date.now() })
          return result
        },
      } as any
    }

    let lastStepText = ""
    let totalUsage = { input: 0, output: 0, cache_read: 0, cache_write: 0 }

    try {
      // No await — streamText returns immediately; streaming happens in the for-await below
      const result = streamText({
        model,
        messages,
        tools: trackedTools,
        maxSteps: 50,
        abortSignal: ac.signal,
        onStepFinish: async ({ text, usage: stepUsage }) => {
          // text is the assistant's text for this step (empty for tool-only steps)
          const content = stream.flushMessage()
          if (content) {
            appendEvent(txPath, { type: "assistant_text", content, ts: Date.now() })
            lastStepText = content
          }

          totalUsage.input += stepUsage.promptTokens
          totalUsage.output += stepUsage.completionTokens

          // If questions signal set or permissions pending: abort
          if (getQuestionsSignal() || pendingPermissions.length > 0) {
            ac.abort()
          }
        },
      })

      for await (const delta of result.textStream) {
        stream.writeToken(delta)
      }
    } catch (err: any) {
      if (err?.name === "AbortError") {
        // Expected abort from questions/permissions signal
      } else {
        appendEvent(txPath, { type: "error", message: String(err), phase: "llm", ts: Date.now() })
        commitSession(workspacePath, sessionId, `session(${sessionId}): error`)
        if (isSubagent) return { status: "error", result: String(err) }
        process.exit(1)
      }
    }

    // Check what ended the turn
    const questionsSignal = getQuestionsSignal()

    if (questionsSignal) {
      appendEvent(txPath, { type: "questions", items: questionsSignal.items, ts: Date.now() })
      if (isSubagent) {
        // v1: subagents cannot ask questions
        appendEvent(txPath, { type: "error", message: "subagents cannot ask questions in v1", phase: "tool", ts: Date.now() })
        commitSession(workspacePath, sessionId, `session(${sessionId}): error`)
        return { status: "error", result: "subagents cannot ask questions in v1" }
      }
      commitSession(workspacePath, sessionId, `session(${sessionId}): questions`)
      process.exit(0)
    }

    if (pendingPermissions.length > 0) {
      await resolvePermissions(pendingPermissions, txPath, codebasePath, auto, sessionId)
      if (auto) {
        // Re-enter LLM in the same process for auto mode
        // Clear pending and loop
        pendingPermissions.length = 0
        return runTurn({ ...args })
      }
      commitSession(workspacePath, sessionId, `session(${sessionId}): permissions`)
      process.exit(0)
    }

    // turn_complete
    const turnN = countTurnComplete(readEvents(txPath)) + 1
    const turnCompleteEvt: TurnCompleteEvent = {
      type: "turn_complete",
      usage: totalUsage,
      codebase_hash: getCodebaseHash(codebasePath),
      codebase_dirty: isCodebaseDirty(codebasePath),
      ts: Date.now(),
    }
    appendEvent(txPath, turnCompleteEvt)
    updateSessionJson(workspacePath, sessionId, config.model, totalUsage)
    commitSession(workspacePath, sessionId, `session(${sessionId}): turn ${turnN}`)

    if (isSubagent) {
      return { status: "done", result: lastStepText }
    }
    return null

  } catch (err: any) {
    appendEvent(txPath, { type: "error", message: String(err), phase: "init", ts: Date.now() })
    try { commitSession(workspacePath, sessionId, `session(${sessionId}): error`) } catch {}
    if (isSubagent) return { status: "error", result: String(err) }
    process.stderr.write(`[ainix] error: ${err}\n`)
    process.exit(1)
  } finally {
    releaseLock(sessionDir)
  }
}

// CLI entry for `ainix run`
export async function runCommand(argv: string[]): Promise<void> {
  // Parse args: --session <id> [--task "..."] [--auto] [files...]
  let sessionId: string | undefined
  let task: string | undefined
  let auto = false
  const files: string[] = []
  const workspacePath = process.cwd()

  for (let i = 0; i < argv.length; i++) {
    if (argv[i] === "--session" && argv[i + 1]) {
      sessionId = argv[++i]
    } else if (argv[i] === "--task" && argv[i + 1]) {
      task = argv[++i]
    } else if (argv[i] === "--auto") {
      auto = true
    } else if (argv[i] && !argv[i]!.startsWith("-")) {
      files.push(argv[i]!)
    }
  }

  if (!sessionId) {
    // Auto-generate session ID
    sessionId = `s${crypto.randomUUID().slice(0, 8)}`
    process.stderr.write(`[ainix] starting session ${sessionId}\n`)
  }

  await runTurn({ sessionId, task, auto, files, workspacePath })
}
```

- [ ] **Step 2: Verify compilation**

```bash
cd /Users/aicoder/Documents/ideation/ainix
bun build src/cli/cmd/run.ts --target=bun 2>&1
```

Expected: no errors (fix any type errors that appear)

- [ ] **Step 3: Commit**

```bash
git add src/cli/cmd/run.ts
git commit -m "feat: core turn loop"
```

---

## Task 17: CLI commands — auth, models, init

**Files:**
- Create: `src/cli/cmd/auth.ts`
- Create: `src/cli/cmd/models.ts`
- Create: `src/cli/cmd/init.ts`
- Modify: `src/cli/index.ts`

- [ ] **Step 1: Write `src/cli/cmd/auth.ts`**

```typescript
import { get, all, set, remove } from "../../auth/index"
import readline from "readline"

async function prompt(q: string): Promise<string> {
  const rl = readline.createInterface({ input: process.stdin, output: process.stdout })
  return new Promise((res) => rl.question(q, (a) => { rl.close(); res(a.trim()) }))
}

export async function authCommand(argv: string[]): Promise<void> {
  const [sub, provider] = argv

  if (sub === "list" || sub === "ls" || !sub) {
    const credentials = await all()
    if (Object.keys(credentials).length === 0) {
      console.log("No credentials configured. Run: ainix auth add <provider>")
      return
    }
    for (const [key, cred] of Object.entries(credentials)) {
      const masked = cred.type === "api" ? `${cred.key.slice(0, 8)}...` : cred.type
      console.log(`  ${key}: ${masked}`)
    }
    return
  }

  if (sub === "add") {
    if (!provider) { console.error("Usage: ainix auth add <provider>"); process.exit(1) }
    const key = await prompt(`${provider} API key: `)
    await set(provider, { type: "api", key })
    console.log(`✓ Saved ${provider} credentials`)
    return
  }

  if (sub === "remove" || sub === "rm") {
    if (!provider) { console.error("Usage: ainix auth remove <provider>"); process.exit(1) }
    await remove(provider)
    console.log(`✓ Removed ${provider} credentials`)
    return
  }

  console.error(`Unknown auth command: ${sub}`)
  process.exit(1)
}
```

- [ ] **Step 2: Write `src/cli/cmd/models.ts`**

```typescript
import { getModels } from "../../provider/models"

export async function modelsCommand(argv: string[]): Promise<void> {
  const [filter] = argv
  try {
    const models = await getModels()
    const filtered = filter
      ? models.filter((m) => m.provider?.toLowerCase().includes(filter.toLowerCase()) || m.id?.toLowerCase().includes(filter.toLowerCase()))
      : models
    for (const m of filtered) {
      console.log(`  ${m.provider ?? "?"}/${m.id ?? "?"}${m.name ? `  (${m.name})` : ""}`)
    }
    console.log(`\n${filtered.length} models${filter ? ` matching "${filter}"` : ""}`)
  } catch (err) {
    console.error(`Error fetching models: ${err}`)
    process.exit(1)
  }
}
```

- [ ] **Step 3: Write `src/cli/cmd/init.ts`**

```typescript
import fs from "fs"
import path from "path"
import { checkGitInstalled, initWorkspace, addSubmodule, initialCommit } from "../../git"

const DEFAULT_CONFIG = `{
  // ainix configuration
  "model": "anthropic/claude-sonnet-4-6",
  // Allow list: tools that run without approval. Default empty = everything gated.
  "allow": [],
  // Tier 1 config-defined tools: shell commands the LLM can call by name
  "tools": []
}
`

export async function initCommand(argv: string[]): Promise<void> {
  const [repoUrl] = argv
  const workspacePath = process.cwd()

  checkGitInstalled()
  initWorkspace(workspacePath)

  // Create directories
  fs.mkdirSync(path.join(workspacePath, "sessions"), { recursive: true })

  // Write .gitignore
  const gitignorePath = path.join(workspacePath, ".gitignore")
  const gitignoreContent = "sessions/*/lock\n.ainix-git.lock\n"
  if (!fs.existsSync(gitignorePath)) {
    fs.writeFileSync(gitignorePath, gitignoreContent, "utf-8")
  } else {
    const existing = fs.readFileSync(gitignorePath, "utf-8")
    if (!existing.includes("sessions/*/lock")) {
      fs.appendFileSync(gitignorePath, "\n" + gitignoreContent)
    }
  }

  // Write ainix.jsonc if not present
  const configPath = path.join(workspacePath, "ainix.jsonc")
  if (!fs.existsSync(configPath)) {
    fs.writeFileSync(configPath, DEFAULT_CONFIG, "utf-8")
  }

  if (repoUrl) {
    addSubmodule(workspacePath, repoUrl)
    initialCommit(workspacePath)
    console.log(`\n✓ ainix workspace initialized with codebase at ${repoUrl}`)
    console.log(`\nStart an agent turn:`)
    console.log(`  ainix run --task "..."`)
  } else {
    initialCommit(workspacePath)
    console.log(`\n✓ ainix workspace initialized`)
    console.log(`\nAdd your codebase as a git submodule:`)
    console.log(`  git submodule add <your-repo-url> codebase`)
    console.log(`\nThen start an agent turn:`)
    console.log(`  ainix run --task "..."`)
  }
}
```

- [ ] **Step 4: Wire up `src/cli/index.ts`**

```typescript
const [, , cmd, ...rest] = process.argv

async function main() {
  switch (cmd) {
    case "init": {
      const { initCommand } = await import("./cmd/init")
      await initCommand(rest)
      break
    }
    case "run": {
      const { runCommand } = await import("./cmd/run")
      await runCommand(rest)
      break
    }
    case "auth": {
      const { authCommand } = await import("./cmd/auth")
      await authCommand(rest)
      break
    }
    case "models": {
      const { modelsCommand } = await import("./cmd/models")
      await modelsCommand(rest)
      break
    }
    default:
      console.log("Usage: ainix <init|run|auth|models>")
      console.log("  init [<repo-url>]       Initialize workspace")
      console.log("  run [options] [files]   Run one agent turn")
      console.log("    --session <id>          Session ID (auto-generated if omitted)")
      console.log("    --task \"...\"          Task instruction for this turn")
      console.log("    --auto                  Auto-approve all permissions")
      console.log("  auth <add|list|remove>  Manage API credentials")
      console.log("  models [provider]       List available models")
      process.exit(cmd ? 1 : 0)
  }
}

main().catch((err) => {
  console.error(err)
  process.exit(1)
})
```

- [ ] **Step 5: Verify full CLI compiles**

```bash
bun build src/cli/index.ts --target=bun 2>&1
```

Expected: no errors

- [ ] **Step 6: Commit**

```bash
git add src/cli/
git commit -m "feat: CLI commands — auth, models, init, run wiring"
```

---

## Task 18: Binary compilation + smoke test

Compile to a single self-contained binary and verify the basic commands work.

**Files:**
- Create: `dist/` directory (gitignored)

- [ ] **Step 1: Add dist to .gitignore**

```bash
echo "dist/" >> /Users/aicoder/Documents/ideation/ainix/.gitignore
```

- [ ] **Step 2: Build the binary**

```bash
cd /Users/aicoder/Documents/ideation/ainix
mkdir -p dist
bun build --compile --minify --target=bun bin/ainix.ts --outfile dist/ainix
```

Expected: `dist/ainix` binary created, no errors

- [ ] **Step 3: Smoke test — help output**

```bash
dist/ainix
```

Expected: Usage text printed

- [ ] **Step 4: Smoke test — models command**

```bash
dist/ainix models anthropic
```

Expected: List of anthropic models (or cache warning if offline, but not a crash)

- [ ] **Step 5: Smoke test — init in a temp dir**

```bash
TMPDIR=$(mktemp -d)
cd "$TMPDIR"
/Users/aicoder/Documents/ideation/ainix/dist/ainix init
ls -la
```

Expected: `.git/`, `sessions/`, `.gitignore`, `ainix.jsonc` created

- [ ] **Step 6: Commit**

```bash
cd /Users/aicoder/Documents/ideation/ainix
git add .gitignore
git commit -m "feat: binary build verified"
```

---

## Task 19: Integration smoke test — full turn

Verify the turn loop runs end-to-end with a real model (requires a configured API key).

- [ ] **Step 1: Set up a test workspace**

```bash
WORKSPACE=$(mktemp -d)
mkdir -p "$WORKSPACE/codebase"
cd "$WORKSPACE"
git init
git init codebase
echo '{ "model": "anthropic/claude-sonnet-4-6", "allow": ["read", "glob", "ls"] }' > ainix.jsonc
mkdir sessions
echo "sessions/*/lock" > .gitignore
git add -A
git commit -m "init"
```

- [ ] **Step 2: Configure an API key**

```bash
/Users/aicoder/Documents/ideation/ainix/dist/ainix auth add anthropic
# Enter your Anthropic API key when prompted
```

- [ ] **Step 3: Run a simple turn**

```bash
cd "$WORKSPACE"
/Users/aicoder/Documents/ideation/ainix/dist/ainix run --session test1 --task "List the files in the codebase root directory." --auto
```

Expected:
- LLM output streams to stdout in cyan
- `sessions/test1/transcript.jsonl` created with events
- `sessions/test1/session.json` created
- Git commit made: `session(test1): turn 1`
- Process exits 0

- [ ] **Step 4: Verify the transcript**

```bash
cat "$WORKSPACE/sessions/test1/transcript.jsonl"
```

Expected: JSONL lines with `session_start`, `user_turn`, `assistant_text`, `turn_complete`

- [ ] **Step 5: Run tests**

```bash
cd /Users/aicoder/Documents/ideation/ainix
bun test
```

Expected: all unit tests pass

- [ ] **Step 6: Final commit**

```bash
git add -A
git commit -m "chore: integration smoke test verified"
```

---

## Implementation Notes

**`streamText` is NOT awaited:** Call it as `const result = streamText({...})` — no `await`. It returns a `StreamTextResult` immediately. The actual streaming happens when you consume `result.textStream` in the `for await` loop. `onStepFinish` fires at step boundaries during that loop.

**Permission gate interaction with AI SDK:** The `wrapTools` function intercepts `execute` for non-allowed tools. When a non-allowed tool is called, it returns a sentinel string and adds to `pendingPermissions`. `onStepFinish` calls `ac.abort()` if `pendingPermissions` is non-empty. The abort stops the multi-step loop. The main loop then handles the pending batch (prompt + append `permissions_reply`), and either exits (default mode) or loops recursively (auto mode).

**Step 2 — handling PREVIOUS turn's pending permissions:** When entry step 2 detects that the last transcript event is `permissions` (from the previous turn), it should NOT call `resolvePermissions` with the full function (which appends a new `permissions` event). Instead, read the items from the existing `permissions` event, prompt the user for each, and append ONLY a `permissions_reply`. The `permissions` event is already in the transcript. Split `resolvePermissions` into `promptAndAppendReply(existingPermissionsEvent, ...)` for step 2 and the full function for current-turn non-allowed tools.

**Questions signal:** The `questions` tool's `execute` sets a module-level flag via `getQuestionsSignal()`. `onStepFinish` aborts the stream when this flag is set. The main loop appends the `questions` event after the stream settles, then commits and exits.

**`require("fs")` in run.ts:** Move the `const fs_ = require("fs")` used for file context injection to a top-level `import fs from "fs"` at the top of `run.ts`.

**`onStepFinish` text buffer timing:** `onStepFinish` fires after a step's text deltas are fully emitted to `textStream` and before the next step begins. Calling `stream.flushMessage()` inside `onStepFinish` correctly captures the complete step text. If `text` is empty (tool-only step), `flushMessage()` returns `""` — skip appending `assistant_text` in that case.

**Session ID for subagents:** Parent session `s1`, subagent gets `s1-sub-<8-char-uuid>`. This maintains a clear parent-child relationship in the sessions directory.

**Error handling:** All errors append an `error` event with `phase` set before committing and exiting. The LLM sees the error as a user message on the next turn.

**First run (no session.json):** If `session.json` doesn't exist, `updateSessionJson` creates it. If `transcript.jsonl` doesn't exist, it's created by `appendEvent` when the first event is written. The `session_start` event should be appended by `initSession()` only on first run (check if `transcript.jsonl` is empty).

**The OpenCode tool port:** All tool descriptions should use the `.txt` sidecar files in the OpenCode `tool/` directory as the description string — they are crafted specifically for LLM consumption. Import them as text (Bun supports `import desc from "./tool.txt"` with `--loader=text` or just `fs.readFileSync`).

**Provider BUNDLED_PROVIDERS and CUSTOM_LOADERS:** These are the two large exports in OpenCode's `provider.ts`. `grep -n "BUNDLED_PROVIDERS\|CUSTOM_LOADERS" /path/to/opencode/src/provider/provider.ts` to find their line ranges, then read and copy those sections. The `CUSTOM_LOADERS` map is the most important — it wires each provider ID to its Vercel AI SDK factory function. Verify that the imported `@ai-sdk/*` packages in `provider.ts` match the version in your `package.json`.

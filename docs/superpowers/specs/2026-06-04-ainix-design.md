# ainix — Design Spec
_2026-06-04_

## 1. What It Is

ainix is a stateless, turn-based AI coding agent harness distributed as a single compiled binary. It treats an agent loop as an ephemeral CLI process: each invocation reads state from disk, runs one LLM turn, appends results to disk, and exits. No daemons, no background processes, no TUI.

State is separated from code by inverting the traditional workspace layout: a git repository tracks AI memory (sessions, transcripts), and the user's application codebase lives as a git submodule inside it. The AI writes to the codebase but never commits it — that stays dirty for human review. The parent repo is auto-committed after every turn, locking the transcript and the exact submodule pointer.

---

## 2. Repository Structure

### The ainix CLI package

```
ainix/
├── package.json
├── tsconfig.json
├── src/
│   ├── cli/
│   │   ├── index.ts              # entry: ainix init | run | auth | models
│   │   └── cmd/
│   │       ├── init.ts           # workspace setup
│   │       ├── run.ts            # harness turn loop
│   │       └── auth.ts           # add/remove/list credentials
│   ├── auth/
│   │   └── index.ts              # vendored from OpenCode (MIT)
│   ├── provider/
│   │   ├── models.ts             # models.dev fetch + hourly cache
│   │   └── provider.ts           # BUNDLED_PROVIDERS + CUSTOM_LOADERS + getSDK + getLanguage
│   ├── session/
│   │   ├── transcript.ts         # JSONL append helpers + Zod event schemas
│   │   └── context.ts            # reconstruct messages[] from JSONL for LLM
│   ├── tools/                    # vendored from OpenCode (MIT) + ainix-specific
│   │   ├── read.ts
│   │   ├── write.ts
│   │   ├── edit.ts
│   │   ├── multiedit.ts
│   │   ├── apply_patch.ts
│   │   ├── glob.ts
│   │   ├── grep.ts
│   │   ├── ls.ts
│   │   ├── codesearch.ts
│   │   ├── webfetch.ts
│   │   ├── shell.ts              # permission-gated shell (ainix-specific)
│   │   └── spawn_subagent.ts     # blocking subagent runner (ainix-specific)
│   ├── acp/
│   │   └── index.ts              # ainix-acp adapter entrypoint
│   └── stream.ts                 # DualStream: ANSI stdout + JSONL append
└── bin/ainix
```

### `context.ts` — transcript → messages[] reconstruction

Every turn replays the full transcript into `messages[]` for the AI SDK. Mapping rules:

| JSONL event(s) | AI SDK message |
|---|---|
| `user_turn` | `{ role: "user", content }` |
| `assistant_text` | `{ role: "assistant", content }` |
| `tool_call{id}` + `tool_result{id}` | `{ role: "assistant", content: [{ type: "tool-call", toolCallId: id, ... }] }` + `{ role: "tool", content: [{ type: "tool-result", toolCallId: id, ... }] }` |
| `questions` + `questions_reply` | `{ role: "assistant", content: "questions: ..." }` + `{ role: "user", content: "replies: ..." }` |
| `permissions` + `permissions_reply` | `{ role: "assistant", content: "permission request: ..." }` + `{ role: "user", content: "approved/denied. output: ..." }` + `command_result` contents inline |
| `command_result` (auto mode) | folded into the preceding permissions_reply message |
| `error` | `{ role: "user", content: "error during <phase>: <message>" }` |
| `subagent_spawned/complete` | informational; injected as system context not user messages |

Tool calls are always paired by `id` before constructing messages. Adjacent `tool_call`/`tool_result` pairs from the same step are grouped into one assistant message with multiple tool-call parts, matching the AI SDK's parallel-tool-call format.

### The workspace `ainix init` creates

```
my-workspace/
├── .git/                         # tracks AI memory
├── .gitignore                    # includes: sessions/*/lock, .ainix-git.lock
├── ainix.jsonc                   # config: model, allow list, provider overrides
├── sessions/
│   └── [session-id]/
│       ├── transcript.jsonl      # append-only event log
│       ├── session.json          # metadata: created_at, model, token totals
│       └── lock                  # ephemeral PID file — gitignored
└── codebase/                     # git submodule → user's app repo
    └── .git
```

`sessions/*/lock` and `.ainix-git.lock` are in `.gitignore`. `git add sessions/<id>/` never stages lock files.

---

## 3. Distribution

- **Runtime**: Bun
- **Language**: TypeScript
- **Binary**: `bun build --compile` → single self-contained executable, no runtime dependency
- **Git**: required on host system, checked at startup with a friendly error if absent. Not bundled (no clean TypeScript equivalent of Rust's `git2` crate that works with `bun build --compile`).

---

## 4. Dependencies

### npm packages

```
ai                              # streamText, tool, jsonSchema — the agentic loop
@ai-sdk/amazon-bedrock
@ai-sdk/anthropic
@ai-sdk/azure
@ai-sdk/cerebras
@ai-sdk/cohere
@ai-sdk/deepinfra
@ai-sdk/gateway
@ai-sdk/google
@ai-sdk/google-vertex
@ai-sdk/groq
@ai-sdk/mistral
@ai-sdk/openai
@ai-sdk/openai-compatible
@ai-sdk/perplexity
@ai-sdk/provider
@ai-sdk/provider-utils
@ai-sdk/togetherai
@ai-sdk/vercel
@ai-sdk/xai
@gitlab/gitlab-ai-provider
@openrouter/ai-sdk-provider
zod                             # auth + JSONL event schemas
```

### Vendored from OpenCode (MIT — copyright header preserved in each file)

- `src/auth/index.ts` — 71 lines, file-based credential store
- `src/provider/provider.ts` — slim extraction: BUNDLED_PROVIDERS + CUSTOM_LOADERS + getSDK + getLanguage (~200 lines, stripped of OpenCode framework deps)
- `src/tools/` — read, write, edit, multiedit, apply_patch, glob, grep, ls, codesearch, webfetch

---

## 5. Auth

Vendored from OpenCode's `auth/index.ts`. Stores credentials at `~/.local/share/ainix/auth.json`.

Three credential types:

```typescript
{ type: "api",       key: string }
{ type: "oauth",     refresh: string, access: string, expires: number }
{ type: "wellknown", key: string, token: string }
```

CLI:
```
ainix auth add <provider>    # interactive: API key or OAuth flow
ainix auth list              # show configured providers
ainix auth remove <provider>
```

---

## 6. Provider & Model Selection

- **Model registry**: `https://models.dev/api.json` — public API, no auth, 85 providers. Cached at `~/.cache/ainix/models.json`, refreshed hourly. If the endpoint is unreachable, use the stale cache with a `[warn] models.dev unreachable, using cached model list` message. If no cache exists, fail with a clear error and suggest specifying the model directly in `ainix.jsonc`.
- **Provider layer**: slim extraction from OpenCode's `provider.ts`. All 20 `@ai-sdk/*` packages bundled. CUSTOM_LOADERS encode per-provider quirks (beta headers, responses vs languageModel API, region prefixing, etc.).
- **Model format**: `provider/model-id` — e.g. `anthropic/claude-sonnet-4-6`
- **Config** (`ainix.jsonc`): `{ "model": "anthropic/claude-sonnet-4-6", "provider": { ... } }`

```
ainix models                 # list available models
ainix models anthropic       # filter by provider
```

---

## 7. JSONL Event Schema

Every session is an append-only JSONL file. One JSON object per line. Every event has `ts` (Unix milliseconds). Every event also has `type` (the event name) which is omitted from the shorthand below for brevity.

```
session_start      { schema_version: 1, id, model, codebase_hash, codebase_dirty, ts }
user_turn          { content, ts }
assistant_text     { content, ts }
tool_call          { id, name, input, ts }
tool_result        { id, output, ts }
command_result     { call_id, cmd, output, exit_code, duration_ms, timed_out, ts }
error              { message, phase, ts }
questions          { items: [{id, content}], ts }
questions_reply    { from: "user"|"session:<id>"|"auto", items: [{id, content}], ts }
permissions        { items: [{id, cmd, reason}], ts }
permissions_reply  { from: "user"|"session:<id>"|"auto", items: [{id, decision: "approve"|"deny"|"instruct", text?}], ts }
subagent_spawned   { session_id, task, ts }
subagent_complete  { session_id, status: "done"|"error", result, ts }
turn_complete      { usage: {input, output, cache_read, cache_write}, codebase_hash, codebase_dirty, ts }
compaction         { summary, turns_covered, from_ts, to_ts, usage: {input, output}, ts }
```

**Key field notes:**
- `tool_call.id` / `tool_result.id` — paired by the same UUID. Required for message reconstruction and AI SDK tool-call matching. The AI SDK may emit multiple parallel tool calls in one step; IDs are the only way to pair results correctly.
- `assistant_text` — **one event per complete assistant message**, not per token. DualStream writes tokens to stdout live (ANSI) and buffers the full message, appending a single `assistant_text` event when the message boundary is reached.
- `command_result.call_id` — matches the `id` of the `permissions` item that authorized this command.
- `error.phase` — where the error occurred: `"llm"` (streamText threw), `"tool"` (tool execution threw), `"git"` (commit failed), `"init"` (startup check failed).
- `codebase_hash` — `git rev-parse HEAD` of the submodule. `codebase_dirty` — whether the working tree has uncommitted changes.
- `usage.cache_read` / `cache_write` — Anthropic prompt-cache tokens; zero for providers that don't support caching.
- `schema_version` on `session_start` — increment when breaking schema changes are made.

`from` on reply events distinguishes human vs agent source — important for the LLM to weight replies correctly.

---

## 8. Turn Loop

Every `ainix run` invocation is one **turn**. A turn may involve multiple internal LLM round-trips (in `--auto` mode, when permissions are auto-approved and the LLM continues), but each `ainix run` process is still one turn — it begins and ends within one process lifetime.

```
ainix run --session <id> [--task "..."] [--auto] [files...]
```

If `--session <id>` does not exist, the session is auto-created (sessions dir + session.json + empty transcript).

`--auto` enables auto mode: all `permissions` events are approved without prompting. The process does not exit on permissions — it approves, runs commands, and re-enters the LLM in the same invocation. Auto mode exits only on `turn_complete` or `questions`.

### Session locking

On entry, ainix writes `sessions/<id>/lock` containing its PID. If a lock file exists and the PID is still alive, exit with an error ("session <id> is already running"). On exit (normal or error), the lock is removed. This prevents two concurrent `ainix run --session <id>` from corrupting the transcript.

### Turn number

Turn N = count of `turn_complete` events already in the transcript + 1. The commit message is `session(<id>): turn N`.

### Context compaction

Before reconstructing `messages[]`, ainix estimates the token count of the full transcript. If it exceeds a configurable threshold (default: 80% of the model's declared context window), ainix runs auto-compaction:

1. Send **all events since the last `compaction`** (or since `session_start` if none) to the LLM with a summarize prompt
2. Append a `compaction` event with the summary
3. Commit (`session(<id>): compaction`)
4. Continue the current turn — reconstruction reads from the `compaction` event forward

The `compaction` event is a **hard breakpoint** in the transcript. The reconstructor always finds the last `compaction` event first; everything before it is ignored for message reconstruction. Original events are preserved in git history.

Reading a compacted transcript:
```
session_start  ← ignored (before compaction)
user_turn      ← ignored
assistant_text ← ignored
...N turns...
turn_complete  ← ignored
compaction     ← BREAKPOINT — reconstruction starts here
user_turn      ← included
assistant_text ← included
turn_complete  ← included
```

The first message in the reconstructed `messages[]` is the compaction summary injected as a system/user context message: `"[Session history summary: <summary>]"`. Everything after the `compaction` event is replayed normally.

If the transcript compacts more than once (very long sessions), the reconstructor uses only the most recent `compaction` as the breakpoint — earlier compactions and all events before them are invisible to the LLM.

**Overflow fallback:** If the events to summarize are themselves too large to fit in a single LLM call (extreme edge case: a session with very large tool outputs), use chunked summarization: split the events into N sequential chunks, summarize each chunk independently, then summarize the chunk summaries into a single final summary. The resulting `compaction` event always has one `summary` string regardless of how many LLM calls were needed to produce it.

`compaction` fields:
- `summary` — the LLM-generated summary of everything it replaced
- `turns_covered` — how many `turn_complete` events were summarized
- `from_ts` — ts of the first event covered (session_start or previous compaction ts)
- `to_ts` — ts of the last event covered (the turn_complete just before this compaction)

### Entry

```
1. Acquire session lock (write PID to sessions/<id>/lock)
2. Check last event in transcript:
   pending questions?   → prompt user replies → append questions_reply → continue
   pending permissions? →
     if --auto:  auto-approve all → run commands → append command_result(s)
                 append permissions_reply{from:"auto"} → continue
     else:       prompt y/n/custom → if approved: run commands,
                                       append command_result(s)
                                       append permissions_reply{from:"user"} → continue
                                      if denied: append permissions_reply{from:"user"} → continue
   otherwise → continue

3. If --task: append user_turn event
4. Read listed files from codebase/ → inject as system context
5. Get git rev-parse HEAD + dirty status of codebase/ → set codebase_hash + codebase_dirty
5b. Token estimation: estimate context size of full transcript since last compaction.
    If > 80% of model's declared context window → run compaction (see below) → append
    compaction event → git commit ("session(<id>): compaction") → continue.
    Reconstruction in step 6 will use the new compaction as its breakpoint.
6. Reconstruct messages[] from transcript (from last compaction breakpoint → LLM message format)
7. Stream LLM with tools:
   - Text tokens → stdout (ANSI cyan) live; buffer full message → append ONE assistant_text on message boundary
   - Tool call → check allow list:
       allowed  → generate UUID id → append tool_call{id} → execute → append tool_result{id}
       not allowed → collect into pending permissions batch
   - After each LLM step: if pending permissions batch → append permissions event → handle per mode (see below)
   - Turn-ending signal (questions) → append event → commit → exit
   - streamText error → append error{phase:"llm"} → commit → exit 1
8. git add sessions/<id>/ && git commit -m "session(<id>): turn N"
9. Remove lock → exit 0
```

### `--task` + pending replies

If `ainix run --session <id> --task "..."` is invoked and the session has pending questions or permissions, pending replies are resolved first (step 2), then `--task` content is appended as a new `user_turn` (step 3). The LLM sees: previous context → question/permission resolved → new task instruction. This is the correct order: the pending item is answered, then new work is added.

### `session.json` update timing

`session.json` holds session metadata: `{ id, created_at, model, token_totals: { input, output, cache_read, cache_write } }`. It is atomically rewritten (write to `.session.json.tmp` → rename) after each `turn_complete`, accumulating token totals from `turn_complete.usage`. Never partially updated during a turn.

### Three turn-ending states

**`turn_complete`** — `streamText` completes with `finishReason: "stop"` (no more tool calls). Append `turn_complete`, commit, exit.

The AI SDK emits `finishReason: "tool-calls"` at each step where tools are called, and `"stop"` only when the model emits a final text response with no tool calls. The harness distinguishes the three states by which turn-ending tool (`questions`/`permissions`) was called, not by `finishReason` alone.

**`questions`** — LLM batched one or more questions. Commit, exit. Next `ainix run` resolves them, appends `questions_reply`, re-enters LLM immediately in the same process.

**`permissions`** — LLM batched one or more shell command requests.

- **Default mode**: commit, exit. Next `ainix run` prompts per item:
  ```
  [1] y/n  write("src/auth.ts")        write file to codebase
  [2] y/n  shell("bun test", ...)      run test suite
  [3] y/n  webfetch("https://...")     fetch external URL
  > _
  ```
  User enters decisions one by one (y/n for each). Approved items execute; denied items are skipped. All decisions are recorded in `permissions_reply` before re-entering the LLM. Custom text on any item replaces its execution with an instruction to the LLM.

- **Auto mode** (`--auto`): no exit, no prompt. Harness runs all commands immediately, appends `command_result(s)`, appends `permissions_reply{from:"auto"}`, re-enters LLM. Loops until `turn_complete` or `questions`.

`permissions_reply.from` values: `"user"` (human approved), `"auto"` (auto mode), `"session:<id>"` (parent agent approved).

---

## 9. Tool Set

Vendored OpenCode tools use OpenCode's internal Tool interface and will need a thin adapter to the Vercel AI SDK `tool()` format. Not architectural — just wiring at integration time.

### Permission model — whitelist, default deny

All tools go through the permission gate. Nothing executes automatically unless explicitly allowed. The `allow` list in `ainix.jsonc` is a whitelist of tool names that auto-execute without prompting. Default: empty (everything gated).

```jsonc
// ainix.jsonc — example allow list
{
  "model": "anthropic/claude-sonnet-4-6",
  "allow": ["read", "glob", "grep", "ls"]
}
```

Tools not in `allow` are batched into a `permissions` event at the end of each LLM step — the harness exits, prompts the user per-item (see Section 8), and re-enters on the next invocation.

Per-item approval: the permission prompt lists each requested tool call individually with `y/n`:
```
[1] y/n  write("src/auth.ts", ...)     write file
[2] y/n  shell("bun test", ...)        run tests
[3] y/n  webfetch("https://...", ...)  fetch URL
```
Each item gets an independent decision. Items approved in the same batch all execute before re-entering the LLM.

### Tools

| Tool | Description | Source |
|------|-------------|--------|
| `read` | Read file from codebase/, with line ranges | OpenCode |
| `write` | Write file to codebase/ | OpenCode |
| `edit` | Smart patch/edit a file | OpenCode |
| `multiedit` | Edit multiple files in one call | OpenCode |
| `apply_patch` | Apply a patch format diff | OpenCode |
| `glob` | Find files by pattern | OpenCode |
| `grep` | Search file contents | OpenCode |
| `ls` | List directory | OpenCode |
| `codesearch` | Semantic code search | OpenCode |
| `webfetch` | Fetch a URL | OpenCode |
| `shell(cmd, reason)` | Run a shell command in codebase/ | ainix |

### Turn-ending signals

These are not tools in the permission sense — they are control signals and always execute immediately.

| Signal | Description |
|--------|-------------|
| `questions(items[])` | Batch questions for user or parent agent — ends turn |
| `permissions(items[])` | Batch tool permission requests — ends turn (if any non-allowed tools pending) |
| `spawn_subagent(task, context_files[])` | Create new session, run subagent turn synchronously |

---

## 10. Extensibility

`bun build --compile` embeds the full Bun runtime, including the TypeScript transpiler. This means the compiled binary can `import()` external `.ts` files from the filesystem at runtime — no `jiti`, no separate toolchain. Two tiers of extensibility, both read from `ainix.jsonc` at startup.

### Tier 1 — Config-defined tools (no code)

Shell commands defined in JSON become named permission-gated tools. Good for test, build, lint, format — the 80% case.

```jsonc
// ainix.jsonc
{
  "model": "anthropic/claude-sonnet-4-6",
  "tools": [
    {
      "name": "run_tests",
      "description": "Run the full test suite and return output",
      "cmd": "bun test 2>&1",
      "cwd": "codebase"
    },
    {
      "name": "type_check",
      "description": "Run TypeScript type checking",
      "cmd": "bun run typecheck 2>&1",
      "cwd": "codebase"
    }
  ]
}
```

Commands are fixed strings — no interpolation, no injection surface. The LLM calls them by name; the harness runs the pre-defined `cmd`. Goes through the standard permissions gate.

| Field | Required | Description |
|-------|----------|-------------|
| `name` | yes | Tool name the LLM calls |
| `description` | yes | What the LLM sees in the tool list |
| `cmd` | yes | Shell command to run (fixed, not LLM-generated) |
| `cwd` | no | `"codebase"` (default), `"workspace"`, or absolute path |

### Tier 2 — Code extensions (TypeScript) — deferred

`.ts` files dynamically imported at runtime via Bun's embedded transpiler. Deferred pending a spike to confirm `await import("/abs/path/user.ts")` works correctly in a `bun build --compile` artifact. If the spike passes, add back before implementation begins. The API shape when implemented:

```typescript
interface ExtensionAPI {
  registerTool(def: {
    name: string
    description: string
    // no tier field — all tools go through the allow-list gate uniformly
    execute: () => Promise<{ output: string; exit_code?: number }>
  }): void
}
```

Extensions cannot register turn-ending signals. Extension tools are subject to the same `allow` list as built-in tools — adding `"my_extension_tool"` to `allow` auto-approves it.

### Load order

1. Built-in tools (read, write, edit, glob, grep, …)
2. Config-defined tools from `ainix.jsonc` `"tools"` array
3. _(deferred)_ Code extensions from `ainix.jsonc` `"extensions"` array

Name collisions with built-in tools are rejected at startup with a clear error.

---

## 11. ACP Compatibility (`ainix-acp`)

ainix does not implement ACP internally — that would require a long-lived process, which conflicts with the stateless-exit philosophy. Instead, ACP compatibility is provided by a **separate thin adapter binary**: `ainix-acp`.

```
Episutra / Zed
  → acp-core (Rust client)
    → spawns: ainix-acp --workspace /path/to/workspace
      (long-lived ACP process, speaks JSON-RPC 2.0 over stdio)
        → calls: ainix run --session <id> --task "..."  (exits after each turn)
          → reads/writes: sessions/<id>/transcript.jsonl
```

ainix itself is unchanged — always stateless, always exits. `ainix-acp` is the long-lived piece.

### How the adapter works

For each ACP turn:

```
1. Client sends prompt via ACP
2. adapter notes current byte offset in sessions/<id>/transcript.jsonl
3. adapter spawns: ainix run --session <id> --task "<prompt>"
   (stdout → /dev/null — ANSI output is for humans, not the adapter)
4. adapter tails transcript.jsonl from the saved offset while ainix runs,
   buffering at newline boundaries (partial lines are held until \n arrives)
5. each complete JSONL line → translate event → send ACP notification in real-time
6. ainix process exits:
   - exit 0 → send StopReason::EndTurn (turn_complete was written before exit)
   - exit 1 → send StopReason::Cancelled + surface error event content
   - timeout (configurable, default 10 min) → kill ainix, send Cancelled
```

The process exit code is authoritative — not the event stream. `StopReason::EndTurn` is sent only after confirmed exit 0. This prevents the adapter from signalling success while ainix is still committing to git.

Streaming works because ainix appends a single `assistant_text` event per complete message. The `assistant_text.content` field holds the full buffered message, so the adapter forwards it as one `AgentMessageChunk`. This means ACP clients see message-level chunks, not token-level — acceptable for the first version.

### Event translation

| JSONL event | ACP output |
|---|---|
| `assistant_text` | `session/update AgentMessageChunk` |
| `tool_call` | `session/update ToolCall` |
| `tool_result` | `session/update ToolCallUpdate` |
| `questions` | `session/request_permission` (option array = questions batch) |
| `permissions` | `session/request_permission` (option array = commands batch) |
| `turn_complete` | `StopReason::EndTurn` |

Permission/question replies from the ACP client are written to the workspace as `questions_reply` or `permissions_reply` events before the next `ainix run` call. The harness picks them up on entry, exactly as it would for human replies.

### Multi-session git safety

When `ainix-acp` manages multiple concurrent ACP sessions, each spawns `ainix run` independently. Both processes eventually call `git commit`, which races on `.git/index.lock`. To prevent corruption, `ainix-acp` holds a workspace-level advisory lock around the git commit portion of each `ainix run`:

```
workspace-level lock file: .ainix-git.lock  (in workspace root, gitignored)
```

Before each `ainix run` invocation, `ainix-acp` acquires `.ainix-git.lock` (with PID + timeout). After `ainix run` exits, it releases the lock. This serializes git commits across concurrent ACP sessions without affecting the session-level `sessions/<id>/lock` (which only guards transcript writes).

**Direct `ainix run` invocations** (non-ACP) do not need the workspace-level lock — they cannot run concurrently because the terminal is occupied. The workspace-level lock is an `ainix-acp` concern only.

### Distribution

`ainix-acp` lives in the same repo as a second compile target (`bun build --compile --entrypoint src/acp/index.ts`). Same codebase, separate binary. Dependencies: `@agentclientprotocol/sdk` (only in ainix-acp, not ainix).

Episutra registers it alongside the other ACP agents:
```
AcpAgent::from_str("ainix-acp --workspace /path/to/workspace")
```

---

## 12. Subagent Architecture  

Each subagent is a full ainix session with its own UUID and `transcript.jsonl`. Communication is file-based — transcripts are the protocol.

**Concurrency:** Subagents run sequentially. `spawn_subagent` blocks the parent turn until the subagent reaches `turn_complete` or `error`. Only one ainix process writes to `codebase/` at a time, eliminating working-tree conflicts. Parallel subagents are a future item.

**v1 restriction:** Subagents cannot emit `questions` in v1. If a subagent LLM calls the `questions` tool, the harness treats it as an error — appends `error{phase:"tool", message:"subagents cannot ask questions in v1"}` to the subagent transcript and terminates the subagent with exit 1. `subagent_complete.status` is `"error"`. This avoids the resume-primitive problem (a blocking subagent that emits questions would stall the parent indefinitely). Design the subagent's system prompt to be fully self-contained with sufficient context.

### Spawning

```
ainix run --session s1 --task "refactor auth module"

LLM calls spawn_subagent(
  task: "extract JWT logic from auth.ts",
  context_files: ["src/auth.ts"]
)
→ harness creates sessions/s1-sub-abc123/
→ runs ainix turn loop for s1-sub-abc123 inline (not a subprocess — same process)
→ blocks until subagent turn_complete or error
→ appends subagent_spawned + subagent_complete to parent transcript
```

### `spawn_subagent` tool result

The `spawn_subagent` tool resolves with:
```typescript
{
  session_id: string,          // e.g. "s1-sub-abc123"
  status: "done" | "error",
  result: string               // last assistant_text content on "done";
                               // error message on "error"
}
```

`subagent_complete.result` in the parent transcript holds the same `result` string, so both the LLM and the JSONL log have the subagent's output.

### Why it works

Transcripts are files. `subagent_complete.result` surfaces the subagent's final output to the parent LLM as a tool result — no special IPC needed. The parent LLM can read the subagent transcript directly via `read` if it needs finer detail.

---

## 13. `ainix init`

```
ainix init [<repo-url>]
```

Full form (repo URL provided):
1. Check `git --version` — error if absent
2. `git init` (skip if `.git` exists)
3. Create `sessions/`, `.gitignore`, `ainix.jsonc` (default model: `anthropic/claude-sonnet-4-6`)
4. `git submodule add <repo-url> codebase`
5. Initial commit of parent repo
6. Print: `ainix run --session s1 --task "..."`

Partial form (no URL): steps 1–3 + 5, prints the submodule command for the user to run manually.

---

## 14. Future / Deferred

- **Proto output adapter**: `ainix export --format proto` converts JSONL transcript to length-delimited binary protobuf records for archegraph integration. Deferred — JSONL is the native format.
- **`websearch` tool**: deferred, decide based on use.
- **Additional `@ai-sdk/*` providers**: add per request, zero architectural impact.
- **Event hooks system**: post-turn hooks that fire after each commit (e.g. `onTurnComplete: "./hooks/notify.ts"`). Inspired by pi's 20+ event hook types. Deferred — code extensions cover the immediate need.
- **Tier-2 code extensions**: deferred pending spike (confirm `await import(external.ts)` works in a `bun build --compile` binary).
- **Parallel subagents**: currently subagents run sequentially (one at a time, no concurrent writes to codebase/). Parallel execution requires a workspace-level lock or separate working trees.

## 15. Design Decisions Log

| Decision | Choice | Rationale |
|---|---|---|
| Transcript format | JSONL | Append-only, parseable, no textproto/markdown ambiguity |
| Distribution | `bun build --compile` single binary | No runtime dep, clean install |
| Git bundling | System git required | No clean TS equivalent for compiled binary |
| AI SDK | Vercel `ai` + 20 `@ai-sdk/*` packages | Unified API, all providers, small overhead |
| Auth | Vendored from OpenCode (MIT) | 71 lines, stable, no framework deps |
| Context window overflow | Auto-compaction (full, hard breakpoint) | Seamless; `compaction` event is clear boundary; full history in git |
| Permission model | Whitelist, default deny | Security-first; user explicitly allows what's safe |
| Permission approval UX | Per-item y/n | Finer control; matches schema's per-item `decision` |
| Background shell tool | Not included | Orphan process on parent exit; PID recycling makes tracking unreliable. Use `shell("nohup ... &")` instead |
| ACP compatibility | Separate `ainix-acp` adapter binary | ainix stays stateless; adapter handles long-lived ACP connection |
| Subagent communication | File-based (transcript tailing) | No IPC, no sockets; transcripts are the protocol |
| Subagent execution | Sequential (blocking) | Prevents concurrent writes to shared codebase/ working tree |
| Subagent questions (v1) | Not allowed | Avoids blocking resume primitive; subagents must be self-contained |
| `spawn_subagent` result | Last `assistant_text` content | Simple: parent LLM gets the output as a tool result; can read transcript for detail |
| ACP multi-session git safety | Workspace-level `.ainix-git.lock` | Serializes `git commit` across concurrent ACP sessions; doesn't affect session locks |
| Compaction overflow | Chunked summarization | Handles extreme edge case (huge tool outputs) without deadlock |
| `session.json` update | Atomic rename after `turn_complete` | Prevents partial reads; accumulates token totals across turns |

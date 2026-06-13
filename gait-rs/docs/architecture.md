# gait-rs Architecture

## Layers

```
┌──────────────────────────────────────────────────────┐
│  Tier 1 · Entry         bin/gait.rs, chat.rs         │
├──────────────────────────────────────────────────────┤
│  Tier 2 · Profile       profile.rs, config.rs,       │
│                         agent_frontmatter.rs          │
├──────────────────────────────────────────────────────┤
│  Tier 3 · Turn          turn.rs  (production loop)   │
│           Generate      stream.rs (stateless loop)   │
├──────────────────────────────────────────────────────┤
│  Tier 4 · Provider      provider.rs, stream.rs,      │
│                         anthropic/, openai/,          │
│                         factory.rs                   │
├──────────────────────────────────────────────────────┤
│  Tier 5 · Session       session/retrace.rs,          │
│                         session/transcript.rs,        │
│                         proto/                        │
└──────────────────────────────────────────────────────┘

Cross-cutting:
  tools/        Tool trait + built-in implementations
  permission.rs Pattern matching (not gating — gating lives in turn.rs)
  ui/           REPL rendering + permission prompts (stderr only)
  git/          Checkpoint, trace, codebase repos
  auth.rs       API key resolution
```

---

## What Each Layer Owns

### Tier 1 · Entry (`bin/gait.rs`, `chat.rs`)
- CLI argument parsing and command dispatch
- Session ID generation
- REPL event loop (rustyline)
- Docker sandboxing decision

**Does NOT own:** Business logic. Commands should delegate immediately to lower layers.

### Tier 2 · Profile (`profile.rs`, `config.rs`, `agent_frontmatter.rs`)
- Parse and merge config hierarchies: `gait.jsonc` → agent frontmatter → CLI flags
- Resolve model string → provider instance
- Connect MCP clients
- Instantiate built-in + MCP + config tools
- Build `PermissionedTools` (tools filtered through allow/deny lists)
- Output: `ResolvedProfile` — the live, wired-up agent spec

### Tier 3 · Turn (`turn.rs`)
The production agentic loop. Single entry point: `run_turn(session, emitter, caps, input)`.

Responsibilities:
- Fold session log → `Vec<Message>` (transcript) via `retrace`
- Build the unified system prompt (harness + agent body + loadable-index + permission status)
- Drive the LLM via `provider.stream()` directly (NOT via `stream.rs`)
- Check permissions before each tool call (allow/deny patterns)
- Execute tools, emit events, handle signals (Continue, TurnComplete, Clarify, Inform, LoadContext)
- Checkpoint the codebase (git shadow DB)
- Return `TurnResult { Done | Permissions | Clarify | Inform }`

**Does NOT own:** Rendering. It emits proto events to `log.jsonl`; rendering is chat.rs/ui's job.

### Tier 3 · Generate (`stream.rs`)
A **separate, simpler** stateless loop. No sessions, no permissions, no transcripts. Used by:
- `generate.rs` — basic text generation without tools
- Integration tests

**Not the production path.** Features added to `turn.rs` do NOT appear here automatically.

### Tier 4 · Provider (`provider.rs`, `anthropic/`, `openai/`, `factory.rs`)
- `Provider` trait: `stream(system, messages, tools) → ChunkStream`
- `StreamChunk` enum: Text, ToolCallComplete, StepUsage, Done
- Provider implementations: AnthropicProvider, OpenAIProvider (covers OpenAI-compat APIs)
- `factory.rs`: parse `provider/model-id` → resolve API key → return `Arc<dyn Provider>`

**Does NOT own:** Tool execution logic. Provider streams chunks; turn.rs decides what to do with them.

### Tier 5 · Session (`session/`, `proto/`)
- Append-only event log: `sessions/<id>/log.jsonl` (proto3 JSON via pbjson)
- Projections folded via `run_projection()`:
  - `ActiveMessages` → `Vec<Message>` (transcript sent to LLM)
  - `ActiveAgent` → enabled MCP/skills/plugins, current model, allow/deny
  - `ActivePermissions` → session + once-scoped approvals
  - `ActiveMutations` → LWW Redact/Replace register
- `session.json` → metadata (id, model, token_totals, codebase_path)

**Does NOT own:** Business logic. Projections are pure folds over events.

---

## Cross-Cutting Modules

### Tools (`tools/`)
Each tool implements the `Tool` trait:
```rust
fn name() -> &str
fn description() -> &str
fn parameters() -> &Value
fn permission(args) -> ToolPermission   // what permission does this call need?
async fn execute(args) -> Result<Value> // do the work
```

`ToolPermission` is either `Free` (no prompt) or `RequiresApproval(Vec<PermissionRequest>)`.

`PermissionRequest` carries:
- `display` — the human-readable subject (file path, shell command, …)
- `patterns` — allow-list patterns from broadest to most specific
- `preview` — optional text the tool wants shown before the user decides (diff, content, etc.)

**The tool layer owns the preview content.** The tool knows what it's about to do and can populate `preview` in `permission()`. The UI renders whatever string is there without caring about format.

### Permission (`permission.rs`)
Pure pattern-matching functions. No I/O, no state.
- `is_request_approved(tool_name, req, allow_list)` → bool
- `is_request_denied(tool_name, req, deny_list)` → bool
- `path_patterns(tool, path)` → Vec<String>  (file tool pattern generator)
- `command_patterns(cmd)` → Vec<String>  (shell tool pattern generator)
- `save_choice(choice, gait_dir, ...)` → writes to gait.jsonc or frontmatter

**Does NOT own:** Gating (deciding whether to proceed). Gating is `turn.rs`'s job.

### UI (`ui/`)
Renders to stderr only. Reads from `PermissionItem` (which comes from `log.jsonl` via transcript.rs).
- `prompt_permission(item, label, theme)` → PermissionChoice  (interactive prompt)
- `ReplRenderer` — formats turn output (text streaming, tool calls, results)
- `ReplBuilder` — handles REPL user input for clarify/inform/permission replies

**Does NOT own:** State or business logic. Pure rendering.

### Git (`git/`)
- `CheckpointRepo` — shadow git DB per session (per-turn codebase snapshots)
- `TraceRepo` — git worktree per session (isolated working directory)
- `CodebaseRepo` — read codebase HEAD, dirty status

---

## Type Contracts at Layer Boundaries

| Boundary | Types crossing |
|----------|---------------|
| Entry → Profile | `Config`, `AgentFrontmatter` |
| Profile → Turn | `ResolvedProfile { provider, permissioned, mcp_registry, skill_registry, plugin_registry, hooks }` |
| Turn → Provider | `&str` (system), `&[Message]`, `&[Box<dyn Tool>]` → `ChunkStream` |
| Turn → Tools | `tool.permission(args)` → `ToolPermission`; `tool.execute(args)` → `Value` |
| Turn → Permission | `PermissionRequest`, `Vec<String>` (allow/deny lists) → bool |
| Turn → Session | `Event` (proto) emitted to log.jsonl; `retrace::messages_from_path` → `Vec<Message>` |
| Turn → UI | `TurnResult { Permissions(Vec<PermissionItem>), Clarify(..), Inform(..) }` |
| Session → UI | `PermissionItem { id, cmd, reason, patterns, tool_name, tool_args, preview }` |

---

## Known Issues / Violations

### 1. `turn.rs` is monolithic (≈110 KB)
It mixes: system prompt construction, the agentic loop, permission gating, signal dispatch, LoadContext handling, test infrastructure. Should be split into:
- `turn/prompt.rs` — system prompt builder
- `turn/gate.rs` — permission check + event emission
- `turn/signals.rs` — ExecutionSignal dispatch
- `turn/loop.rs` — the inner streaming loop

### 2. `stream.rs` duplicates the agentic loop
`stream.rs` has its own 50-step loop that calls `tool.execute()` directly with no permission checks. It was written before `turn.rs` matured. Features added to `turn.rs` (e.g. per-tool signals, max_iter config) silently don't apply to the `stream.rs` path. The only current callers are `generate.rs` and integration tests. Options:
- Delete `stream.rs` and rewrite those callers to use `turn.rs` with appropriate `TurnInput`
- Keep it but document it as "permission-free generation only, not a general agent loop"

### 3. `bin/gait.rs` is growing (≈77 KB)
Contains both CLI parsing and non-trivial command logic. Commands like `cmd_run` handle session creation, profile loading, and turn invocation inline. This will become hard to test and maintain. Business logic should live in domain modules, not in the binary.

### 4. `PermissionItem` lives in `session/transcript.rs`
`PermissionItem` is the type the UI renders. It lives in the session layer because it appears in the transcript, but it's consumed primarily by the UI layer. This creates an upward dependency: `ui/permission.rs` imports `session::transcript::PermissionItem`. Acceptable for now, but worth watching.

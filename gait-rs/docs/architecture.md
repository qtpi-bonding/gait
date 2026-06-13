# gait-rs Architecture

gait is an event-sourced, turn-based agent. Every turn appends immutable events to a
per-session log; all mutable state (conversation, agent config, permissions) is a
**projection** folded from that log. Nothing is stored twice — the log is the single
source of truth.

---

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
│  Tier 4 · Provider      provider.rs, anthropic/,     │
│                         openai/, factory.rs          │
├──────────────────────────────────────────────────────┤
│  Tier 5 · Session       session/{retrace,transcript, │
│           (event log)   ingestor,session_json}.rs    │
│                         proto/, git/                  │
└──────────────────────────────────────────────────────┘

Cross-cutting:
  tools/        Tool trait + built-in implementations
  permission.rs Pure pattern matching (gating lives in turn.rs)
  ui/           REPL rendering + permission prompts (stderr only)
  git/          The three repos: codebase, trace, checkpoint
  auth.rs       API key resolution
```

### Tier 1 · Entry (`bin/gait.rs`, `chat.rs`)
CLI parsing, command dispatch, session-ID generation, REPL loop, Docker sandbox decision.
Should delegate to lower layers immediately — **not** own business logic.

### Tier 2 · Profile (`profile.rs`, `config.rs`, `agent_frontmatter.rs`)
Merges config hierarchy (`gait.jsonc` → agent frontmatter → CLI flags), resolves model →
provider, connects MCP clients, builds tools, produces `ResolvedProfile` — the live agent spec.

### Tier 3 · Turn (`turn.rs`)
The production agentic loop. Entry: `run_turn(session, emitter, caps, input)`. Folds the log
into a transcript, builds the system prompt, drives the LLM via `provider.stream()` **directly**,
gates each tool call against allow/deny patterns, executes tools, emits events, returns
`TurnResult { Done | Permissions | Clarify | Inform }`. Emits proto events — does **not** render.

### Tier 3 · Generate (`stream.rs`)
A **separate, simpler** stateless loop — no sessions, permissions, or transcripts. Used only by
`generate.rs` (tool-less text generation) and integration tests. **Not the production path.**

### Tier 4 · Provider (`provider.rs`, `anthropic/`, `openai/`, `factory.rs`)
`Provider` trait: `stream(system, messages, tools) → ChunkStream`. `StreamChunk` enum (Text,
ToolCallComplete, StepUsage, Done). `factory.rs`: `provider/model-id` → API key → `Arc<dyn Provider>`.
Streams chunks; turn.rs decides what to do with them.

### Tier 5 · Session — see deep dive below.

---

## The Event Log (Tier 5 core)

### One log per session
`<gait_dir>/sessions/<id>/log.jsonl` — append-only NDJSON, one `Event` per line. Mutations
(Redact/Replace) are appended as **new** events; existing lines are never modified.

### The Event envelope
Proto `gait.log.v1.Event` (`proto/log.proto:28`):

```protobuf
message Event {
  uint32 version          = 1;  // format generation (part of content_hash)
  string id               = 2;  // UUIDv4
  uint64 seq              = 3;  // Lamport clock — the ordering key
  uint64 ts               = 4;  // wall-clock ms (metadata only, never used for ordering)
  string content_hash     = 5;  // SHA-256(JCS(body)) integrity stamp
  repeated string parents = 6;  // causal DAG parents
  oneof body {
    Payload payload = 16;       // a substantive event
    Redact  redact  = 17;       // mutation: remove a prior event
    Replace replace = 18;       // mutation: rewrite a prior event's body
  }
}
```

### Append path (`proto/wire.rs`, `proto/envelope.rs`)
`write_to_transcript(path, body)`: read last event → `mint()` (assign `seq = prev.seq + 1`,
fresh UUID, parents) → `stamp_hash()` → serialize (pbjson canonical JSON) → append line.

### content_hash (`proto/hash.rs`)
`SHA-256( "v" || version || "." || JCS(body_json) )`, lowercase hex. Covers the **body only**
(the `payload`/`redact`/`replace` arm), not the envelope. Proves the body hasn't been tampered
with for a given `version`. **Not** a Merkle chain — there is no prev-hash linkage yet.

### Event kinds (`Payload.kind` oneof — complete list)
`SessionStart, Agent, UserTurn, AssistantText, ToolCall, ToolResult, Clarify, ClarifyReply,
ClarifyConvergence, Inform, InformReply, InformConvergence, Permissions, PermissionsReply,
AgentUpdate, TurnComplete, Summary, Error, RawEvent, SandboxApplied, HookRun, Cwd, Toggle,
ModelChange`. Constructors live in `proto/events.rs` (e.g. `toggle()`, `tool_call()`, `summary()`).

### The Emitter (`proto/emitter.rs`)
Owns the log path + an unbounded channel sender. Two methods:
- `emit(body)` — **durable + streamed**: writes the event to the log AND sends it to subscribers.
- `delta(content)` — **ephemeral**: streams a typing fragment to the channel, no file write.

A dropped receiver (headless/test) is silently ignored.

### Two proto packages
- **`gait.log.v1`** (`log.proto`) — the durable event log schema. Source of truth.
- **`gait.state.v1`** (`state.proto`) — `SessionStateReport` / `SessionState` (Idle,
  AwaitingPermission, AwaitingClarify, AwaitingInform). **Derived, never stored** — computed on
  demand by retrace from the log tail, returned by `gait status`. It references (not copies) the
  item types from `log.v1`.

---

## Projections (`session/retrace.rs`)

A projection is a derived read-model produced by left-folding the log. All share one trait:

```rust
trait Projection {
    type State;
    fn seed(&self) -> Self::State;                               // initial (often config-seeded)
    fn checkpoint(&self, kind) -> Option<Self::State> { None }   // a checkpoint resets the fold
    fn apply(&self, state: &mut Self::State, ev, kind);          // fold one event
    fn finish(&self, state) -> Self::State { state }             // finalize
}
```

`run_projection(p, events, mutations)` seeds, then folds each event's **effective** kind
(after mutations are applied). If an event is a checkpoint for this projection, the state resets
to that baseline **positionally** (where the event sits in the stream — `through_seq` is not consulted).

### The five projections

| Projection | State | Folds | Semantics |
|---|---|---|---|
| **ActiveMessages** | `Vec<Message>` (the transcript) | UserTurn, AssistantText, ToolCall, ToolResult, Clarify/Inform, Error | Consecutive ToolCalls coalesce into one assistant msg; Permissions excluded (audit only) |
| **ActiveAgent** | model, instructions, tools/skills/mcp/plugins sets, allow/deny | Toggle (OR-Set), ModelChange (LWW) | Seeded from config; `Agent` event is its checkpoint |
| **ActivePermissions** | `{ session: Set, once: Set }` | PermissionsReply (allow inserts, deny removes), TurnComplete (clears `once`) | Effective allow = session ∪ once |
| **ActiveCwd** | `PathBuf` | Cwd (LWW) | Seeded to codebase root |
| **LwwAgent** | `Option<Agent>` | Agent (LWW) | Internal drift-guard for run_turn |

`fold(events, agent_seed)` runs Messages + Agent + Permissions in one pass over the same
mutation-applied stream and returns `RetraceResult { messages, agent, effective_allow, mutations }`.

### Mutations — Redact & Replace
`active_mutations(events)` converges all Redact/Replace events into a `HashMap<target_id, MutationState>`
using **LWW**: for each target, the mutation with the highest `(seq, id)` wins.

`apply_cascade()` enforces ToolCall↔ToolResult integrity: if **either** member of a call/result
pair is redacted, **both** are redacted (a cascade Redact overrides any Replace on the peer).
You can never have a result without its call in the folded transcript.

`effective_kinds()` applies the converged mutation map while folding — redacted events are
skipped, replaced events yield their replacement payload.

### Compaction — Summary checkpoints
A `Summary` event is a **checkpoint for ActiveMessages only**. When the fold hits it, the message
state resets to a single synthesized summary message (positional reset — everything before it is
dropped from context). The Agent, Permissions, and Cwd projections **ignore** Summary and fold the
whole log, so agent config and session grants survive compaction. Multiple summaries: the latest wins.

---

## The Three Repos (`git/`)

`Repos { codebase, traces, checkpoints }` is a DI bundle of three trait objects, built by
`Repos::production()` and threaded through `Session` into turns and tools. Tests swap in mocks
(e.g. `MockCheckpointRepo`).

| Repo | Role | On disk | Writes? |
|---|---|---|---|
| **CodebaseRepo** | Read user's real git state (`head_hash`, `dirty_files`) for the prompt | the user's `<codebase>/.git` | **Never** — trait has no mutating methods |
| **TraceRepo** | Per-session git worktree + commits the event log each turn | `<gait_dir>/.git` + `sessions/<id>/` worktree on branch `session/<id>` | Yes (turn commits) |
| **CheckpointRepo** | Per-turn snapshot of the codebase working tree (for `rewind`) | `sessions/<id>/codebase.git/` (bare, `alternates` → user's `.git/objects`) | Yes (snapshot commits) — never touches user's `.git` |

### On-disk layout for one session
```
gait_dir/
├── .git/                                  ← TraceRepo main repo
│   └── refs/heads/session/<id>            ← session branch
└── sessions/<id>/
    ├── .git                               ← worktree pointer
    ├── log.jsonl                          ← the event log (source of truth)
    ├── session.json                       ← metadata sidecar (see violation #1)
    ├── lock                               ← PID file (RAII during a turn)
    ├── codebase.git/                      ← CheckpointRepo bare shadow DB
    │   └── objects/info/alternates        ← → user's .git/objects (dedup)
    └── [codebase working-tree files]
```

### During a turn
1. **CodebaseRepo** reads HEAD + dirty files → injected into the system prompt `<status>`.
2. **TraceRepo** creates/attaches the session worktree (idempotent).
3. **CheckpointRepo** inits the shadow DB (idempotent).
4. Tools run, possibly mutating the codebase.
5. **CheckpointRepo** snapshots the new tree → SHA recorded in the `TurnComplete` event.
6. **TraceRepo** commits the turn's log changes.

---

## Type Contracts at Layer Boundaries

| Boundary | Types crossing |
|----------|---------------|
| Entry → Profile | `Config`, `AgentFrontmatter` |
| Profile → Turn | `ResolvedProfile { provider, permissioned, mcp/skill/plugin_registry, hooks }` |
| Turn → Provider | `&str` system, `&[Message]`, `&[Box<dyn Tool>]` → `ChunkStream` |
| Turn → Tools | `tool.permission(args) → ToolPermission`; `tool.execute(args) → Value` |
| Turn → Permission | `PermissionRequest`, allow/deny `Vec<String>` → bool |
| Turn → Session | `Event` emitted; `retrace::fold` → `RetraceResult` |
| Turn → UI | `TurnResult { Permissions(Vec<PermissionItem>), Clarify, Inform }` |
| Session → UI | `PermissionItem { id, cmd, reason, patterns, tool_name, tool_args, preview }` |

**The tool layer owns preview content.** `PermissionRequest.preview` is populated in a tool's
`permission()` method — never in turn.rs or the UI. The UI renders whatever string is there
without knowing its format (diff, file content, command, …).

---

## Known Issues / Boundary Violations

### 1. `session.json` holds state that should be a projection
`session.json` carries `token_totals` — cumulative input/output/cache tokens. But every
`TurnComplete` event already records per-turn `usage`. So token totals exist **twice**: as
appendable events (source of truth) and as a mutable read-modify-write sidecar. This contradicts
"everything is a projection." The fix is an `ActiveUsage` projection that sums `TurnComplete.usage`
over the log; `session.json` should keep only write-once identity (`id`, `created_at`,
`codebase_path`, `assigned_by`).

### 2. `compat_rewrite_keys` is a backward-compat layer we said we don't want
`proto/wire.rs` rewrites legacy event keys on read (`manifest`→`agent`, `compaction`→`summary`,
strips `outputSchemaRetries`). The project policy is **no backward compat**. Either commit to a
log-format version gate (and migrate old logs once) or delete this shim — silently rewriting keys
hides schema drift.

### 3. `turn.rs` is monolithic (~110 KB)
Mixes prompt construction, the streaming loop, permission gating, signal dispatch, LoadContext
handling, and a large test module. Should split into `turn/{prompt,gate,signals,loop}.rs`.

### 4. `stream.rs` duplicates the agentic loop without permissions
A second 50-step loop that calls `tool.execute()` directly with **no permission checks**. Predates
mature `turn.rs`. Features added to `turn.rs` silently don't apply here. Only `generate.rs` and
integration tests use it. Either delete it and route callers through `turn.rs`, or rename/document
it as permission-free generation, not a general agent loop.

### 5. `bin/gait.rs` is growing (~77 KB)
Command bodies (`cmd_run`, etc.) hold session-creation and turn-invocation logic inline. Business
logic should live in domain modules so it's testable without the binary.

### 6. `PermissionItem` lives in the session layer but is UI-facing
`ui/permission.rs` imports `session::transcript::PermissionItem`. It sits in `transcript.rs`
because it appears in the log, but its primary consumer is the UI — an upward dependency. Tolerable,
but watch it: if more UI-only fields (like `preview`) accrete on it, consider a UI-side view type.

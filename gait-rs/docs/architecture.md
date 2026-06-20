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
  port/, embed.rs  Turn I/O boundary: TurnSource/TurnSink/TurnDriver + run_loop (see below)
  tools/        Tool trait + built-in implementations
  permission.rs Pure pattern matching (gating lives in turn.rs)
  ui/           REPL rendering + permission prompts (stderr only)
  git/          The three repos: codebase, trace, checkpoint
  auth.rs       API key resolution
```

### Tier 1 · Entry (`bin/gait.rs`, `cli/run.rs`, `chat.rs` + `repl_loop.rs`)
CLI/REPL parsing, command dispatch, session-ID generation, Docker sandbox decision. The actual
turn-driving is the **Turn I/O Boundary** (below): `cmd_run` (CLI) and `run_chat` (REPL) each build
a `TurnSource`/`TurnSink`/`TurnDriver` and call the unified `embed::run_loop` — the same surface
external embedders use. `bin/gait.rs` is now a thin dispatcher.

### Tier 2 · Profile (`profile.rs`, `config.rs`, `agent_frontmatter.rs`)
Merges config hierarchy (`gait.jsonc` → agent frontmatter → CLI flags), resolves model →
provider, connects MCP clients, builds tools, produces `ResolvedProfile` — the live agent spec.

### Tier 3 · Turn (`turn.rs`)
The production agentic loop. Entry: `run_turn(session, recorder, caps, input)`. The turn's input
event (a `user_turn`, or a reply) is appended to the log by the **I/O boundary** *before* this runs
(append-then-fold); `run_turn` folds the log into a transcript, builds the system prompt, drives the
LLM via `provider.stream()` **directly**, gates each tool call against allow/deny patterns, executes
tools, records events through the `Recorder`, and returns `TurnResult { Done | Permissions | Clarify
| Inform }`. Emits proto events — does **not** render or read input.

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

### The Recorder (`proto/emitter.rs`)
Owns the log path + an unbounded channel sender (`StreamItem`). Two methods:
- `record(body)` — **durable + streamed**: writes the event to the log AND publishes it to the
  channel, in lockstep. This is the **one write path**: tools, hooks, and the workflow runner all
  emit through it (no log-only bypass), so every event reaches live subscribers as well as the log.
- `publish_delta(content)` — **ephemeral**: streams a typing fragment to the channel, no file write.

A dropped receiver (headless/test) is silently ignored. Build one with
`Recorder::for_session(gait_dir, id, sender)`; `detached()` is a no-op for session-less paths.
(Formerly `Emitter`/`emit`/`delta` — renamed when the turn-I/O boundary landed, see below.)

### Two proto packages
- **`gait.log.v1`** (`log.proto`) — the durable event log schema. Source of truth.
- **`gait.state.v1`** (`state.proto`) — `SessionStateReport` / `SessionState` (Idle,
  AwaitingPermission, AwaitingClarify, AwaitingInform). **Derived, never stored** — computed on
  demand by retrace from the log tail, returned by `gait status`. It references (not copies) the
  item types from `log.v1`.

---

## The Turn I/O Boundary (ports & adapters)

Turns are driven through a small **ports-and-adapters** layer around the event log, so the CLI,
the REPL, the workflow runner, and external embedders are all just adapters over **one** turn
engine. `run_turn` never touches stdin/stdout/a terminal — it only folds the log and emits events.

### Output port (`port/output.rs`, `cli/output.rs`, `ui/`)
- `classify(&StreamItem) → StreamClass{Trace, Error}` — read-time routing; `Error`-kind payloads
  (and `workflow_error`/`node_error` raw events) are `Error`, everything else `Trace`.
- `trait TurnSink { item, result, session_id, diagnostic, begin_turn, end_turn }` — the agnostic
  output contract. `drive(rx, &mut sink)` pumps the channel into a sink.
- Adapters: **`StdioSink`** (CLI — Trace+Error → stderr, the one proto `RunResult` → stdout),
  the REPL **`ReplSink`** (view-layer render), and **`JsonSink`** (a callback sink emitting pbjson
  for embedders — see below).

### Input port (`port/input.rs`)
- `trait TurnSource { async fn next() → Option<TurnStart> }` where `TurnStart{Input(event::Body) |
  Resume}`. **Append-then-fold:** the adapter builds the input event (`embed::user_turn_body(...)`)
  and the loop appends it to the log *before* `run_turn`, which then folds it — symmetric with the
  reply path. (`run_turn` no longer creates the `user_turn` itself; `TurnInput.task` is gone.)
- Adapters: **`ArgSource`** (CLI `--task`, then EOF), **`ReplSource`** (REPL line reader +
  interaction/respawn).

### The unified run-loop (`embed.rs`)
- `trait TurnDriver { resolve_profile, turn_config, after_turn, on_turn_error }` — per-turn machinery the loop
  can't own generically (rebuild `ResolvedProfile`, auto/max_iter/preview, after-turn policy).
- `run_loop(source, &mut sink, session, gait_dir, session_id, &mut driver)` — appends each input
  event, runs one `run_turn` per yield, pumps its stream to the sink. Shared by `cmd_run`
  (`cli/run.rs`, `ArgSource`+`StdioSink`+`CliDriver`) and `run_chat`
  (`chat.rs`/`repl_loop.rs`, `ReplSource`+`ReplSink`+`ReplDriver`).

### The embedding facade (`gait_ai::embed`)
The whole surface is public under one module: `TurnSource`/`TurnStart`, `TurnSink`/`JsonSink`,
`TurnDriver`/`LoopConfig`, `run_loop`, `user_turn_body`, `resolve_profile`, plus the proto/session
re-exports. An external program drives gait by implementing the three ports — the CLI and REPL are
just the first two embedders. `JsonSink` emits **pbjson** (canonical protobuf-JSON), so an
FFI-style consumer (Serverpod/Flutter) parses output with a model generated from the same `.proto`.
A network transport or C-ABI shim would be additional `TurnSource`+`TurnSink` pairs over
`run_loop`, with zero core change. See `docs/embedding.md`.

The workflow runner (`workflow/runner.rs`) is under the same boundary: it records events through a
`Recorder` + a spawned consumer driving a `StdioSink`, and its prompt sub-turns publish to the
workflow's live channel.

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
    ├── session.json                       ← write-once identity metadata
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
| Turn → Tools | `tool.permission(args) → ToolPermission`; `tool.execute(args, &Recorder) → Value` (the `Recorder` lets a tool publish events live, e.g. `shell` → `cwd`, hooks → `hook_run`) |
| Turn → Permission | `PermissionRequest`, allow/deny `Vec<String>` → bool |
| Turn → Session | `Event` emitted; `retrace::fold` → `RetraceResult` |
| Turn → UI | `TurnResult { Permissions(Vec<PermissionItem>), Clarify, Inform }` |
| Session → UI | `PermissionItem { id, cmd, reason, patterns, tool_name, tool_args, preview }` |

**The tool layer owns preview content.** Preview strings are produced by the `Tool::preview()`
method (the gating-only `permission()` returns just display+patterns). `turn.rs` decides *when* to
call `preview()` per the `preview` policy (`gait.jsonc`): at the Tier-3 ask prompt it goes into
`PermissionItem.preview` (persisted, for re-render on resume); in `always` mode it is streamed live
for auto/allow-listed tools via the ephemeral `Emitter::delta` channel (never logged — the durable
record of the change lives in the codebase git). The UI renders whatever string is there without
knowing its format (diff, file content, command, …).

---

## Resolved (kept for the record)

### ✓ `session.json.token_totals` → `total_usage` projection
**Resolved.** The dead, duplicated
`token_totals` sidecar is gone; cumulative usage is now `retrace::total_usage(events)` — a
projection summing every `TurnComplete.usage`. Exposed via `gait cost <session-id>`. `session.json`
now holds only write-once identity (`id`, `created_at`, `codebase_path`, `assigned_by`, `agent_name`).

### ✓ `compat_rewrite_keys` shim deleted
**Resolved** in the same cleanup. The silent legacy-key rewriter (`manifest`→`agent`,
`compaction`→`summary`) is removed; `read_events` parses lines directly, so old-format logs now fail
loudly instead of being silently transformed — matching the no-backward-compat policy.

### ✓ `bin/gait.rs` command bodies held turn logic inline
**Resolved** by the turn-I/O boundary (Phases 1–5).
`cmd_run` moved to `cli/run.rs` and both it and the REPL now drive the unified `embed::run_loop`;
`bin/gait.rs` is a thin dispatcher. Turn-driving is testable without the binary (`tests/integration_run_loop.rs`, `tests/integration_embed.rs`).

---

## Known Issues / Boundary Violations

### 1. `turn.rs` is monolithic (~110 KB)
Mixes prompt construction, the streaming loop, permission gating, signal dispatch, LoadContext
handling, and a large test module. Should split into `turn/{prompt,gate,signals,loop}.rs`.

### 2. `stream.rs` duplicates the agentic loop without permissions
A second 50-step loop that calls `tool.execute()` directly with **no permission checks**. Predates
mature `turn.rs`. Features added to `turn.rs` silently don't apply here. Only `generate.rs` and
integration tests use it. Either delete it and route callers through `turn.rs`, or rename/document
it as permission-free generation, not a general agent loop.

### 3. `PermissionItem` lives in the session layer but is UI-facing
`ui/permission.rs` imports `session::transcript::PermissionItem`. It sits in `transcript.rs`
because it appears in the log, but its primary consumer is the UI — an upward dependency. Tolerable,
but watch it: if more UI-only fields (like `preview`) accrete on it, consider a UI-side view type.

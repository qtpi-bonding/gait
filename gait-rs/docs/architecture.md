# gait-rs Architecture

gait is an event-sourced, turn-based agent. Every turn appends immutable events to a
per-session log; all mutable state (conversation, agent config, permissions) is a
**projection** folded from that log. Nothing is stored twice — the log is the single
source of truth.

---

## Workspace crates

`gait-core` (the root package) is the engine; the workspace splits the rest into focused crates:

| Crate | Role |
|---|---|
| **gait-types** | foundation leaf: `types` (`Tool`/`Hook` traits, `ToolError`, the `Rec` recorder-surface trait), `config/`, `diagnostic.rs`. Depends only on `gait-proto`. |
| **gait-provider** | generation/wire leaf: `provider.rs`, `transport.rs`, `factory.rs`, `providers.rs`, `anthropic/`, `openai/`, `strict.rs`. Depends only on `gait-types`. |
| **gait-sandbox** | containment leaf: `sandbox.rs` (incl. `brain_scope()`), `process/` (incl. `proc_start_time()`). Depends only on `gait-types`. |
| **gait-core** | the library: `turn/` (incl. the sealed runtimes `brain.rs`/`spine.rs`/`hand.rs`), `tools/`, `session/`, `embed.rs`. Depends on `gait-types`/`gait-provider`/`gait-sandbox`, re-exporting their modules at the crate root (`crate::types`, `crate::config`, `crate::diagnostic`, `crate::provider`, `crate::sandbox`, `crate::process`, …) so internal `crate::…` paths are unchanged. |
| **gait-proto** | proto codegen + the log codec: `gait.{log,state,sandbox,workflow}.v1` and `envelope`/`wire`/`hash` (mint, append, `content_hash`) |
| **gait-cli** | command implementations — `cmd_run`, `cmd_spine`, `cmd_brain`, `cmd_run_tool` — thin arg-parsers over the `gait-core` runtimes |
| **gait-repl** | the REPL/TUI (`chat.rs`, `repl_loop.rs`) — another embedder over `run_loop` |
| **gait-workflow** | the workflow runner (a DAG of prompt/tool nodes) |
| **gait-bin** | the `gait` binary — a thin dispatcher into `gait-cli` |

The three sealed runtimes live in **gait-core** so both the CLI and the REPL can reach
them; `gait-cli`'s `cmd_brain` / `cmd_run_tool` just parse args and call `run_brain` /
`run_hand`.

`gait-types`/`gait-provider`/`gait-sandbox` form a compiler-enforced DAG under
`gait-core`: `gait-types` is the sole foundation leaf (depends only on `gait-proto`),
and `gait-provider`/`gait-sandbox` each depend on `gait-types` alone — neither can
reach `turn`/`session`/`spine`/`permissions`, which is enforced by each building
standalone (`cargo build -p gait-types`, `-p gait-provider`, `-p gait-sandbox`).
This broke a live `types ↔ tools` cycle (`ToolError` moved down into `gait-types`;
a `Rec` trait replaced the concrete `Recorder` in the `Tool`/`Hook` trait surface)
and inverted two upward edges (`brain_scope` out of `turn` into `sandbox`;
`proc_start_time` out of `session::daemons` into `process`).

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
│           (event log)   ingestor,session_meta}.rs    │
│                         proto/, git/                  │
└──────────────────────────────────────────────────────┘

Cross-cutting:
  port/, embed.rs  Turn I/O boundary: TurnSource/TurnSink/TurnDriver + run_loop (see below)
  tools/        Tool trait + built-in implementations
  permission.rs Pure pattern matching (gating lives in turn.rs)
  ui/           REPL rendering + permission prompts (stderr only)
  git/          The three repos: codebase, trace, checkpoint
  auth.rs       API key resolution
  sandbox       brain/spine/hands process seal — see sandbox-security.md
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

### Proto packages & pbjson
- **`gait.log.v1`** (`log.proto`) — the durable event log schema. Source of truth.
- **`gait.state.v1`** (`state.proto`) — `SessionStateReport` / `SessionState` (Idle,
  AwaitingPermission, AwaitingClarify, AwaitingInform). **Derived, never stored** — computed on
  demand by retrace from the log tail, returned by `gait status`. It references (not copies) the
  item types from `log.v1`.
- **`gait.sandbox.v1`** (`sandbox.proto`) — the **IPC** between the sealed processes. The Nerve
  `Signal` (brain↔spine: `Impulse`, `Feedback`, `Append`, `Spawn`, `Notify`, `StreamForward`,
  `Terminated`, `BrainInit`) and the hand reply (`Impulse` → `HandResponse`). See the sealed-execution
  section below and `sandbox-security.md`.
- **`gait.workflow.v1`** (`workflow.proto`) — workflow DAG definitions.

**pbjson** does double duty. The log is serialized as **pbjson canonical JSON**, so `content_hash`
over `JCS(body)` is stable and the *same* `.proto` parses it everywhere (incl. `JsonSink` for
embedders). And `pbjson_types` supplies the `google.protobuf` `Struct`/`Value` carriers that ferry
dynamic tool args/results across the sandbox IPC (`Impulse.args`, `Feedback.result`).

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
  the workflow-runner **view sink** (drives the REPL `ReplBuilder` for interactive prompt nodes),
  and **`JsonSink`** (a callback sink emitting pbjson for embedders — see below).

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
  (`chat.rs`, `chat → ReplSource` driven via the workflow runner's interactive prompt node).

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

## The Ops Boundary (`gait-ops`)

Turns aren't the only thing shared across frontends — the deterministic command verbs (`link`,
`init`, session admin, permissions, extensions, auth, daemons: ~21 verbs) are too. Before the
`gait-ops` crate existed, every such verb lived as a `cmd_*` function in `gait-cli`, fusing logic +
terminal I/O + `process::exit` into one CLI-only blob — a REPL or ACP adapter couldn't call it
without spawning a subprocess (which is exactly what the REPL did for `undo`/`unlock`/`load`/
`unload` before this sweep). `gait-ops` (`crates/gait-ops`) gives these verbs a shared home, the
same way the Turn I/O Boundary above gives the turn engine one. Full design rationale, the audit
that motivated it, and the walls' construction are in
`docs/plans/2026-07-09-core-ops-layer-design.md` (implementation plan:
`docs/plans/2026-07-09-core-ops-layer-plan.md`).

### The governing rule

> If a verb runs a turn in any form — a one-off model call, a streaming turn, a workflow — it is
> NOT an op.

Ops carry no model turn; their effects are limited to filesystem, git, session-log, config,
registry, and daemon-registry mutations/queries, plus idempotent network reads (e.g. fetching the
model list). "Runs a turn → not an op" is the one-line test for every new verb. Turn-drivers
(`run`, `exec`, `workflow run/continue`) stay on the Turn I/O Boundary above; workflow-admin verbs
(`workflow list/log/abort`) are deferred (coupled to `WorkflowRunLog`, not yet in `gait-core`).

### Two mechanical walls

Unlike `mod turn` (which tolerates tracked `#[allow]`ed debt), the ops boundary is enforced with
**no exceptions**:

- **Wall 1 — no terminal, no exit.** `crates/gait-ops/src/lib.rs` carries a crate-level hard deny:
  `#![deny(clippy::print_stdout, clippy::print_stderr, clippy::exit)]`. An op that needs to tell
  the caller something returns it in an `Outcome`; an op that fails returns `Err(OpError)`. The
  build breaks the moment anyone writes `println!`/`eprintln!`/`process::exit` inside `gait-ops`.
- **Wall 2 — no turn engine.** `gait-ops` is its own workspace crate (a *sibling* of `gait-workflow`,
  not a module inside `gait-core`) and depends on `gait-core` + `gait-proto` **only** — never
  `gait-workflow`. That dependency shape alone makes `run_workflow`/`single_prompt_run`
  unreachable, no matter what wrapper code exists. `gait-core` still exposes its turn engine
  publicly, though, so a per-crate `crates/gait-ops/clippy.toml` bans the reachable entrypoints via
  `disallowed-methods` (`gait_core::embed::run_loop`, `run_one_off_turn`, `turn::run_turn`,
  `turn::brain::run_brain`, `turn::hand::run_hand`). `crates/gait-ops/tests/ban_list_complete.rs`
  recursively scans `gait-core`'s `src/embed.rs` + `src/turn/**` for `pub async fn run_*` and fails
  the build unless every match is either in that ban list or in an explicit, reasoned allowlist —
  so a new turn entrypoint can't silently land unbanned. (Honest limitation: `disallowed-methods`
  only catches named leaves, not a hypothetical new wrapper around a banned call — dependency
  unreachability, not the clippy ban, is the load-bearing guarantee.)

### The op contract

Every op in `gait_ops::{workspace, session, permission, extension, auth, daemon, config}` has the
same shape:

- **Typed params in — never a clap `Args`.** clap types stay in `gait-cli`; an op takes plain
  owned/borrowed values or a small params struct defined in `ops`.
- **`Result<Outcome, OpError>` out**, never `()`. `Outcome` carries exactly the data a frontend
  needs to render — no formatting, no exit code.
- **`OpError`** (`thiserror`) is a typed enum — `NotFound`, `Conflict`, `SessionBusy`,
  `ConfirmationRequired`, plus `Other(#[from] anyhow::Error)` — so a structured frontend (ACP/
  JSON-RPC) can match variants instead of string-matching an `anyhow::Error`.
- **No I/O to the user: no stdin reads, no prompts, no `process::exit`.** Fixed-value confirmations
  (e.g. an API key) are passed in as a param — the frontend owns the prompt. Count-dependent
  confirmations (e.g. "discard N sessions?") use a preview→act pair instead of a bare
  `confirmed: bool` — see `session::classify_bulk` (pure preview, no effect) /
  `session::bulk_act(plan, confirmed)` (acts, refusing with `OpError::ConfirmationRequired` if
  `confirmed` is false).
- **May be `async fn`** only for idempotent network reads (`auth::models`). Never blocks on stdin.

Read-only/`--json` outcomes that both a CLI table and a `--json` payload need to render from (bulk
outcomes, `ps`, `stats`) carry `to_rows()`/`to_json()` helpers on the `Outcome` type itself, in
`gait-ops` — returning `Vec<(…)>`/`String` data only (Wall-1-clean), never printing. This keeps the
`--json` shape as one source of truth instead of letting the CLI and any future frontend re-derive
it independently and drift apart.

### Frontend adapter pattern

Each `cmd_*` in `gait-cli` is a thin adapter: parse clap `Args` → build op params → call
`gait_ops::…` → on `Ok(outcome)` render + `exit(0)`, on `Err(e)` render + `exit(1)`. `gait-cli`'s
`#[allow(clippy::print_stdout, ...)]` is fine there — the deny lives only on `gait-ops`. The REPL
(`gait-repl`) calls the same ops in-process (no more `Command::new(gait_bin)` shell-outs for
`undo`/`unlock`/`load`/`unload`); a future ACP adapter would do the same, mapping a request to op
params and an op result to a JSON-RPC response.

---

## Sealed execution: brain / spine / hands

The *same* `run_loop`/`run_turn` runs in one of two modes, decided by whether the turn has a
**relay** (`TurnInput.relay`):

- **In-process** (`relay.is_none()`) — the embed/test path. `run_turn` reads and writes the
  gait-dir directly. Simple, unsandboxed.
- **Sealed relay** (`relay.is_some()`) — the production `gait run` / `gait chat` path. A **spine**
  (the parent process) spawns a sealed **brain** (`gait __brain`) per turn and a sealed **hand**
  (`gait __run-tool`) per tool call. The brain runs the identical `run_loop` but does **no disk
  I/O**: it receives its inputs in a `BrainInit` signal and delegates every write (events via
  `Append`, commits, process launches via `Spawn`) and the disk reads it can't do (edit/write
  previews) to the spine over the **Nerve** — length-prefixed `gait.sandbox.v1.Signal` framed over
  the child's stdio.

Both `cmd_run` (CLI) and `run_chat` (REPL) take the sealed path through the shared
`spine::relay_brain_turn` — the same way they share `run_loop`. The spine is the **sole disk
reader+writer and capability authority**; the LLM-facing brain holds the fewest capabilities; each
hand is kernel-sealed (Seatbelt/Landlock) to exactly the scope in its `Impulse`. Full role
breakdown, threat model, and seals: [`sandbox-security.md`](sandbox-security.md).

> Under the sealed path the **spine** performs the per-turn worktree/checkpoint/commit writes
> described in "The Three Repos · During a turn" (the brain has `writable: []`); under the
> in-process path `run_turn` does them itself.

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
    ├── log.jsonl                          ← the event log (source of truth; incl. session identity)
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
projection summing every `TurnComplete.usage`. Exposed via `gait cost <session-id>`.

### ✓ `session.json` folded into the log
**Resolved.** The `session.json` sidecar is gone entirely. Session identity (`codebase_path`,
`assigned_by`) now lives on the `SessionStart` event; readers use `session_meta::read_session_meta`
(file) / `from_events` (in-memory). The dead `agent_name`/`system_prompt`/`created_at` fields were
dropped (the live system prompt is already replay-complete on the `Agent` event). This also lets the
sealed brain derive identity from `BrainInit.events` instead of reading disk.

### ✓ `compat_rewrite_keys` shim deleted
**Resolved** in the same cleanup. The silent legacy-key rewriter (`manifest`→`agent`,
`compaction`→`summary`) is removed; `read_events` parses lines directly, so old-format logs now fail
loudly instead of being silently transformed — matching the no-backward-compat policy.

### ✓ `bin/gait.rs` command bodies held turn logic inline
**Resolved** by the turn-I/O boundary (Phases 1–5).
`cmd_run` moved to `cli/run.rs` and both it and the REPL now drive the unified `embed::run_loop`;
`bin/gait.rs` is a thin dispatcher. Turn-driving is testable without the binary (`tests/integration_run_loop.rs`, `tests/integration_embed.rs`).

### ✓ `stream.rs` permission-free duplicate loop removed
**Resolved.** The second 50-step agentic loop that called `tool.execute()` with no permission
checks is gone; tool-less generation routes through the production path. No `crate::stream` callers remain.

### ✓ `PermissionItem` moved to proto
**Resolved.** `PermissionItem` is now a `gait.log.v1` proto message (`log.proto:282`), shared cleanly
by the log and the renderer — the old `ui/permission.rs → session::transcript::PermissionItem` upward
import is gone (and `ui/permission.rs` no longer exists). The `preview` field lives on it as a proto
field, now filled **spine-side** under the seal (see `sandbox-security.md`).

---

## Known Issues / Boundary Violations

### 1. `turn/mod.rs` is still large (~1.9 K lines)
The `turn.rs` monolith was split into a module: `prompt.rs`, `gate.rs`, `exec.rs`, and the sealed
runtimes `brain.rs`/`spine.rs`/`hand.rs`/`nerve.rs`/`synapse.rs` are now separate files (tests live in
`turn/tests/`). What remains in `mod.rs` is the `run_turn` loop itself plus signal dispatch and the
scope/unlock helpers (`check_own_tool_scope`, resume handling). No longer the ~110 KB single file, but
still a candidate for a further `loop`-vs-`signals` split.

# Agent Architecture — Primitive Decomposition

**Date:** 2026-06-10
**Status:** Design v2 (incorporates code-grounded + adversarial review; grorg/archebuild composability)

> **v2 revisions** (from review): split `Session` into a durable token + a live
> `ResolvedProfile`; named the `Retrace` (Log→Transcript reconstruction) as a primitive and
> scoped `Transcript` to a read-only projection (general mutability deferred to its own
> spec); specified deny-monotonicity, the Sandbox⊇Permission invariant, Loadable-supplied
> permission classifiers, the Scheduler/Harness mechanism-vs-policy boundary, and
> Profile→Session re-resolution; corrected the `TurnOpts` field audit and the "unchanged"
> claims.

## Problem

gait's notion of "an agent" is real but unnamed and scattered:

- There is **no `Agent` type**. A running agent is `SessionJson` (a bare identity struct)
  plus `TurnOpts` (a god-bag mixing *what the agent is* — tools, permissions — with *what one
  execution needs* — codebase path, workspace, repos — with *per-call inputs* — task, auto).
- The actual assembly is rebuilt by hand in **four composition roots** (`bin/gait.rs`,
  `exec_prompt.rs`, `runner.rs`, `harness.rs`).
- `TurnOpts` and `Repos` are grab-bags conflating distinct concerns.
- "Transcript" conflates the immutable audit record with the working context the model sees.

This is **consolidation, not a rewrite.** Most names stay; the net change is a handful of new
names, three splits, and one deletion.

## Guiding model

Decompose like an open system: a **system** (the agent) coupled to **baths** (what it acts
on), with **interactions** (capabilities), a **boundary** (reach), and two immutable
**worldlines** whose working states are projections. Each primitive answers one question. The
physics framing is intuition only; the **coding names are canonical.**

## Primitives

| Coding name | Answers | Intuition |
|---|---|---|
| `Model` | what generates words? | the generator (stateless) |
| `Tool` | what can it do? | an interaction operator |
| `Loadable` | where do extra tools come from? | capability sources (mcp/skill/plugin) |
| `Sandbox` (holds `scope`) | how far can effects reach? | boundary condition |
| `Codebase` | what is acted on? | the passive bath (working tree) |
| `Profile` | who is this, reusably? | the operator spec — a **type** |
| `ResolvedProfile` | the live capabilities for one run | the operator *instantiated* |
| `Session` | who is this, running? | a trajectory — a **token** |
| `Log` | what actually happened? | the system worldline (immutable) |
| `Retrace` | how do events become context? | `Log → messages` reconstruction |
| `Transcript` | what does the model see now? | the (read-only) projection of the Log |
| `Checkpoint` | codebase snapshot per turn | environment worldline (immutable) |
| `PermissionPolicy` | what is allowed? | the standing authorization |
| `run_turn` (engine) | advance one step | the propagator |
| `Harness` | how is a turn run + hooked? | execution mechanism |
| `Scheduler` | what runs next / on pause? | orchestration policy |

### `Model` — the generator
Stateless map `context → words (+ tool intentions)`. Provider + model-id. The composed system
string is *passed in*, not owned. **Today:** `Provider` trait (`provider.rs`). The current
`model: String` lives on `TurnOpts`/the session-start event; it is a *declarative ref* (see
`Profile`) that resolution turns into a live `Model`. **Change:** ✅ keep the trait.

### `Tool` — a capability
One interaction. Self-describes its action via `permission(args)` (see Permission model).
**Today:** `Tool` trait (`types.rs:188`); already carries `permission()`,
`execution_signal()`, `output_filter()`. **Change:** none.

### `Loadable` — capability sources
mcp servers, skills, and (future) wasm plugins resolve into `Tool`s. **Today:** `LoadableKind`
enum + `LoadableEntry` (`types.rs:241`) + `McpClient` + the registry. Note: the *enabled* set
is reconstructed by replaying `load` tool calls from the Log (`derive_enabled_loadables`,
`turn.rs:155`) — that replay is part of the `Retrace`. **Change:** 🔧 a Loadable must also supply
a `permission(args)` classifier at registration (see Permission model).

### `Sandbox` — the boundary
A hard surface: no effect of this agent, by any channel (incl. shell subprocesses), crosses
it. Holds the declared `scope`. **Today:** does not exist. **Change:** 🆕 new — lands the
scoped-agents plan (`2026-06-10-scoped-agents.md`). See the Sandbox×Permission invariant.

### `Codebase` — the environment
The working tree the agent edits. **Today:** a codebase path + `CodebaseRepo`
(`git/codebase.rs`). Note: unlike `Transcript←Log`, the Codebase is *authoritative* and the
`Checkpoint` trails it — the two worldlines are symmetric for *undo*, not for *derivation*.
**Change:** 🔧 wrap path + `CodebaseRepo`. Default: a `Session` field (it's thin), kept in the
primitive set as a conceptual name rather than necessarily a standalone type.

### `Profile` — the reusable definition (type)
Declarative: `Model` ref, instructions, `Tool`/`Loadable` set (+ which are enabled at start),
`Sandbox`/scope, `PermissionPolicy`. Serializable, shareable — the grown-up `.md` frontmatter.
**Today:** scattered across `agent_frontmatter.rs`, `config.rs`, and
`SessionJson.{agent_name, system_prompt}`. **Change:** 🆕 name + consolidate.

### `ResolvedProfile` — the live capabilities (resolution of a Profile)
The non-serializable runtime: live `Arc<dyn Model>`, `Vec<Box<dyn Tool>>` (incl. resolved
Loadables + their clients), `Sandbox`, `PermissionPolicy`, mid-turn `hooks`, and the *composed*
system string. Built by resolving a `Profile` against config + available providers/servers.
**Why separate from Session:** it can't be serialized and must be **re-resolved each process**
(the harness re-spawns `gait run --session X` as a subprocess). Keeping it off the durable
token is what makes resume and re-resolution honest — and it's validated by existing code: the
`curate` OutputFilter already builds a capability set and calls `run_turn` recursively with no
durable identity (`turn.rs:349`). **Change:** 🆕 new; absorbs the live half of `TurnOpts`.

**Re-resolution contract.** Pin the resolved `Model` id + tool names into the Log at session
start (the `model` field already does half of this). On resume, *diff* the current Profile
resolution against the pinned set; define drift policy (warn / pin-to-original / fail). The
`Retrace` must tolerate Log `ToolCall`s for tools absent from the current resolution (render
them, don't panic, don't re-permission-check a vanished tool).

### `Session` — the durable instance (token)
Serializable identity + bindings + history handles only: `id`, `assigned_by`, codebase
binding, `Log`/`Checkpoint` handles, `token_totals`, `created_at`, and its `SessionLock` (the
PID file guarding concurrent execution). **Not** the live provider/tools — those are
`ResolvedProfile`. **Today:** `SessionJson` (identity) + the durable slices of `TurnOpts`.
**Change:** 🔧 this is a **rename + expansion** of `SessionJson` (it does not exist as
`Session` today). The engine entry becomes `run_turn(session: &mut Session, caps:
&ResolvedProfile, input: TurnInput)`, where `TurnInput` bundles the per-call `task` with
execution opts (`auto`, `max_iter`, `output_schema_retries`).

### `Log` — immutable history (system worldline)
Append-only event stream. The audit truth. The model **never** edits it. **Today:** `AnyEvent`
append-only (`transcript.rs`), stored in the `TraceRepo`. The JSONL wire format is **frozen**
(invariant). **Change:** ✅ keep; rename the concept from "transcript" to "log."

### `Retrace` — the reconstruction (Log → working state)
The function that rebuilds the model's working state from the Log. It already computes **two**
projections, via **two different mutation patterns**:
1. the **message list** — pairs `ToolCall`/`ToolResult`, honors `Compaction` breakpoints
   (prefix truncation);
2. the **enabled-loadable set** — `derive_enabled_loadables` (`turn.rs:155`) replays
   `load`/unload events as **last-write-wins toggles per named entity**.

That second pattern is the important one: mutation-via-replayed-events *already works today*,
and LWW-per-entity is precisely the ordering semantic general transcript mutation will need.
So the Retrace is **already** the home of "the model mutates its state by emitting Log events the
Retrace interprets" — it just does it over auxiliary state, not over the message list yet. **This
is the load-bearing primitive for any future mutable transcript.** **Today:** `context.rs`
reconstruction + `derive_enabled_loadables`. **Change:** 🆕 name it (today it's unnamed); its
contract is where the future mutation spec plugs in.

### `Transcript` — the working view (read-only projection)
What the `Model` reasons over = the message-list output of the `Retrace`. The *message list* is
read-only today, its only mutation being prefix-`Compaction`. (The Retrace *does* already mutate
auxiliary state — the enabled-loadable set — via LWW toggle replay; see `Retrace`.) General
message mutation (`redact`/`replace` by event-id) is **out of scope here**: it reuses the
loadable toggle's LWW-per-entity replay, so the genuinely new piece is the `ToolCall→ToolResult`
**pairing cascade** (redacting a call must drop its result, or the provider rejects the orphan)
plus any new Log fields — which gets **its own spec**. We split the *name* now so that spec has
a home; we do **not** advertise general message mutability as a current capability. **Change:**
🔧 split by name from `Log`.

### `Checkpoint` — environment worldline
Immutable codebase snapshots per turn. **Today:** `CheckpointRepo` (`git/checkpoint.rs`) + the
checkpoint SHA stored on `TurnComplete`. Note: there is no `Checkpoint` *struct* — the concept
is the `CheckpointRepo` trait plus a SHA string. **Change:** ✅ keep.

### `PermissionPolicy` — the standing authorization
`allow` / `deny` / `ask` patterns. See the Permission model for layering + precedence.
**Today:** `permission.rs` matchers + `ToolPermission`/`PermissionRequest` (`types.rs`) +
`deny`/`allow` on `TurnOpts`. **Change:** 🔧 keep the matchers; move ownership onto
`Profile`/`ResolvedProfile`, off `TurnOpts`.

### `run_turn` — the propagator
`run_turn(session, caps, input) -> TurnResult`, where `input: TurnInput` carries the per-call
`task` plus execution opts. Reconstructs `Transcript` via `Retrace` →
calls `Model` with `Tool`s → gates via `PermissionPolicy` → executes within `Sandbox` against
`Codebase` → appends to `Log` and `Checkpoint`. The per-call `task` + execution opts (`auto`,
`max_iter`, `output_schema_retries`) live on `TurnInput`, not on any durable type. **Change:**
🔧 slim signature; `TurnOpts` dissolves.

### `Harness` — the execution mechanism
Runs a turn and *executes* lifecycle/coordination side effects on request:
`inject_into(session_id, event)`, `resume(session_id)`, commit-trace, take-lock, spawn-process.
Pure mechanism. **Today:** `NativeHarnessBackend::after_turn` (`harness.rs:36`) — but it
currently *also* embeds the policy decision. **Change:** 🔧 narrow to mechanism the
`Scheduler` calls.

### `Scheduler` — orchestration policy (incl. pause handling)
Decides *what runs next* and *what happens when a turn pauses for input*. Returns **action
descriptions** (`ResumeAssigner(id)`, `EscalateAndPark`, `SpawnChild(...)`), never side effects
— the `Harness` executes them. This is where the dropped `User`'s homeless obligation lands:
on `TurnResult::{Clarify, Inform, Permissions}` (a suspend), the Scheduler decides escalate vs
park vs answer-from-parent. **Both external consumers own this layer** — grorg's Fleet Server
and archebuild's DAG runner — so it is the seam they replace or drive. **Default:**
`NativeScheduler` = today's `assigned_by → ResumeAssigner` peer-spawn. **Change:** 🔧 extract
from `Harness`; `Scheduler` decides, `Harness` does.

## Permission model (request × policy → decision)

Three roles that share the word "permission":

1. **Request (Tool/Loadable).** `permission(args)` returns `Passthrough` (no gate — the
   control-class tools `output`/`clarify`/`inform` that reach the *user*, not the codebase) or
   `RequiresApproval(Vec<PermissionRequest>)` — a self-description of the action. **Code-only.**
   - **Asymmetry (be honest):** native tools self-describe with argument granularity
     (`shell:rm`, `edit:src/**`). MCP/dynamic tools currently fall through to the trait default
     `RequiresApproval([tool_name])` — *name-granular only*. So `mcp__x__delete` and
     `mcp__x__read` are both bare-name approvals. **Fix:** a `Loadable` must supply a
     `permission(args)` classifier at registration (even coarse: read/write/destructive), so
     the Request side is populated at discovery time. The "tool-supplied default policy" is
     therefore **primary** for Loadables, not an optional extra.
2. **Policy (Profile).** `allow`/`deny`/`ask` patterns. **User-facing; frontmatter + config.**
   - **Precedence:** `tool-default < global config < agent frontmatter < runtime approval`,
     **but `deny` is monotonic**: any layer's `deny` is final and cannot be re-allowed by an
     inner layer. Only `allow`/`ask` are innermost-wins. (Without this, an LLM-authored worker
     Profile could re-allow a globally-denied destructive command in an unattended `--auto`
     run.)
3. **Gate (`run_turn`).** Request × Policy → Allow / Deny / Ask. `ask` returns
   `TurnResult::Permissions`; the reply is logged and may update Policy at once/agent/global.

User-facing surface is **frontmatter only**; tool self-descriptions are built into code.

## Sandbox × PermissionPolicy (two gates, one invariant)

Soft per-interaction `PermissionPolicy` and hard whole-system `Sandbox` are distinct and can
*disagree* — a permission may allow a write the sandbox kills at the syscall level, surfacing
as an opaque `EPERM` the model can't reason about (and `--auto` loops retrying).

**Invariant:** the `Sandbox` scope must be a **superset** of everything `PermissionPolicy` can
allow. Validate at **resolve time** — if a Profile allows a path the Sandbox excludes, fail
fast at `ResolvedProfile` construction, not at syscall time. And: sandbox denials must be
caught and rendered as a **distinct, non-retryable** tool result ("blocked by sandbox
boundary"), so the model sees a boundary, not a flaky command.

## The two worldlines

| | Immutable record | Stored in | Working view | Derived each turn? |
|---|---|---|---|---|
| **System** | `Log` | `TraceRepo` (`traces`) | `Transcript` (via `Retrace`) | **yes** — rebuilt from Log |
| **Environment** | `Checkpoint` | `CheckpointRepo` | `Codebase` | **no** — Codebase is authoritative |

`Log:Transcript` and `Checkpoint:Codebase` are symmetric for **undo** (reset a working view to
a record point) but **not** for derivation (you rebuild Transcript from Log; you do not rebuild
the Codebase from a Checkpoint). `TraceRepo` keeps its name: it stores the Log *plus* command
outputs and other artifacts, so it is not "the Log."

## Migration — the grab-bags dissolve

**`TurnOpts`** (`turn.rs:22-47`). Note `provider` and `tools` are **`run_turn` arguments**, not
struct fields; the rest are fields:

| Current field | Goes to |
|---|---|
| `task` | `TurnInput` (the `run_turn` input arg) |
| `auto`, `max_iter`, `output_schema_retries` | `TurnInput` (per-call execution opts) |
| `allow`, `deny` | `PermissionPolicy` (on Profile) |
| `system_prompt` (composed live string) | built during resolution → `ResolvedProfile` |
| `model` (declarative ref) | `Profile` → resolves into `ResolvedProfile`'s `Model` |
| `mcp_registry`, `mcp_clients`, `skill_registry` | `ResolvedProfile` (resolved Loadables) |
| `config_enabled` | `Profile` (initial enabled set); runtime changes via `Retrace` replay |
| `hooks` (mid-turn) | `ResolvedProfile` (distinct from the `Harness` post-turn hook) |
| `codebase_path`, `workspace` | `Session` (bindings) / `Codebase` |
| `repos` | splits → `Codebase` + `TraceRepo`(Log) + `CheckpointRepo`(Checkpoint) |
| `session_id` | `Session` (identity) |

**`Repos`** (`git/mod.rs`): `.codebase → Codebase`, `.traces → TraceRepo` (backs `Log`),
`.checkpoints → CheckpointRepo` (backs `Checkpoint`). *(verified complete)*

**`SessionJson`** (`session/session_json.rs`): `agent_name`, `system_prompt` → `Profile`;
`id`, `assigned_by`, `codebase_path` → `Session`; `token_totals`, `created_at` → `Session`.
*(verified complete)*

**Net change.** 🆕 `Sandbox`, `Profile`, `ResolvedProfile`, `Retrace`. 🔧 splits: `TurnOpts` →
`Session` + `ResolvedProfile` + `TurnInput`; `Log`/`Transcript`; `Harness`/`Scheduler`.
❌ drop `User` (the suspend/escalation obligation moves to `Scheduler`).
**Renamed (not "unchanged"):** `SessionJson` → `Session` (rename + expand). **Genuinely
unchanged:** `Provider`(→`Model` role), `Tool`, `LoadableKind`/`LoadableEntry`, `TraceRepo`,
`CheckpointRepo`, the permission-matching functions. **Not a struct (concept only):**
`Checkpoint` (= `CheckpointRepo` + SHA).

## Composability with external orchestrators

gait is a building block for outer orchestrators (grorg via library; archebuild via CLI
subprocess). The internal decomposition is invisible to both. The refactor is safe **iff** it
preserves:

1. **CLI surface** — `gait run --workspace --codebase --task --session --auto …` flags stay.
2. **Public library entry** — a deliberate `run_turn`-equivalent returning `TurnResult` + cost/
   tokens/status (grorg's import). Internals become `Session`+`ResolvedProfile`-based; the
   public call stays clean.
3. **Durable `Log`** — JSONL flushed per turn, never buffered library-internally.
4. **Shadow-git isolation** — never write the codebase's real `.git`. `Checkpoint` honors this;
   the new `Sandbox` must too (archebuild runs gait inside a worktree it owns).
5. **`TurnResult` → exit-code mapping** — stable (`Done`→0; pauses→distinct codes).
6. **`Scheduler` is the orchestration seam** — external schedulers replace/drive it.

## Out of scope / future

- **General mutable `Transcript`** — `redact`/`replace`-by-id over the message list. Reuses
  the loadable-toggle LWW-per-entity replay the `Retrace` already does; the new work is the
  `ToolCall→ToolResult` cascade + any new Log event fields. **Its own spec.** This design only
  names `Retrace` + `Transcript` so that spec has a clean home.
- **Scoped-agents enforcement** (`Sandbox` impls) — `2026-06-10-scoped-agents.md`.
- **wasm plugins** (`Loadable` plugin kind + lifecycle hooks) — `2026-06-07-wasm-plugins.md`.
- **Fleet stack** — `2026-06-10-fleet-vision.md` constrains this design; doesn't change it.

## Non-goals

- Not a rewrite. The slice traits stay; this is consolidation + naming.
- Not a security sandbox for untrusted code (that remains `--container`).
- No change to the `Log` JSONL wire format (frozen invariant).

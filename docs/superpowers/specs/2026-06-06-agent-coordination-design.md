# Agent Coordination Design

**Date:** 2026-06-06  
**Status:** Draft

---

## Problem

The current `spawn_subagent` tool calls `runTurn` recursively in-process and blocks until the child finishes. This breaks in four ways:

1. **Clarify/inform can't cross agent boundaries** — a child agent can't ask a question that routes to another agent or a specific human.
2. **No async fan-out** — the parent blocks; it can't do other work while children run.
3. **No agent discovery** — agents can't see who else is active.
4. **Tight coupling to gait internals** — external orchestrators (grorg) can't participate in coordination.

---

## Design

### Core principle

Agents are peers, not a tree. There is no parent/child — only sessions with an optional `assigned_by` pointer that records who created them. Coordination (routing, re-running, injecting) is handled by a harness backend. Gait itself is stateless per-turn.

---

### Session metadata

`session.json` gains one new optional field:

```json
{
  "id": "s-abc123",
  "assigned_by": "s-parent456",
  "codebase_path": "...",
  "created_at": 1234567890,
  "token_totals": { ... }
}
```

`assigned_by` is the session ID of whoever spawned this session. Absent for top-level human-initiated sessions.

---

### New transcript events

#### `agent_update`

Injected by the harness into the assigner's transcript when an assigned session exits.

```jsonc
{
  "type": "agent_update",
  "session_id": "s-child",
  "status": "done" | "clarify" | "inform",
  "summary": "short text — last assistant message or clarify summary",
  "items": [ /* clarify or inform items, if status is clarify/inform */ ],
  "ts": 1234567890
}
```

- `summary` is always short (last step text or a one-liner). Full result lives in the child's transcript; the assigner's LLM can read it via the `read` tool if needed.
- `items` is only present when `status` is `clarify` or `inform`.

#### `clarify` and `inform` — new `to` field

Each item in a `clarify` or `inform` event gains an optional `to` field:

```jsonc
{ "id": "c1", "content": "Which db engine?", "why": "schema design", "kind": "is", "to": "human:alice" }
{ "id": "c2", "content": "Is this approach correct?", "why": "validation", "kind": "ought", "to": "agent:s-parent456" }
```

`to` format:
- `"human"` — default; routes to whoever is running the session interactively
- `"human:name"` — specific named human (org-aware; harness resolves to actual person)
- `"agent:session-id"` — specific agent session

Absent `to` defaults to `"human"`.

---

### New tools

#### `spawn_agent`

Creates a new session and returns its ID immediately. Does **not** block.

```typescript
spawn_agent({
  task: string,           // task description injected as first user_turn
  context_files?: string[], // files to prepend as context
  to?: string,            // who should receive the result — defaults to this session's id
})
// returns: { session_id: string }
```

Gait's side: creates `sessions/{newId}/session.json` with `assigned_by: currentSessionId`, writes the initial `session_start` + `user_turn` events, commits the branch, returns the session ID. Does not call `runTurn`.

The harness sees the new branch and schedules the session for execution.

#### `list_agents`

Returns currently active agent sessions, sorted by last commit timestamp descending.

```typescript
list_agents()
// returns: [{ session_id, last_active_ts, status, summary }]
```

**Active** means the session has no terminal event in its transcript. Terminal events are: `turn_complete` where the session has no pending `agent_update` from a child, or an `error` with no subsequent recovery.

Implementation: reads `git branch --list 'session/*'`, for each branch reads the last event from its transcript file. Sessions whose last event is `turn_complete` with no outstanding assigned children are excluded.

Note: session branches are orphan branches (no shared history with main). "Not merged into main" is not computable via `git merge-base`. Active/done state is determined solely from the transcript.

---

### HarnessBackend interface

```typescript
interface HarnessBackend {
  // Called at the end of every runTurn, after transcript commit.
  // Responsible for routing agent_update to the assigner and scheduling re-runs.
  afterTurn(
    sessionId: string,
    assignedBy: string | undefined,
    result: TurnResult,
  ): Promise<void>
}

type TurnResult =
  | { type: "done";    summary: string }
  | { type: "clarify"; items: ClarifyItem[] }
  | { type: "inform";  items: InformItem[] }
  | { type: "permissions" }  // needs human approval — routes to human by default
```

Two implementations ship with gait:

**`NativeHarnessBackend`** (default):
- If `assignedBy` is set: acquires lock on assigner session, appends `agent_update` event, commits, releases lock, invokes `gait run --session {assignedBy}` as a child process.
- If no `assignedBy`: for `clarify`/`inform`, prompts the interactive user as today. For `done`, exits cleanly.

**`GrorgHarnessBackend`**:
- Calls grorg's API (via MCP or HTTP) to update the corresponding Action node.
- Grorg handles routing to the right agent or human via its work graph dispatcher.
- `afterTurn` is a fire-and-forget call; grorg schedules re-runs independently.

Selected via `gait.jsonc`:

```jsonc
{
  "model": "...",
  "harness": "native",         // default
  // "harness": "grorg",
  // "harnessConfig": { "url": "http://localhost:9090" }
}
```

---

### Commit and lock ordering

The invariant: **one writer at a time per session transcript**.

`acquireLock` throws immediately if a live PID holds the lock — it does **not** block. So writers must use a wait/retry loop:

```
retry until acquired (with backoff, max N attempts):
  try acquireLock(sessionDir)
  on "already running": wait, retry
append event(s) → commit → releaseLock
```

**If the assigner is mid-turn when a child finishes**, injection must wait. The harness retries until the assigner's turn completes and releases its lock, then injects and commits. It does not attempt to re-run the assigner until after the current turn naturally exits — `gait run` on a locked session will fail anyway.

When multiple child agents finish simultaneously and all target the same assigner, the harness serialises via the lock, batching all pending `agent_update` events for a session in one shot before releasing.

The harness never holds the assigner's lock when calling `gait run`. Release first, then re-run.

---

### Compaction

`runTurn` checks for compaction at the top of every turn before the LLM call. Injected `agent_update` events are small by design (short summary string + optional small items list, not the full child transcript). If multiple children finish simultaneously, the harness batches all their `agent_update` events into one injection before triggering a re-run — so compaction fires once, covering all of them.

Hard bound: `agent_update.summary` is truncated at 500 characters. `agent_update.items` mirrors the original clarify/inform items which are already small. This bounds worst-case injection size.

---

### What gets removed

- `src/tools/spawn_subagent.ts` — replaced by `spawn_agent`
- `src/tools/spawn_subagent.test.ts`
- The `registerSubagentRunner` / in-process recursive `runTurn` call in `run.ts`
- The `isSubagent` flag and all its branches in `run.ts`
- The `needs_clarification` / `needs_inform` status values added in the recent fix — those were a stopgap for the old model

---

### What stays

- All clarify/inform/convergence logic — just gains a `to` field on items
- All transcript event types — plus the new `agent_update`
- The lock infrastructure
- Compaction — unchanged
- MCP client — unchanged
- Permissions — unchanged; `afterTurn` gets a `permissions` result type so the harness can route approval requests

---

### Migration path

1. Add `agent_update` to transcript schema + `to` field to clarify/inform item schemas
2. Implement `spawn_agent` tool + `list_agents` tool
3. Implement `HarnessBackend` interface + `NativeHarnessBackend`
4. Wire `afterTurn` call into `runTurn`
5. Delete `spawn_subagent` + `isSubagent` + the recent stopgap changes
6. Update `context.ts` to reconstruct `agent_update` events into messages

---

## Open questions

- Should `spawn_agent` write the initial `user_turn` event itself, or just create `session.json` and let the harness inject the task?
- Does `NativeHarnessBackend` need a queue/scheduler, or is spawning a `gait run` child process sufficient for the single-user case?
- For grorg: does `afterTurn` call grorg's MCP server, its gRPC API, or a CLI? Depends on what grorg exposes first.

## Known risks (from review)

- **Lock retry semantics**: `acquireLock` throws, not blocks. Harness needs explicit wait/retry — see §Commit and lock ordering. Max retry count and backoff need concrete values.
- **Cycle protection**: `to: "agent:..."` enables A→B→A clarify loops and re-run storms. Mitigation: track clarify chain depth in session metadata; reject routing that would exceed a max depth (e.g. 5). `NativeHarnessBackend` enforces this; grorg uses its own cycle detection.
- **Dead-letter routing**: if `to: "agent:s-xyz"` points to a terminated session, or `human:alice` cannot be resolved, the harness must surface the error to the nearest available human rather than silently dropping the item.
- **Grorg state divergence**: `GrorgHarnessBackend` delegates scheduling to grorg, which means grorg's work graph becomes the source of truth for re-runs. This is intentional but means git transcript state and grorg state can diverge if grorg has a bug. Acceptable tradeoff; document explicitly.

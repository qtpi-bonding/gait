# Fleet Vision — grorg · gait · archegraph/archebuild

**Date:** 2026-06-10
**Status:** Vision / north-star (not an implementation spec; constrains the others)

The end state: a persistent **grorg Fleet** manages many projects (some dependent, some
independent). You add intent to grorg; the stack turns it into merged code, reporting
progress back up and escalating to you only when it must. This doc fixes the **layers, the
data-flow directions, and the open problems** so the grorg, gait, and archegraph/archebuild
designs stay consistent.

## The stack

```
        ┌─────────────────────────────────────────────────────────┐
        │  grorg Fleet   — cross-project work DAG; what/when;       │
        │                  scheduler, cron, event router; the hub   │
        │                  for human interaction                    │
        └───────────────┬──────────────────────────▲───────────────┘
            spawn (intent)                          │ progress / escalation
                        ▼                           │ (bridge + grorg API)
        ┌─────────────────────────────────────────────────────────┐
        │  Orchestrator gait  — one feature build.                  │
        │    · authors the archegraph spec from your request        │
        │    · runs archebuild; MONITORS node workers; can correct  │
        │    · escalates to you via grorg.create_action             │
        └───────────────┬──────────────────────────▲───────────────┘
            run DAG                                 │ node events (observe)
            + control                               │
                        ▼                           │
        ┌─────────────────────────────────────────────────────────┐
        │  archebuild  — derive DAG from spec; topological parallel │
        │                execution; worktree-per-node; convergence  │
        └───────────────┬──────────────────────────▲───────────────┘
            spawn worker                            │ NodeHookEvent
                        ▼                           │
        ┌─────────────────────────────────────────────────────────┐
        │  Worker gait  — implement ONE node, scoped to its subtree │
        │                 in an archebuild-owned worktree           │
        └─────────────────────────────────────────────────────────┘
```

**gait appears twice** — as the orchestrator (planner/supervisor) and as the per-node worker.
They are **two different `Profile`s** (per the agent-architecture spec): the orchestrator has
archebuild-invoking + grorg tools and broad scope; the worker has code-editing tools scoped
to one node. Naming them as two Profiles keeps "gait calls gait" legible.

Each layer is a **`Scheduler` at its own granularity**: grorg = what/when across projects;
archebuild = how to decompose one feature; gait = do one unit. Resources and intent flow
down; results and questions flow up.

## Flow directions

| Direction | Carries | Mechanism |
|---|---|---|
| **down** | intent | grorg spawns orchestrator-gait; orchestrator runs archebuild; archebuild spawns worker-gaits |
| **down** | control | orchestrator pauses / re-tasks / cancels nodes mid-run (requires steerable archebuild) |
| **down** | concurrency budget | hierarchical leases (see below) |
| **up** | progress | `NodeHookEvent` → orchestrator → bridge → grorg `Session` |
| **up** | escalation | worker/orchestrator → `grorg.create_action` → you |

The key correction over a naive pipeline: **the stack must move information *up* mid-flight**
(questions, blockers), not only completion at the end. grorg is the hub that surfaces those
to the human.

## Human-in-the-loop: grorg is the only door

gait gets a **`grorg.create_action` tool** (and grorg's API generally). That is the single
channel for reaching the human:

- A worker hits an unanswerable decision → it escalates to its **orchestrator-gait** (its
  parent/observer), which either corrects it directly or, if it needs *you*, creates a grorg
  action.
- This is the **observer chain** from the agent-architecture spec, tunneled through grorg's
  API rather than a gait-internal `User` type — consistent with dropping `User` as a
  primitive. The human is reached through a *tool*, not a core object.

**Open policy:** what does a worker in `--auto` do with an unanswerable `Clarify` when its
orchestrator is also unattended? Default: **escalate-and-park** (create a grorg action, mark
the node blocked) rather than guess. To be confirmed.

## "Done" means *integrated*, not *built*

grorg sequences dependent projects off a "done" signal. archebuild "done" = *nodes built +
verified in a feature worktree/branch* — **not merged**. The bridge event grorg keys a
dependency edge off must report the **merge/publish (integrated/available)** transition, not
build-complete — otherwise a dependent build starts against unintegrated code. grorg owns the
dependency DAG; this just fixes *which* event satisfies an edge.

## Archebuild must be observable AND steerable

For the orchestrator-gait to "monitor the DAG workers and correct," archebuild cannot be a
black-box `run(dag) → done`:

- **Observable (exists):** it already emits `NodeHookEvent`; the orchestrator consumes that
  stream (the same one feeding the bridge).
- **Steerable (to add):** it must accept mid-run control — pause / re-task / cancel a node,
  adjust parallelism. This is the one genuinely new integration surface the supervised flow
  requires.

## Concurrency governance

Fan-out is multiplicative: `projects × parallel nodes × subagents`. Two complementary
governors, because they bound different resources:

1. **Hierarchical concurrency leases** — make concurrency a resource that flows *down the
   tree like intent*. The Fleet holds a global slot budget; it leases each feature-build a
   sub-budget; archebuild can't exceed its lease; a worker's subagents come out of the *same*
   lease. Enforced at each `Scheduler` seam. Prevents one project starving others. Governs
   **machines / processes / worktrees** (local).
2. **Central LLM gateway** — every gait agent calls the provider *through* one proxy. It
   enforces **global provider rate limits (RPM/TPM) and a cost ledger**, and queues/sheds
   when saturated. Decouples "agents alive" from "API calls in flight," and is the single
   point for cost accounting + provider failover. Governs **the API and the $** (global, and
   usually the binding constraint). gait's per-session `token_totals` aggregate up to this
   ledger.

Freebies already present:
- **Priority** from grorg's critical-path (CPM) — admit critical-path nodes first when slots
  are scarce.
- **Backpressure signal** = lease availability / gateway queue depth; archebuild reads it and
  lowers its own parallelism gracefully instead of hard-blocking.

## Invariants / open problems (the load-bearing list)

1. **Upward channel** — escalation via `grorg.create_action`; archebuild made steerable;
   `--auto` unanswerable-Clarify policy = escalate-and-park. *(protocol-level; nail first)*
2. **"Done" = integrated** — the bridge reports the merge/publish transition as the
   edge-satisfying signal. *(protocol-level)*
3. **Two gait Profiles** — orchestrator vs worker, distinct tools/scope.
4. **Concurrency** — hierarchical leases + central LLM gateway; priority from CPM.
5. **Trace correlation** — a lineage id (grorg action → orchestrator session → build → node →
   worker session) threaded so a failure 3 layers deep is traceable. (gait's `assigned_by`
   lineage should extend across the bridge.)

## Relationship to the agent-architecture refactor

This vision **constrains** `2026-06-10-agent-architecture-design.md` but does not change it.
The refactor must keep the composability boundary that makes this stack possible:
CLI surface, a clean public `run_turn` library entry, durable `Log`, shadow-git isolation,
`TurnResult` → exit codes, and the `Scheduler` seam external orchestrators drive. The new
`grorg.create_action` tool is just another `Tool` (`Loadable`); the orchestrator/worker split
is just two `Profile`s. Nothing here requires a new gait primitive.

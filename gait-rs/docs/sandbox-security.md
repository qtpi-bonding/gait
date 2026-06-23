# Sandbox Security: brain / spine / hands

gait runs a turn as **three processes with different trust levels**, so the part that
talks to the LLM holds the fewest capabilities. This is the security model; the
layer/data architecture is in [`architecture.md`](architecture.md).

**Spec:** `docs/superpowers/specs/2026-06-20-sandbox-sealed-tool-runner-design.md`
**Implementation plans:** `docs/superpowers/plans/2026-06-21-*` (brain-init, profile-spec-via-spine, unify-hand-response, spine-enriched-previews, flip-brain-scope, session-into-log).

## The three roles

```
                    ┌──────────────────────────────────────────────┐
   LLM API  <──────│  BRAIN   (gait __brain)                       │
   (network)        │  sealed: read_all:false, readable:[],         │
                    │          writable:[], network:All             │
                    │  pure compute: runs the turn loop, no disk    │
                    └───────────────▲───────────────┬───────────────┘
                       Nerve (proto │ Signal)        │ Impulse / HandResponse
                       BrainInit, Append,            │ (proto, per tool call)
                       Feedback, Spawn, …            │
                    ┌───────────────┴───────────────▼───────────────┐
                    │  SPINE   (parent process)                     │
                    │  unsealed, NOT LLM-reachable                  │
                    │  sole disk reader + writer; capability        │
                    │  authority (scope ceiling); spawns brain+hands│
                    └───────────────┬───────────────────────────────┘
                                    │ spawns one per tool call
                    ┌───────────────▼───────────────────────────────┐
                    │  HAND    (gait __run-tool)                    │
                    │  sealed per call to the Impulse's scope:      │
                    │  kernel Seatbelt/Landlock; reads/writes only  │
                    │  the codebase + temp jail it was granted      │
                    └────────────────────────────────────────────────┘
```

### Brain — the sealed inference process
- Runs one turn: streams from the LLM, folds the in-memory event log, decides tool calls.
- **Capabilities:** compute + outbound network (for the LLM call). **No disk reads, no disk writes.**
- Gets everything it would otherwise read from disk via **`BrainInit`** (config, full event log, git snapshot, the composed `Profile`) and the in-memory log mirror fed by its own `Recorder`. See `turn/brain.rs`.
- Is the **only LLM-reachable process** — i.e. the only one whose control flow is influenced by model output. So it holds the fewest capabilities.

### Spine — the trust anchor
- The parent. **Not** driven by model output; it relays structured `Signal`s.
- **Sole disk reader and writer.** Builds `BrainInit`, persists the log (`wire::append_with_id`), commits sessions, checkpoints, builds the `Profile`, renders edit/write previews.
- **Capability authority:** computes the per-call scope ceiling (`bound_request`) that bounds each hand. The brain cannot widen it.
- Spawns the brain (per turn) and hands (per tool call); relays the Nerve.

### Hand — the scoped executor
- A throwaway subprocess for one tool call (`gait __run-tool`).
- Self-applies a **kernel seal** (Seatbelt on macOS, Landlock on Linux) to exactly the scope in the `Impulse` (codebase + temp jail, per-tool read/write/network).
- Runs the tool, returns a single `HandResponse` (success `Feedback` or `HandError`), exits.

## Boundaries (the invariants)

| Concern | Owner | Notes |
|---|---|---|
| Disk read | **spine** (+ hands, scoped) | brain reads nothing |
| Disk write | **spine** (+ hands, scoped) | brain writes nothing |
| Capability authority (scope ceiling) | **spine** | brain proposes, spine bounds |
| LLM / model-driven control flow | **brain** only | the exposed surface holds the least |
| Process launch (spawn_agent, children) | **spine** | brain delegates via `Spawn` signal |
| Preview / diff rendering | **spine** | reads the file the brain may not |

The IPC is one proto envelope per link: **brain↔spine** is the Nerve `Signal`
(`oneof`: `Impulse`, `Feedback`, `Append`, `Spawn`, `Notify`, `StreamForward`,
`Terminated`, `BrainInit`); **spine↔hand** is `Impulse` (request) + `HandResponse`
(reply). Definitions in `proto/sandbox.proto`.

## Threat model — what the seal does and doesn't buy

**Designed against:** a prompt-injected or otherwise-compromised brain. Because the
brain is the LLM-facing process, it's the most likely to be subverted.

**What the disk seal prevents:** the brain **cannot read secrets off disk** —
`~/.ssh`, the gait auth/key store, other projects, other sessions' transcripts —
and so cannot exfiltrate them. Privilege *escalation* (reading/writing outside the
granted scope) is blocked. Trust is concentrated in the spine, which is **not**
reachable by model output and which already had full disk access — so the change is
purely subtractive on the exposed component, not a risk shift.

**What it does NOT do (by design — know these):**
- **Misuse within scope.** The brain still directs the hands. Anything inside the
  granted scope is fair game: read any codebase file, edit code (incl. a backdoor),
  run in-scope shell. A repo-local secret (`.env`) is readable *through a hand*. The
  seal stops escalation, not "the agent does in-scope things you didn't want."
- **In-context exfiltration over the network.** The brain keeps `network:All`. It
  can't read *new* on-disk secrets, but it can send what's already in its context to
  the LLM endpoint. The real exfil *tool*, `webfetch`, runs in a **hand** behind its
  own egress allowlist — but the network is not a closed perimeter on the brain.
- **Absolute kernel guarantees.** macOS-first (Seatbelt), Landlock on Linux; sandbox
  escapes exist. This is defense-in-depth, not a proof.

## Why the brain isn't network-restricted (decision, not oversight)

We considered restricting the brain's egress to the provider + MCP hosts and
**deferred it.** Rationale, preserved so it isn't re-litigated:

- The kernel seal **can't** enforce it: `build_sbpl` emits `(allow network-outbound)`
  for both `NetworkPolicy::All` and `Hosts` (`sandbox.rs:684-699`) — Seatbelt filters
  by IP/remote, not hostname. A `brain_scope().network = Hosts(...)` flip is a no-op.
- Real enforcement would be **app-level** (a `reqwest` DNS resolver that refuses
  non-allowlisted hosts on the brain's provider + MCP clients).
- The value is **defense-in-depth, not closing a live hole:** the brain's only
  network code targets config-derived URLs (provider, MCP) that the model cannot
  change — there is no model-coercible outbound path in the brain. The exfil-capable
  tool (`webfetch`) already enforces egress in the hand.

It's worth doing only if/when (a) supply-chain containment matters (a compromised
dependency phoning home), or (b) in-brain network code is added later. Until then the
disk seal + the hand-level webfetch allowlist are the substantive controls.

## End state of the seals

```rust
// brain: pure compute + LLM network
SandboxScope { read_all: false, readable: [], writable: [], network: All }

// hand: per-call, from the Impulse's bounded scope
SandboxScope { read_all: <per cfg>, readable: [..granted..], writable: [codebase, jail], network: <per tool> }

// spine: unsealed (the trust anchor)
```

# Clarify / Inform Protocol

Design plan for structured knowledge exchange between LLM and human in gait sessions, with composition into catuskoti (search) and grorg (coordination).

---

## What This Is

Two symmetric event families that turn informal Q&A in gait sessions into structured, searchable, coordinatable records:

- **`clarify_*`** — LLM needs to understand the human. Epistemically: IS (facts) or OUGHT (judgment). Blocks the LLM until converged.
- **`inform_*`** — LLM conveys understanding to human. Either party can trigger it. Blocks until converged.

Both run an SCF-inspired convergence loop: cycles until `consistent: true`.

---

## Full Event Schema

### clarify family

```typescript
// LLM asks
ClarifyEvent {
  type: "clarify"
  ts: number
  items: [{
    id: uuid                          // global unique key, FK across all events
    content: string                   // the question
    why: string                       // what changes based on the answer
    kind: "is" | "ought"              // IS = needs facts, OUGHT = needs judgment
    links?: [{
      target: uuid                    // another clarify/inform id, or external ref
      relation: "depends_on" | "related_to" | "part_of"
    }]
  }]
}

// Human answers
ClarifyReplyEvent {
  type: "clarify_reply"
  ts: number
  items: [{
    id: uuid
    content: string
    reasoning?: string                // human's explanation of why — closes loop on `why`
  }]
}

// Convergence cycles — repeat until consistent: true
ClarifyConvergenceEvent {
  type: "clarify_convergence"
  ts: number
  items: [{
    id: uuid
    proposal: string                  // LLM's current understanding of the answer
    response?: string                 // human's correction or confirmation
    consistent: boolean               // true = converged, loop ends
  }]
}
```

### inform family

```typescript
// LLM conveys knowledge (triggered by human asking or LLM proactively)
InformEvent {
  type: "inform"
  ts: number
  items: [{
    id: uuid
    content: string                   // topic being explained
    context?: string                  // what prompted this
    links?: [{
      target: uuid
      relation: "depends_on" | "related_to" | "part_of"
    }]
  }]
}

// LLM explains
InformReplyEvent {
  type: "inform_reply"
  ts: number
  items: [{
    id: uuid
    content: string                   // the explanation
  }]
}

// Convergence cycles — repeat until consistent: true
InformConvergenceEvent {
  type: "inform_convergence"
  ts: number
  items: [{
    id: uuid
    proposal: string                  // human's paraphrase of what they understood
    response?: string                 // LLM's correction if needed
    consistent: boolean               // true = human understood correctly, loop ends
  }]
}
```

---

## Lifecycle

```
clarify:
  clarify             ← LLM asks (is/ought, with why)
  clarify_reply       ← human answers
  clarify_convergence [consistent: false]  ← LLM proposes, human corrects
  clarify_convergence [consistent: true]   ← converged

inform:
  inform              ← LLM initiates (or human triggers by asking)
  inform_reply        ← LLM explains
  inform_convergence  [consistent: false]  ← human paraphrases, LLM corrects
  inform_convergence  [consistent: true]   ← converged
```

The `id` UUID on each item is the FK that joins all events in a chain. A given `id` can span many events over many turns — gait's append-only JSONL preserves the full thread.

---

## IS / OUGHT as a forcing function

The `kind` field on `clarify` is not descriptive metadata — it's a validity check. If the LLM cannot classify a question as IS or OUGHT, the question is ambiguous and should be decomposed:

- IS + OUGHT collapsed into one question → split into two linked `clarify` items with `depends_on`
- The scheduler (grorg) respects the dependency: the OUGHT can't be answered until the IS is closed

This enforces precision at ask-time rather than cleanup at query-time.

---

## Symmetry

The two families are mirrors. The only semantic difference is direction of the teach-back:

| | clarify | inform |
|---|---|---|
| who asks | LLM | LLM (or triggered by human) |
| who answers | human | LLM |
| convergence proposal | LLM's understanding of human | human's understanding of LLM |
| convergence correction | human corrects LLM | LLM corrects human |
| kind field | is / ought | — |
| links | yes | yes |

---

## Composition with Catuskoti

Each `clarify` or `inform` item maps to a **Catuskoti Concept**:

- URI: `session/{session-id}/clarify/{item-uuid}` or `session/{session-id}/inform/{item-uuid}`
- Content chunks: each event in the chain (`reply`, `convergence` cycles) is a Content node
- Context edges derived from fields:
  - `kind → is` or `kind → ought` (clarify only)
  - `why → <text>` (indexed for semantic search)
  - `status → open | resolved`
  - `session → {session-id}`
  - `links[].relation → {target-uuid}` (cross-concept connections)

**Query patterns:**
- "All unresolved OUGHT questions across all sessions" → Context filter `kind=ought, status=open`
- "What did we decide about auth?" → semantic search over `why` + `content` fields
- "Show me the full reasoning chain for q-uuid" → fetch Concept `session/x/clarify/q-uuid`, expand all Content chunks in order

Ingestion trigger: git post-commit hook on the session branch. Gait adds no custom hook machinery — it already commits the transcript at every turn boundary. The catuskoti bridge is a standalone tool that watches session commits, diffs the JSONL (append-only, so new lines = new events), and ingests. Delta-sync via simhash means re-ingestion is idempotent — only changed content re-embeds.

---

## Composition with Grorg

Each `clarify` item maps to a **Grorg Question Action**:

```
Action {
  id: derived from clarify item uuid
  name: clarify.content
  status: PLANNED → IN_PROGRESS → DONE
  metadata: {
    rationale: clarify.why,
    tags: [clarify.kind],
    external_refs: {
      gait_session: session-id,
      gait_clarify_id: item-uuid
    }
  }
}
```

- `clarify` emitted → Action created (PLANNED)
- `clarify_reply` emitted → Action → IN_PROGRESS
- `clarify_convergence [consistent: true]` → Action → DONE, DependsOn unblocks

`links[relation: "depends_on"]` → `DependsOn` edge between Question Actions in grorg. Blocked work stops automatically via CPM — no new mechanism needed.

`inform` items are symmetric with `clarify` — they also create grorg Actions. Same lifecycle: PLANNED → IN_PROGRESS → DONE when `inform_convergence [consistent: true]` fires. `links[relation: "depends_on"]` between inform items maps to grorg `DependsOn` edges, enabling lesson-plan style graphs where understanding X must precede understanding Y.

Grorg bridge: same pattern — git post-commit on session branch, reads new clarify/inform events, syncs Actions. Gait ships zero grorg or catuskoti code.

The join key across all three systems: the `id` UUID on each item.

---

## Zoom-out: Composition Check

```
gait (session capture)
  transcript.jsonl — append-only JSONL, committed at every turn boundary
  clarify/inform events — structured Q&A with convergence chains
  everything else — tool calls, text, permissions (unchanged)
  NO catuskoti code. NO grorg code. Just schema + git.

git post-commit (session branch)  ← the event bus. zero gait changes needed.
  → catuskoti-gait-bridge (separate tool)
      reads new JSONL lines since last commit
      ingests into catuskoti (idempotent via simhash)
  → grorg-gait-bridge (lives in grorg-session-bridge crate)
      reads new clarify/inform events
      syncs Question/Information Actions in grorg

catuskoti (search layer)
  Concept per clarify/inform item
  Content chunks = convergence cycles (full chain of thought)
  Context edges = kind, why, status, links
  Query: semantic search + structural filter

grorg (coordination layer)
  Question Actions for clarify items (blocking, CPM-scheduled)
  Information Actions for inform items (same lifecycle)
  fleet.sessions → gait session-id FK
```

**What each system owns:**
- gait: the content, the append-only truth, the schema contract
- git: the event bus (free, already there)
- catuskoti: the search index, the retrieval lens
- grorg: the coordination graph, what's blocking what

**What gait ships for integration: nothing.** The bridges are external tools consuming a stable schema over git. The item UUID is the shared key across all three systems. No separate catuskoti ID needed — catuskoti uses the gait session URI as its Concept identifier.

The only thing gait builds is the six event types and the two tools. Everything else composes for free.

---

## Gait Implementation

Replaces the existing `questions` / `questions_reply` tools entirely. Clean break — old sessions keep old events, new sessions use new events.

### 1. Transcript schema (`gait-rs/src/session/transcript.rs`) ✅ done
- ~~Remove `QuestionsEvent`, `QuestionsReplyEvent`~~
- ~~Add `ClarifyEvent`, `ClarifyReplyEvent`, `ClarifyConvergenceEvent`~~ — implemented
- ~~Add `InformEvent`, `InformReplyEvent`, `InformConvergenceEvent`~~ — implemented
- ~~Update `AnyEvent` discriminated union~~ — implemented

### 2. Tools ✅ done (`15067a1`)
- ~~Add `gait-rs/src/tools/clarify.rs`~~ — intercepts in turn.rs, emits Clarify event, exits turn
- ~~Add `gait-rs/src/tools/inform.rs`~~ — intercepts in turn.rs, emits Inform event, exits turn
- ~~Update `gait-rs/src/tools/mod.rs`~~ — registered; clarify/inform added to permission exemption

### 3. Message reconstruction (`gait-rs/src/session/context.rs`) ✅ done
- ~~Add clarify → assistant message, clarify_reply → user message, clarify_convergence → both~~ — implemented
- ~~Same pattern for inform_*~~ — implemented

### 4. Turn loop (`gait-rs/src/turn.rs`) ✅ done (`15067a1`)
- ~~`TurnResult::Clarify` / `TurnResult::Inform` variants added~~
- ~~Step loop intercepts clarify/inform calls, emits events, exits turn~~

### 5. Chat REPL (`gait-rs/src/chat.rs`) ✅ done (`15067a1`)
- ~~Handle `ClarifyNeeded` / `InformNeeded` from renderer — prompt user, write reply events, re-spawn~~

### 6. Integration bridges (separate repos/PRs, not in gait)
- `catuskoti-gait-bridge` — git post-commit watcher, ingests new transcript events
- `grorg-session-bridge` — git post-commit watcher, syncs clarify/inform → grorg Actions
- Gait ships nothing for this. The schema is the contract.

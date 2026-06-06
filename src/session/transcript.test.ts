import { describe, it, expect, beforeEach, afterEach } from "bun:test"
import fs from "fs"
import os from "os"
import path from "path"
import {
  parseEvent,
  appendEvent, readEvents, lastEvent, countTurnComplete, lastCompactionIndex,
  SessionStartEvent,
  UserTurnEvent,
  AssistantTextEvent,
  ToolCallEvent,
  ToolResultEvent,
  CommandResultEvent,
  ErrorEvent,
  PermissionsEvent,
  PermissionsReplyEvent,
  TurnCompleteEvent,
  CompactionEvent,
  ClarifyEvent,
  ClarifyReplyEvent,
  ClarifyConvergenceEvent,
  InformEvent,
  InformReplyEvent,
  InformConvergenceEvent,
  AgentUpdateEvent,
  type AnyEvent,
} from "./transcript"

let tmpDir: string
let txPath: string

beforeEach(() => {
  tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), "gait-tx-"))
  txPath = path.join(tmpDir, "transcript.jsonl")
})

afterEach(() => {
  fs.rmSync(tmpDir, { recursive: true })
})

describe("JSONL event schemas", () => {
  it("parses a valid session_start event", () => {
    const raw = { type: "session_start", schema_version: 1, id: "s1", model: "anthropic/claude-sonnet-4-6", codebase_hash: "abc123", codebase_dirty: false, ts: 1234 }
    const event = parseEvent(raw)
    expect(event.type).toBe("session_start")
  })

  it("parses a permissions_reply with instruct decision", () => {
    const raw = { type: "permissions_reply", from: "user", items: [{ id: "p1", decision: "instruct", text: "use --no-cache" }], ts: 1234 }
    const event = parseEvent(raw) as PermissionsReplyEvent
    expect(event.items[0]!.decision).toBe("instruct")
    expect(event.items[0]!.text).toBe("use --no-cache")
  })

  it("rejects unknown event type", () => {
    expect(() => parseEvent({ type: "unknown_event", ts: 1234 })).toThrow()
  })

  it("rejects permissions_reply with invalid decision", () => {
    const raw = { type: "permissions_reply", from: "user", items: [{ id: "p1", decision: "maybe" }], ts: 1234 }
    expect(() => parseEvent(raw)).toThrow()
  })

  it("parses compaction event with usage", () => {
    const raw = { type: "compaction", summary: "did stuff", turns_covered: 3, from_ts: 100, to_ts: 200, usage: { input: 1000, output: 500 }, ts: 300 }
    const event = parseEvent(raw) as CompactionEvent
    expect(event.usage.input).toBe(1000)
  })

  it("parses clarify event with is kind", () => {
    const raw = { type: "clarify", items: [{ id: "c1", content: "What is the db?", why: "need it for schema", kind: "is" }], ts: 1 }
    const event = parseEvent(raw) as ClarifyEvent
    expect(event.items[0]!.kind).toBe("is")
    expect(event.items[0]!.why).toBe("need it for schema")
  })

  it("parses clarify event with ought kind", () => {
    const raw = { type: "clarify", items: [{ id: "c1", content: "postgres or sqlite?", why: "schema strategy", kind: "ought" }], ts: 1 }
    const event = parseEvent(raw) as ClarifyEvent
    expect(event.items[0]!.kind).toBe("ought")
  })

  it("parses clarify event with links", () => {
    const raw = {
      type: "clarify",
      items: [{ id: "c2", content: "which db?", why: "schema", kind: "ought", links: [{ target: "c1", relation: "depends_on" }] }],
      ts: 1,
    }
    const event = parseEvent(raw) as ClarifyEvent
    expect(event.items[0]!.links![0]!.relation).toBe("depends_on")
  })

  it("rejects clarify event with invalid kind", () => {
    const raw = { type: "clarify", items: [{ id: "c1", content: "?", why: "x", kind: "maybe" }], ts: 1 }
    expect(() => parseEvent(raw)).toThrow()
  })

  it("parses clarify_reply with optional reasoning", () => {
    const raw = { type: "clarify_reply", items: [{ id: "c1", content: "postgres", reasoning: "team knows it" }], ts: 1 }
    const event = parseEvent(raw) as ClarifyReplyEvent
    expect(event.items[0]!.reasoning).toBe("team knows it")
  })

  it("parses clarify_reply without reasoning", () => {
    const raw = { type: "clarify_reply", items: [{ id: "c1", content: "postgres" }], ts: 1 }
    const event = parseEvent(raw) as ClarifyReplyEvent
    expect(event.items[0]!.reasoning).toBeUndefined()
  })

  it("parses clarify_convergence with consistent true", () => {
    const raw = { type: "clarify_convergence", items: [{ id: "c1", proposal: "use postgres with optimistic locking", response: "yes", consistent: true }], ts: 1 }
    const event = parseEvent(raw) as ClarifyConvergenceEvent
    expect(event.items[0]!.consistent).toBe(true)
  })

  it("parses clarify_convergence with consistent false and no response yet", () => {
    const raw = { type: "clarify_convergence", items: [{ id: "c1", proposal: "use postgres with row locking", consistent: false }], ts: 1 }
    const event = parseEvent(raw) as ClarifyConvergenceEvent
    expect(event.items[0]!.consistent).toBe(false)
    expect(event.items[0]!.response).toBeUndefined()
  })

  it("parses inform event with optional context", () => {
    const raw = { type: "inform", items: [{ id: "i1", content: "How HNSW works", context: "you asked about vector search" }], ts: 1 }
    const event = parseEvent(raw) as InformEvent
    expect(event.items[0]!.content).toBe("How HNSW works")
    expect(event.items[0]!.context).toBe("you asked about vector search")
  })

  it("parses inform event without context", () => {
    const raw = { type: "inform", items: [{ id: "i1", content: "How HNSW works" }], ts: 1 }
    const event = parseEvent(raw) as InformEvent
    expect(event.items[0]!.context).toBeUndefined()
  })

  it("parses inform_reply event", () => {
    const raw = { type: "inform_reply", items: [{ id: "i1", content: "HNSW is a graph-based approximate nearest neighbour algorithm..." }], ts: 1 }
    const event = parseEvent(raw) as InformReplyEvent
    expect(event.items[0]!.id).toBe("i1")
  })

  it("parses inform_convergence with consistent true", () => {
    const raw = { type: "inform_convergence", items: [{ id: "i1", proposal: "HNSW builds a layered graph for fast approximate search", response: "correct", consistent: true }], ts: 1 }
    const event = parseEvent(raw) as InformConvergenceEvent
    expect(event.items[0]!.consistent).toBe(true)
  })

  it("parses inform_convergence with no response yet", () => {
    const raw = { type: "inform_convergence", items: [{ id: "i1", proposal: "HNSW is a tree?", consistent: false }], ts: 1 }
    const event = parseEvent(raw) as InformConvergenceEvent
    expect(event.items[0]!.consistent).toBe(false)
    expect(event.items[0]!.response).toBeUndefined()
  })

  it("parses agent_update event with status done", () => {
    const raw = { type: "agent_update", session_id: "s-child", status: "done", summary: "task complete", ts: 1 }
    const event = parseEvent(raw) as AgentUpdateEvent
    expect(event.type).toBe("agent_update")
    expect(event.session_id).toBe("s-child")
    expect(event.status).toBe("done")
    expect(event.summary).toBe("task complete")
  })

  it("parses agent_update event with items when status is clarify", () => {
    const raw = { type: "agent_update", session_id: "s-child", status: "clarify", summary: "need more info", items: [{ id: "q1", content: "which db?" }], ts: 1 }
    const event = parseEvent(raw) as AgentUpdateEvent
    expect(event.status).toBe("clarify")
    expect(event.items).toEqual([{ id: "q1", content: "which db?" }])
  })

  it("rejects agent_update summary longer than 500 chars", () => {
    const raw = { type: "agent_update", session_id: "s-child", status: "done", summary: "x".repeat(501), ts: 1 }
    expect(() => parseEvent(raw)).toThrow()
  })

  it("parses clarify item with to field", () => {
    const raw = { type: "clarify", items: [{ id: "c1", content: "which db?", why: "need it", kind: "is", to: "human:alice" }], ts: 1 }
    const event = parseEvent(raw) as ClarifyEvent
    expect(event.items[0]!.to).toBe("human:alice")
  })

  it("clarify item parses without to field (to is optional)", () => {
    const raw = { type: "clarify", items: [{ id: "c1", content: "which db?", why: "need it", kind: "is" }], ts: 1 }
    const event = parseEvent(raw) as ClarifyEvent
    expect(event.items[0]!.to).toBeUndefined()
  })

  it("parses inform item with to field routing to an agent", () => {
    const raw = { type: "inform", items: [{ id: "i1", content: "schema info", to: "agent:s-parent" }], ts: 1 }
    const event = parseEvent(raw) as InformEvent
    expect(event.items[0]!.to).toBe("agent:s-parent")
  })
})

describe("appendEvent / readEvents roundtrip", () => {
  it("reads back what was written", () => {
    appendEvent(txPath, { type: "user_turn", content: "hello", ts: 1 })
    appendEvent(txPath, { type: "assistant_text", content: "world", ts: 2 })
    const events = readEvents(txPath)
    expect(events).toHaveLength(2)
    expect(events[0]!.type).toBe("user_turn")
    expect(events[1]!.type).toBe("assistant_text")
  })

  it("returns empty array for missing file", () => {
    expect(readEvents("/tmp/does-not-exist.jsonl")).toEqual([])
  })

  it("preserves all fields through roundtrip", () => {
    appendEvent(txPath, { type: "tool_call", id: "tc1", name: "read", input: { path: "foo.ts" }, ts: 42 })
    const events = readEvents(txPath)
    const e = events[0] as ToolCallEvent
    expect(e.id).toBe("tc1")
    expect(e.name).toBe("read")
    expect(e.input).toEqual({ path: "foo.ts" })
    expect(e.ts).toBe(42)
  })
})

describe("lastEvent", () => {
  it("returns null for empty/missing transcript", () => {
    expect(lastEvent(txPath)).toBeNull()
  })

  it("returns the last appended event", () => {
    appendEvent(txPath, { type: "user_turn", content: "first", ts: 1 })
    appendEvent(txPath, { type: "assistant_text", content: "second", ts: 2 })
    appendEvent(txPath, { type: "turn_complete", usage: { input: 10, output: 5, cache_read: 0, cache_write: 0 }, codebase_hash: "abc", codebase_dirty: false, ts: 3 })
    expect(lastEvent(txPath)?.type).toBe("turn_complete")
  })
})

describe("countTurnComplete", () => {
  it("returns 0 with no turn_complete events", () => {
    const events: AnyEvent[] = [
      { type: "user_turn", content: "hi", ts: 1 },
      { type: "assistant_text", content: "hello", ts: 2 },
    ]
    expect(countTurnComplete(events)).toBe(0)
  })

  it("counts multiple turn_complete events", () => {
    const tc: AnyEvent = { type: "turn_complete", usage: { input: 1, output: 1, cache_read: 0, cache_write: 0 }, codebase_hash: "h", codebase_dirty: false, ts: 1 }
    expect(countTurnComplete([tc, tc, tc])).toBe(3)
  })
})

describe("lastCompactionIndex", () => {
  it("returns -1 when no compaction event", () => {
    const events: AnyEvent[] = [{ type: "user_turn", content: "hi", ts: 1 }]
    expect(lastCompactionIndex(events)).toBe(-1)
  })

  it("returns index of last compaction", () => {
    const compaction: AnyEvent = { type: "compaction", summary: "s", turns_covered: 1, from_ts: 0, to_ts: 1, usage: { input: 100, output: 50 }, ts: 2 }
    const events: AnyEvent[] = [
      { type: "user_turn", content: "old", ts: 0 },
      compaction,
      { type: "user_turn", content: "new", ts: 3 },
    ]
    expect(lastCompactionIndex(events)).toBe(1)
  })

  it("returns the LAST compaction when multiple exist", () => {
    const c1: AnyEvent = { type: "compaction", summary: "first", turns_covered: 1, from_ts: 0, to_ts: 1, usage: { input: 10, output: 5 }, ts: 1 }
    const c2: AnyEvent = { type: "compaction", summary: "second", turns_covered: 2, from_ts: 1, to_ts: 2, usage: { input: 20, output: 10 }, ts: 2 }
    expect(lastCompactionIndex([c1, { type: "user_turn", content: "x", ts: 99 }, c2])).toBe(2)
  })
})

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
  QuestionsEvent,
  QuestionsReplyEvent,
  PermissionsEvent,
  PermissionsReplyEvent,
  SubagentSpawnedEvent,
  SubagentCompleteEvent,
  TurnCompleteEvent,
  CompactionEvent,
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

  it("parses subagent_complete with result", () => {
    const raw = { type: "subagent_complete", session_id: "s1-sub-abc", status: "done", result: "auth module extracted", ts: 1234 }
    const event = parseEvent(raw) as SubagentCompleteEvent
    expect(event.result).toBe("auth module extracted")
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

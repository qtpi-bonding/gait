import { describe, it, expect } from "bun:test"
import {
  parseEvent,
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

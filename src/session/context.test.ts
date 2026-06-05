import { describe, it, expect, beforeEach, afterEach } from "bun:test"
import fs from "fs"
import os from "os"
import path from "path"
import { reconstructMessages } from "./context"
import { appendEvent } from "./transcript"

let tmpDir: string
let transcriptPath: string

beforeEach(() => {
  tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), "gait-test-"))
  transcriptPath = path.join(tmpDir, "transcript.jsonl")
})

afterEach(() => {
  fs.rmSync(tmpDir, { recursive: true })
})

describe("reconstructMessages", () => {
  it("returns empty array for empty transcript", () => {
    fs.writeFileSync(transcriptPath, "")
    expect(reconstructMessages(transcriptPath)).toEqual([])
  })

  it("maps user_turn to user message", () => {
    appendEvent(transcriptPath, { type: "session_start", schema_version: 1, id: "s1", model: "m", codebase_hash: "h", codebase_dirty: false, ts: 1 })
    appendEvent(transcriptPath, { type: "user_turn", content: "hello", ts: 2 })
    const msgs = reconstructMessages(transcriptPath)
    expect(msgs).toEqual([{ role: "user", content: "hello" }])
  })

  it("maps assistant_text to assistant message", () => {
    appendEvent(transcriptPath, { type: "assistant_text", content: "world", ts: 1 })
    const msgs = reconstructMessages(transcriptPath)
    expect(msgs).toEqual([{ role: "assistant", content: "world" }])
  })

  it("pairs tool_call and tool_result by id", () => {
    appendEvent(transcriptPath, { type: "tool_call", id: "tc1", name: "read", input: { filePath: "foo.ts" }, ts: 1 })
    appendEvent(transcriptPath, { type: "tool_result", id: "tc1", output: "file content", ts: 2 })
    const msgs = reconstructMessages(transcriptPath)
    expect(msgs).toHaveLength(2)
    expect(msgs[0]!.role).toBe("assistant")
    expect(msgs[1]!.role).toBe("tool")
  })

  it("uses compaction as breakpoint — ignores events before it", () => {
    appendEvent(transcriptPath, { type: "user_turn", content: "old message", ts: 1 })
    appendEvent(transcriptPath, { type: "compaction", summary: "the summary", turns_covered: 1, from_ts: 0, to_ts: 100, usage: { input: 100, output: 50 }, ts: 200 })
    appendEvent(transcriptPath, { type: "user_turn", content: "new message", ts: 300 })
    const msgs = reconstructMessages(transcriptPath)
    // First message should be the compaction summary injected as context
    expect(msgs[0]!.content).toContain("the summary")
    // Old user_turn should be absent
    const hasOldMessage = msgs.some(m => typeof m.content === "string" && m.content.includes("old message"))
    expect(hasOldMessage).toBe(false)
    // New user_turn should be present
    const hasNewMessage = msgs.some(m => typeof m.content === "string" && m.content.includes("new message"))
    expect(hasNewMessage).toBe(true)
  })

  it("maps error event to user message", () => {
    appendEvent(transcriptPath, { type: "error", message: "LLM timed out", phase: "llm", ts: 1 })
    const msgs = reconstructMessages(transcriptPath)
    expect(msgs[0]!.role).toBe("user")
    expect(typeof msgs[0]!.content === "string" && msgs[0]!.content).toContain("error during llm")
  })

  it("groups adjacent tool_call+tool_result pairs into one assistant message", () => {
    appendEvent(transcriptPath, { type: "tool_call", id: "tc1", name: "read", input: {}, ts: 1 })
    appendEvent(transcriptPath, { type: "tool_call", id: "tc2", name: "glob", input: {}, ts: 2 })
    appendEvent(transcriptPath, { type: "tool_result", id: "tc1", output: "r1", ts: 3 })
    appendEvent(transcriptPath, { type: "tool_result", id: "tc2", output: "r2", ts: 4 })
    const msgs = reconstructMessages(transcriptPath)
    expect(msgs).toHaveLength(2)
    expect(Array.isArray(msgs[0]!.content)).toBe(true)
    expect(Array.isArray(msgs[1]!.content)).toBe(true)
  })

  it("reconstructs a full two-turn conversation correctly", () => {
    // Turn 1
    appendEvent(transcriptPath, { type: "user_turn", content: "list files", ts: 1 })
    appendEvent(transcriptPath, { type: "tool_call", id: "tc1", name: "ls", input: { path: "." }, ts: 2 })
    appendEvent(transcriptPath, { type: "tool_result", id: "tc1", output: "foo.ts\nbar.ts", ts: 3 })
    appendEvent(transcriptPath, { type: "assistant_text", content: "I see two files.", ts: 4 })
    appendEvent(transcriptPath, { type: "turn_complete", usage: { input: 100, output: 20, cache_read: 0, cache_write: 0 }, codebase_hash: "h", codebase_dirty: false, ts: 5 })
    // Turn 2
    appendEvent(transcriptPath, { type: "user_turn", content: "read foo.ts", ts: 6 })
    appendEvent(transcriptPath, { type: "assistant_text", content: "Here is foo.ts.", ts: 7 })

    const msgs = reconstructMessages(transcriptPath)
    // user_turn, assistant(tool-call), tool(result), assistant_text, user_turn, assistant_text
    // turn_complete is skipped
    expect(msgs).toHaveLength(6)
    expect(msgs[0]).toEqual({ role: "user", content: "list files" })
    expect(msgs[1]!.role).toBe("assistant") // tool-call step
    expect(msgs[2]!.role).toBe("tool")
    expect(msgs[3]).toEqual({ role: "assistant", content: "I see two files." })
    expect(msgs[4]).toEqual({ role: "user", content: "read foo.ts" })
    expect(msgs[5]).toEqual({ role: "assistant", content: "Here is foo.ts." })
  })

  it("reconstructs permissions → permissions_reply flow as message pair", () => {
    appendEvent(transcriptPath, { type: "user_turn", content: "run tests", ts: 1 })
    appendEvent(transcriptPath, { type: "permissions", items: [{ id: "p1", cmd: "shell(bun test)", reason: "run test suite" }], ts: 2 })
    appendEvent(transcriptPath, { type: "permissions_reply", from: "user", items: [{ id: "p1", decision: "approve" }], ts: 3 })
    appendEvent(transcriptPath, { type: "assistant_text", content: "Tests passed.", ts: 4 })

    const msgs = reconstructMessages(transcriptPath)
    expect(msgs).toHaveLength(4)
    expect(msgs[1]!.role).toBe("assistant")
    expect(typeof msgs[1]!.content === "string" && msgs[1]!.content).toContain("permission request")
    expect(msgs[2]!.role).toBe("user")
    expect(typeof msgs[2]!.content === "string" && msgs[2]!.content).toContain("approve")
    expect(msgs[3]).toEqual({ role: "assistant", content: "Tests passed." })
  })

  it("reconstructs questions → questions_reply flow", () => {
    appendEvent(transcriptPath, { type: "user_turn", content: "do the thing", ts: 1 })
    appendEvent(transcriptPath, { type: "questions", items: [{ id: "q1", content: "Which env?" }], ts: 2 })
    appendEvent(transcriptPath, { type: "questions_reply", from: "user", items: [{ id: "q1", content: "production" }], ts: 3 })
    appendEvent(transcriptPath, { type: "user_turn", content: "continue", ts: 4 })

    const msgs = reconstructMessages(transcriptPath)
    expect(msgs).toHaveLength(4)
    expect(msgs[1]!.role).toBe("assistant") // questions shown as assistant
    expect(msgs[2]!.role).toBe("user")      // reply shown as user
    expect(typeof msgs[2]!.content === "string" && msgs[2]!.content).toContain("production")
  })
})

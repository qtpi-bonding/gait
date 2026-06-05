import { describe, it, expect, beforeEach, afterEach } from "bun:test"
import fs from "fs"
import os from "os"
import path from "path"
import { reconstructMessages } from "./context"
import { appendEvent } from "./transcript"

let tmpDir: string
let transcriptPath: string

beforeEach(() => {
  tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), "ainix-test-"))
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
    // One assistant message with two tool-calls, one tool message with two results
    expect(msgs).toHaveLength(2)
    expect(Array.isArray(msgs[0]!.content)).toBe(true)
    expect(Array.isArray(msgs[1]!.content)).toBe(true)
  })
})

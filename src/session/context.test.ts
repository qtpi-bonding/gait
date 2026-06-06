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

  it("reconstructs clarify → clarify_reply → clarify_convergence flow", () => {
    appendEvent(transcriptPath, { type: "user_turn", content: "build the schema", ts: 1 })
    appendEvent(transcriptPath, { type: "clarify", items: [{ id: "c1", content: "postgres or sqlite?", why: "determines schema strategy", kind: "ought" }], ts: 2 })
    appendEvent(transcriptPath, { type: "clarify_reply", items: [{ id: "c1", content: "postgres", reasoning: "team knows it" }], ts: 3 })
    appendEvent(transcriptPath, { type: "clarify_convergence", items: [{ id: "c1", proposal: "I'll design for postgres", response: "yes", consistent: true }], ts: 4 })

    const msgs = reconstructMessages(transcriptPath)
    // user_turn, clarify(assistant), clarify_reply(user), convergence proposal(assistant), convergence response(user)
    expect(msgs).toHaveLength(5)
    expect(msgs[1]!.role).toBe("assistant")
    expect(typeof msgs[1]!.content === "string" && msgs[1]!.content).toContain("postgres or sqlite")
    expect(typeof msgs[1]!.content === "string" && msgs[1]!.content).toContain("ought")
    expect(msgs[2]!.role).toBe("user")
    expect(typeof msgs[2]!.content === "string" && msgs[2]!.content).toContain("postgres")
    expect(msgs[3]!.role).toBe("assistant")
    expect(typeof msgs[3]!.content === "string" && msgs[3]!.content).toContain("I'll design for postgres")
    expect(msgs[4]!.role).toBe("user")
    expect(typeof msgs[4]!.content === "string" && msgs[4]!.content).toContain("yes")
  })

  it("reconstructs clarify_convergence without response as single assistant message", () => {
    appendEvent(transcriptPath, { type: "clarify_convergence", items: [{ id: "c1", proposal: "use postgres", consistent: false }], ts: 1 })
    const msgs = reconstructMessages(transcriptPath)
    expect(msgs).toHaveLength(1)
    expect(msgs[0]!.role).toBe("assistant")
    expect(typeof msgs[0]!.content === "string" && msgs[0]!.content).toContain("use postgres")
  })

  it("reconstructs inform → inform_reply → inform_convergence flow", () => {
    appendEvent(transcriptPath, { type: "user_turn", content: "what is HNSW?", ts: 1 })
    appendEvent(transcriptPath, { type: "inform", items: [{ id: "i1", content: "How HNSW indexing works" }], ts: 2 })
    appendEvent(transcriptPath, { type: "inform_reply", items: [{ id: "i1", content: "HNSW is a graph-based ANN algorithm..." }], ts: 3 })
    appendEvent(transcriptPath, { type: "inform_convergence", items: [{ id: "i1", proposal: "HNSW builds a graph for fast approximate search", response: "correct", consistent: true }], ts: 4 })

    const msgs = reconstructMessages(transcriptPath)
    // user_turn, inform(assistant), inform_reply(assistant), convergence proposal(user), convergence response(assistant)
    expect(msgs).toHaveLength(5)
    expect(msgs[1]!.role).toBe("assistant")
    expect(typeof msgs[1]!.content === "string" && msgs[1]!.content).toContain("HNSW indexing")
    expect(msgs[2]!.role).toBe("assistant")
    expect(typeof msgs[2]!.content === "string" && msgs[2]!.content).toContain("graph-based ANN")
    expect(msgs[3]!.role).toBe("user")
    expect(typeof msgs[3]!.content === "string" && msgs[3]!.content).toContain("HNSW builds a graph")
    expect(msgs[4]!.role).toBe("assistant")
    expect(typeof msgs[4]!.content === "string" && msgs[4]!.content).toContain("correct")
  })

  it("reconstructs inform_convergence without response as single user message", () => {
    appendEvent(transcriptPath, { type: "inform_convergence", items: [{ id: "i1", proposal: "HNSW is a tree?", consistent: false }], ts: 1 })
    const msgs = reconstructMessages(transcriptPath)
    expect(msgs).toHaveLength(1)
    expect(msgs[0]!.role).toBe("user")
  })

  it("reconstructs multiple iterations of clarify_convergence", () => {
    appendEvent(transcriptPath, { type: "clarify", items: [{ id: "c1", content: "which db?", why: "schema", kind: "ought" }], ts: 1 })
    appendEvent(transcriptPath, { type: "clarify_reply", items: [{ id: "c1", content: "postgres" }], ts: 2 })
    appendEvent(transcriptPath, { type: "clarify_convergence", items: [{ id: "c1", proposal: "use postgres with row locking", response: "no, optimistic locking", consistent: false }], ts: 3 })
    appendEvent(transcriptPath, { type: "clarify_convergence", items: [{ id: "c1", proposal: "use postgres with optimistic locking", response: "yes", consistent: true }], ts: 4 })

    const msgs = reconstructMessages(transcriptPath)
    // clarify(asst), clarify_reply(user), convergence1 proposal(asst) + response(user), convergence2 proposal(asst) + response(user)
    expect(msgs).toHaveLength(6)
    const contents = msgs.map(m => typeof m.content === "string" ? m.content : "")
    expect(contents.some(c => c.includes("row locking"))).toBe(true)
    expect(contents.some(c => c.includes("optimistic locking"))).toBe(true)
  })

  it("maps agent_update done event to user message", () => {
    appendEvent(transcriptPath, { type: "agent_update", session_id: "s-child", status: "done", summary: "task finished", ts: 1 } as any)
    const msgs = reconstructMessages(transcriptPath)
    expect(msgs).toHaveLength(1)
    expect(msgs[0]!.role).toBe("user")
    expect(typeof msgs[0]!.content === "string" && msgs[0]!.content).toContain("s-child")
    expect(typeof msgs[0]!.content === "string" && msgs[0]!.content).toContain("done")
  })

  it("maps agent_update clarify event includes items in content", () => {
    appendEvent(transcriptPath, { type: "agent_update", session_id: "s-child", status: "clarify", summary: "needs input", items: [{ id: "q1", content: "which db?" }], ts: 1 } as any)
    const msgs = reconstructMessages(transcriptPath)
    expect(msgs).toHaveLength(1)
    expect(msgs[0]!.role).toBe("user")
    expect(typeof msgs[0]!.content === "string" && msgs[0]!.content).toContain("items")
  })

  it("reconstructs multiple permissions with mixed decisions in reply", () => {
    appendEvent(transcriptPath, { type: "user_turn", content: "run everything", ts: 1 })
    appendEvent(transcriptPath, {
      type: "permissions",
      items: [
        { id: "p1", cmd: "shell(bun test)", reason: "run tests" },
        { id: "p2", cmd: "shell(rm -rf dist)", reason: "clean build" },
        { id: "p3", cmd: "write(config.json)", reason: "update config" },
      ],
      ts: 2,
    })
    appendEvent(transcriptPath, {
      type: "permissions_reply",
      from: "user",
      items: [
        { id: "p1", decision: "approve" },
        { id: "p2", decision: "deny" },
        { id: "p3", decision: "approve" },
      ],
      ts: 3,
    })

    const msgs = reconstructMessages(transcriptPath)
    expect(msgs).toHaveLength(3) // user_turn, assistant(permissions), user(reply)
    const requestContent = msgs[1]!.content as string
    expect(requestContent).toContain("bun test")
    expect(requestContent).toContain("rm -rf dist")
    expect(requestContent).toContain("write(config.json)")
    const replyContent = msgs[2]!.content as string
    expect(replyContent).toContain("approve")
    expect(replyContent).toContain("deny")
  })
})

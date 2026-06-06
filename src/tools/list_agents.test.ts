import { describe, it, expect, beforeEach, afterEach } from "bun:test"
import fs from "fs"
import os from "os"
import path from "path"
import { execSync } from "child_process"
import { createListAgentsTool } from "./list_agents"

let tmpDir: string

function initWorkspace(dir: string): void {
  execSync("git init", { cwd: dir })
  execSync("git config user.email 'test@test.com'", { cwd: dir })
  execSync("git config user.name 'Test'", { cwd: dir })
  fs.writeFileSync(path.join(dir, "gait.jsonc"), "{}")
  execSync("git add gait.jsonc && git commit -m 'init'", { cwd: dir, shell: "/bin/sh" })
}

function createSession(workspacePath: string, sessionId: string, lastEventType: string): void {
  execSync(
    `git worktree add -b session/${sessionId} sessions/${sessionId} HEAD`,
    { cwd: workspacePath }
  )
  const sessionDir = path.join(workspacePath, "sessions", sessionId)
  const txPath = path.join(sessionDir, "transcript.jsonl")

  const startEvent = JSON.stringify({ type: "session_start", schema_version: 1, id: sessionId, model: "m", codebase_hash: "h", codebase_dirty: false, ts: 1 })

  let lastEvent: string
  switch (lastEventType) {
    case "turn_complete":
      lastEvent = JSON.stringify({ type: "turn_complete", usage: { input: 1, output: 1, cache_read: 0, cache_write: 0 }, codebase_hash: "h", codebase_dirty: false, ts: 2 })
      break
    case "clarify":
      lastEvent = JSON.stringify({ type: "clarify", items: [], ts: 2 })
      break
    case "assistant_text":
      lastEvent = JSON.stringify({ type: "assistant_text", content: "some output", ts: 2 })
      break
    case "error":
      lastEvent = JSON.stringify({ type: "error", message: "err", phase: "llm", ts: 2 })
      break
    case "user_turn":
      lastEvent = JSON.stringify({ type: "user_turn", content: "task", ts: 2 })
      break
    default:
      lastEvent = JSON.stringify({ type: "user_turn", content: "task", ts: 2 })
  }

  fs.writeFileSync(txPath, startEvent + "\n" + lastEvent + "\n", "utf-8")
}

beforeEach(() => {
  tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), "gait-list-agents-"))
  initWorkspace(tmpDir)
})

afterEach(() => {
  try { execSync("git worktree prune", { cwd: tmpDir }) } catch {}
  fs.rmSync(tmpDir, { recursive: true, force: true })
})

describe("createListAgentsTool", () => {
  it("returns empty array when no sessions exist", async () => {
    const tool = createListAgentsTool(tmpDir)
    const result = await tool.execute!({}, {} as any)
    expect(result).toEqual([])
  })

  it("excludes done sessions (turn_complete as last event)", async () => {
    createSession(tmpDir, "s-done", "turn_complete")
    const tool = createListAgentsTool(tmpDir)
    const result = await tool.execute!({}, {} as any) as Array<{ session_id: string }>
    const ids = result.map(r => r.session_id)
    expect(ids).not.toContain("s-done")
  })

  it("includes clarify sessions with status clarify", async () => {
    createSession(tmpDir, "s-clarify", "clarify")
    const tool = createListAgentsTool(tmpDir)
    const result = await tool.execute!({}, {} as any) as Array<{ session_id: string; status: string }>
    const entry = result.find(r => r.session_id === "s-clarify")
    expect(entry).toBeDefined()
    expect(entry!.status).toBe("clarify")
  })

  it("includes running sessions (assistant_text as last event)", async () => {
    createSession(tmpDir, "s-running", "assistant_text")
    const tool = createListAgentsTool(tmpDir)
    const result = await tool.execute!({}, {} as any) as Array<{ session_id: string; status: string }>
    const entry = result.find(r => r.session_id === "s-running")
    expect(entry).toBeDefined()
    expect(entry!.status).toBe("running")
  })

  it("returns multiple active sessions", async () => {
    createSession(tmpDir, "s-a", "user_turn")
    createSession(tmpDir, "s-b", "clarify")
    const tool = createListAgentsTool(tmpDir)
    const result = await tool.execute!({}, {} as any) as Array<{ session_id: string }>
    const ids = result.map(r => r.session_id)
    expect(ids).toContain("s-a")
    expect(ids).toContain("s-b")
  })
})

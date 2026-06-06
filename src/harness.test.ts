import { describe, it, expect, beforeEach, afterEach } from "bun:test"
import fs from "fs"
import os from "os"
import path from "path"
import { execSync } from "child_process"
import { NativeHarnessBackend } from "./harness"

let tmpDir: string

function initWorkspace(dir: string): void {
  fs.mkdirSync(dir, { recursive: true })
  execSync("git init", { cwd: dir })
  execSync('git config user.email "test@example.com"', { cwd: dir })
  execSync('git config user.name "Test"', { cwd: dir })
  fs.writeFileSync(path.join(dir, "gait.jsonc"), "{}", "utf-8")
  execSync("git add gait.jsonc", { cwd: dir })
  execSync('git commit -m "init"', { cwd: dir })
}

function createLegacySession(workspacePath: string, sessionId: string): void {
  const sessionDir = path.join(workspacePath, "sessions", sessionId)
  fs.mkdirSync(sessionDir, { recursive: true })
  const event = {
    type: "session_start",
    schema_version: 1,
    id: sessionId,
    model: "test-model",
    codebase_hash: "abc",
    codebase_dirty: false,
    ts: Date.now(),
  }
  fs.appendFileSync(
    path.join(sessionDir, "transcript.jsonl"),
    JSON.stringify(event) + "\n",
    "utf-8"
  )
}

beforeEach(() => {
  tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), "gait-harness-"))
})

afterEach(() => {
  fs.rmSync(tmpDir, { recursive: true })
})

describe("NativeHarnessBackend", () => {
  it("afterTurn with no assignedBy is a no-op", async () => {
    const harness = new NativeHarnessBackend(tmpDir)
    // Should not throw and return void
    const result = await harness.afterTurn("s-child", undefined, { type: "done", summary: "finished" })
    expect(result).toBeUndefined()
  })

  it("afterTurn with assignedBy injects agent_update into assigner transcript", async () => {
    initWorkspace(tmpDir)
    createLegacySession(tmpDir, "s-parent")
    const harness = new NativeHarnessBackend(tmpDir)

    await harness.afterTurn("s-child", "s-parent", { type: "done", summary: "finished" })

    const transcriptPath = path.join(tmpDir, "sessions", "s-parent", "transcript.jsonl")
    const lines = fs.readFileSync(transcriptPath, "utf-8").split("\n").filter(Boolean)
    const events = lines.map((l) => JSON.parse(l))
    const agentUpdate = events.find((e) => e.type === "agent_update")

    expect(agentUpdate).toBeDefined()
    expect(agentUpdate.type).toBe("agent_update")
    expect(agentUpdate.session_id).toBe("s-child")
    expect(agentUpdate.status).toBe("done")
    expect(agentUpdate.summary).toBe("finished")
  })

  it("afterTurn truncates summary to 500 chars", async () => {
    initWorkspace(tmpDir)
    createLegacySession(tmpDir, "s-parent")
    const harness = new NativeHarnessBackend(tmpDir)

    await harness.afterTurn("s-child", "s-parent", { type: "done", summary: "x".repeat(600) })

    const transcriptPath = path.join(tmpDir, "sessions", "s-parent", "transcript.jsonl")
    const lines = fs.readFileSync(transcriptPath, "utf-8").split("\n").filter(Boolean)
    const events = lines.map((l) => JSON.parse(l))
    const agentUpdate = events.find((e) => e.type === "agent_update")

    expect(agentUpdate).toBeDefined()
    expect(agentUpdate.summary.length).toBe(500)
  })

  it("afterTurn with clarify result uses clarify status with items", async () => {
    initWorkspace(tmpDir)
    createLegacySession(tmpDir, "s-parent")
    const harness = new NativeHarnessBackend(tmpDir)

    await harness.afterTurn("s-child", "s-parent", {
      type: "clarify",
      items: [{ id: "q1", content: "which db?", why: "schema", kind: "ought" }],
    })

    const transcriptPath = path.join(tmpDir, "sessions", "s-parent", "transcript.jsonl")
    const lines = fs.readFileSync(transcriptPath, "utf-8").split("\n").filter(Boolean)
    const events = lines.map((l) => JSON.parse(l))
    const agentUpdate = events.find((e) => e.type === "agent_update")

    expect(agentUpdate).toBeDefined()
    expect(agentUpdate.status).toBe("clarify")
    expect(agentUpdate.items).toBeTruthy()
  })

  it("afterTurn with permissions result uses done status in agent_update", async () => {
    initWorkspace(tmpDir)
    createLegacySession(tmpDir, "s-parent")
    const harness = new NativeHarnessBackend(tmpDir)

    await harness.afterTurn("s-child", "s-parent", { type: "permissions" })

    const transcriptPath = path.join(tmpDir, "sessions", "s-parent", "transcript.jsonl")
    const lines = fs.readFileSync(transcriptPath, "utf-8").split("\n").filter(Boolean)
    const events = lines.map((l) => JSON.parse(l))
    const agentUpdate = events.find((e) => e.type === "agent_update")

    expect(agentUpdate).toBeDefined()
    expect(agentUpdate.status).toBe("done")
  })
})

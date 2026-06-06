import { describe, it, expect, beforeEach, afterEach } from "bun:test"
import fs from "fs"
import os from "os"
import path from "path"
import { execSync } from "child_process"
import { createSpawnAgentTool } from "./spawn_agent"

let tmpDir: string

function initWorkspace(dir: string): void {
  execSync("git init", { cwd: dir })
  execSync("git config user.email 'test@test.com'", { cwd: dir })
  execSync("git config user.name 'Test'", { cwd: dir })
  fs.writeFileSync(path.join(dir, "gait.jsonc"), JSON.stringify({ model: "anthropic/claude-sonnet-4-6" }, null, 2))
  execSync("git add gait.jsonc && git commit -m 'init'", { cwd: dir, shell: "/bin/sh" })
}

beforeEach(() => {
  tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), "gait-spawn-agent-"))
  initWorkspace(tmpDir)
})

afterEach(() => {
  try { execSync("git worktree prune", { cwd: tmpDir }) } catch {}
  fs.rmSync(tmpDir, { recursive: true, force: true })
})

describe("createSpawnAgentTool", () => {
  it("execute returns a session_id", async () => {
    const tool = createSpawnAgentTool("s-parent", tmpDir)
    const result = await (tool as any).execute({ task: "hello world" }, {})
    expect(result.session_id).toMatch(/^s[0-9a-f]/)
  })

  it("creates session.json with assigned_by", async () => {
    const tool = createSpawnAgentTool("s-parent", tmpDir)
    const result = await (tool as any).execute({ task: "hello world" }, {})
    const id = result.session_id
    const sjPath = path.join(tmpDir, "sessions", id, "session.json")
    expect(fs.existsSync(sjPath)).toBe(true)
    const sj = JSON.parse(fs.readFileSync(sjPath, "utf-8"))
    expect(sj.assigned_by).toBe("s-parent")
    expect(sj.id).toBe(id)
  })

  it("session.json includes agent_name when agent param provided", async () => {
    const tool = createSpawnAgentTool("s-parent", tmpDir)
    const result = await (tool as any).execute({ task: "do stuff", agent: "coding" }, {})
    const id = result.session_id
    const sjPath = path.join(tmpDir, "sessions", id, "session.json")
    const sj = JSON.parse(fs.readFileSync(sjPath, "utf-8"))
    expect(sj.agent_name).toBe("coding")
  })

  it("creates transcript with session_start and user_turn events", async () => {
    const tool = createSpawnAgentTool("s-parent", tmpDir)
    const result = await (tool as any).execute({ task: "build auth" }, {})
    const id = result.session_id
    const txPath = path.join(tmpDir, "sessions", id, "transcript.jsonl")
    expect(fs.existsSync(txPath)).toBe(true)
    const lines = fs.readFileSync(txPath, "utf-8").split("\n").filter(Boolean)
    const events = lines.map((l) => JSON.parse(l))
    const hasSessionStart = events.some((e: any) => e.type === "session_start")
    const hasUserTurn = events.some((e: any) => e.type === "user_turn" && e.content === "build auth")
    expect(hasSessionStart).toBe(true)
    expect(hasUserTurn).toBe(true)
  })

  it("creates worktree for new session", async () => {
    const tool = createSpawnAgentTool("s-parent", tmpDir)
    const result = await (tool as any).execute({ task: "build auth" }, {})
    const id = result.session_id
    const sessionDir = path.join(tmpDir, "sessions", id)
    expect(fs.existsSync(sessionDir)).toBe(true)
    const gitPath = path.join(sessionDir, ".git")
    expect(fs.existsSync(gitPath)).toBe(true)
    expect(fs.statSync(gitPath).isFile()).toBe(true)
  })
})

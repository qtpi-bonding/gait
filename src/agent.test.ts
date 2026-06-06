import { describe, it, expect, beforeEach, afterEach } from "bun:test"
import fs from "fs"
import os from "os"
import path from "path"
import { loadAgentSystemPrompt, resolveAgentPrompt } from "./agent"

let tmpDir: string

beforeEach(() => {
  tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), "gait-agent-"))
})

afterEach(() => {
  fs.rmSync(tmpDir, { recursive: true, force: true })
})

describe("loadAgentSystemPrompt", () => {
  it("reads agents/{name}.md and returns trimmed content", () => {
    fs.mkdirSync(path.join(tmpDir, "agents"))
    fs.writeFileSync(path.join(tmpDir, "agents", "coder.md"), "You are a coder.\n")
    expect(loadAgentSystemPrompt(tmpDir, "coder")).toBe("You are a coder.")
  })

  it("throws with agent name if file does not exist", () => {
    expect(() => loadAgentSystemPrompt(tmpDir, "nonexistent")).toThrow(
      'Agent "nonexistent" not found'
    )
  })
})

describe("resolveAgentPrompt", () => {
  it("returns inline systemPrompt when provided", () => {
    expect(resolveAgentPrompt(tmpDir, { systemPrompt: "inline prompt" })).toBe("inline prompt")
  })

  it("inline takes precedence over agentName", () => {
    fs.mkdirSync(path.join(tmpDir, "agents"))
    fs.writeFileSync(path.join(tmpDir, "agents", "coder.md"), "from file")
    expect(resolveAgentPrompt(tmpDir, { systemPrompt: "inline", agentName: "coder" })).toBe("inline")
  })

  it("uses agentName when no inline prompt", () => {
    fs.mkdirSync(path.join(tmpDir, "agents"))
    fs.writeFileSync(path.join(tmpDir, "agents", "coder.md"), "from file")
    expect(resolveAgentPrompt(tmpDir, { agentName: "coder" })).toBe("from file")
  })

  it("falls back to defaultAgent when agentName not set", () => {
    fs.mkdirSync(path.join(tmpDir, "agents"))
    fs.writeFileSync(path.join(tmpDir, "agents", "default.md"), "default prompt")
    expect(resolveAgentPrompt(tmpDir, { defaultAgent: "default" })).toBe("default prompt")
  })

  it("agentName takes precedence over defaultAgent", () => {
    fs.mkdirSync(path.join(tmpDir, "agents"))
    fs.writeFileSync(path.join(tmpDir, "agents", "coder.md"), "coder prompt")
    fs.writeFileSync(path.join(tmpDir, "agents", "default.md"), "default prompt")
    expect(resolveAgentPrompt(tmpDir, { agentName: "coder", defaultAgent: "default" })).toBe(
      "coder prompt"
    )
  })

  it("returns undefined when nothing is set", () => {
    expect(resolveAgentPrompt(tmpDir, {})).toBeUndefined()
  })
})

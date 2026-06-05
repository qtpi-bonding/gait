import { describe, it, expect } from "bun:test"
import fs from "fs"
import os from "os"
import path from "path"
import { buildSystemPrompt, buildTurnContext } from "./system_prompt"

describe("buildSystemPrompt", () => {
  it("contains <gait> block with session, workspace, codebase", () => {
    const result = buildSystemPrompt({ sessionId: "s1234", workspacePath: "/ws", codebasePath: "/cb" })
    expect(result).toContain("<gait>")
    expect(result).toContain("session: s1234")
    expect(result).toContain("workspace: /ws")
    expect(result).toContain("codebase: /cb")
    expect(result).toContain("</gait>")
  })

  it("does not contain timestamp or git state", () => {
    const result = buildSystemPrompt({ sessionId: "s1", workspacePath: "/ws", codebasePath: "/cb" })
    expect(result).not.toContain("timestamp")
    expect(result).not.toContain("git:")
  })

  it("omits <agents> block when AGENTS.md does not exist", () => {
    const result = buildSystemPrompt({ sessionId: "s1", workspacePath: "/nonexistent", codebasePath: "/cb" })
    expect(result).not.toContain("<agents>")
  })

  it("includes <agents> block when AGENTS.md exists", () => {
    const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "gait-sp-"))
    fs.writeFileSync(path.join(tmp, "AGENTS.md"), "You are a Rust expert.")
    const result = buildSystemPrompt({ sessionId: "s1", workspacePath: tmp, codebasePath: "/cb" })
    expect(result).toContain("<agents>")
    expect(result).toContain("You are a Rust expert.")
    expect(result).toContain("</agents>")
    fs.rmSync(tmp, { recursive: true })
  })
})

describe("buildTurnContext", () => {
  it("contains <gait-state> with timestamp and git hash", () => {
    const result = buildTurnContext({ codebaseHash: "abc123", dirtyFiles: [], timestamp: 9999 })
    expect(result).toContain("<gait-state>")
    expect(result).toContain("timestamp: 9999")
    expect(result).toContain("git: abc123 | clean")
    expect(result).toContain("</gait-state>")
  })

  it("shows clean when no dirty files", () => {
    const result = buildTurnContext({ codebaseHash: "abc", dirtyFiles: [], timestamp: 1 })
    expect(result).toContain("| clean")
    expect(result).not.toContain("| dirty")
  })

  it("shows dirty files when present", () => {
    const result = buildTurnContext({
      codebaseHash: "abc", timestamp: 1,
      dirtyFiles: [" M src/auth.ts", "?? src/new.ts"],
    })
    expect(result).toContain("| dirty")
    expect(result).toContain("M src/auth.ts")
    expect(result).toContain("? src/new.ts")
  })
})

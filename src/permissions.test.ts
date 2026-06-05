import { describe, it, expect } from "bun:test"
import { isAllowed, wrapTools } from "./permissions"
import { tool } from "ai"
import { z } from "zod"

const mockTool = tool({
  description: "test tool",
  parameters: z.object({ x: z.string() }),
  execute: async ({ x }) => `executed: ${x}`,
})

describe("isAllowed", () => {
  it("returns true when tool is in allow list", () => {
    expect(isAllowed("read", ["read", "glob"])).toBe(true)
  })

  it("returns false when tool is not in allow list", () => {
    expect(isAllowed("write", ["read", "glob"])).toBe(false)
  })

  it("returns false for empty allow list", () => {
    expect(isAllowed("read", [])).toBe(false)
  })
})

describe("wrapTools", () => {
  it("executes allowed tools immediately", async () => {
    const pending: any[] = []
    const wrapped = wrapTools({ read: mockTool }, ["read"], pending, "codebase/")
    const result = await wrapped["read"]!.execute!({ x: "hello" }, {} as any)
    expect(result).toBe("executed: hello")
    expect(pending).toHaveLength(0)
  })

  it("collects non-allowed tools into pending without executing", async () => {
    const pending: any[] = []
    const wrapped = wrapTools({ write: mockTool }, [], pending, "codebase/")
    const result = await wrapped["write"]!.execute!({ x: "hello" }, {} as any)
    expect(result).toContain("[permission required")
    expect(pending).toHaveLength(1)
    expect(pending[0].name).toBe("write")
  })
})

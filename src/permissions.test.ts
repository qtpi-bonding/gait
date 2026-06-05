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

  it("pending item includes tool name and input", async () => {
    const pending: any[] = []
    const wrapped = wrapTools({ shell: mockTool }, [], pending, "codebase/")
    await wrapped["shell"]!.execute!({ x: "rm -rf /" }, {} as any)
    expect(pending[0].name).toBe("shell")
    expect(pending[0].input).toEqual({ x: "rm -rf /" })
  })

  it("accumulates multiple non-allowed tool calls", async () => {
    const pending: any[] = []
    const tools = { write: mockTool, shell: mockTool }
    const wrapped = wrapTools(tools, [], pending, "codebase/")
    await wrapped["write"]!.execute!({ x: "a" }, {} as any)
    await wrapped["shell"]!.execute!({ x: "b" }, {} as any)
    expect(pending).toHaveLength(2)
    expect(pending.map((p) => p.name)).toEqual(["write", "shell"])
  })

  it("allows some tools and gates others from the same set", async () => {
    const pending: any[] = []
    const tools = { read: mockTool, write: mockTool, shell: mockTool }
    const wrapped = wrapTools(tools, ["read", "write"], pending, "codebase/")

    const readResult = await wrapped["read"]!.execute!({ x: "hi" }, {} as any)
    const writeResult = await wrapped["write"]!.execute!({ x: "hi" }, {} as any)
    await wrapped["shell"]!.execute!({ x: "hi" }, {} as any)

    expect(readResult).toBe("executed: hi")
    expect(writeResult).toBe("executed: hi")
    expect(pending).toHaveLength(1)
    expect(pending[0].name).toBe("shell")
  })

  it("sentinel string mentions the tool name", async () => {
    const pending: any[] = []
    const wrapped = wrapTools({ my_tool: mockTool }, [], pending, "codebase/")
    const result = await wrapped["my_tool"]!.execute!({ x: "val" }, {} as any)
    expect(typeof result).toBe("string")
    expect(result).toContain("my_tool")
  })

  it("pending item has a unique id per call", async () => {
    const pending: any[] = []
    const wrapped = wrapTools({ write: mockTool }, [], pending, "codebase/")
    await wrapped["write"]!.execute!({ x: "a" }, {} as any)
    await wrapped["write"]!.execute!({ x: "b" }, {} as any)
    expect(pending[0].id).toBeTruthy()
    expect(pending[1].id).toBeTruthy()
    expect(pending[0].id).not.toBe(pending[1].id)
  })
})

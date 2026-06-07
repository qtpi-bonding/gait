import { describe, test, expect, beforeEach } from "bun:test"
import { createOutputTool, getOutputSignal, resetOutputSignal } from "./output"

describe("output signal", () => {
  beforeEach(() => resetOutputSignal())

  test("starts null", () => {
    expect(getOutputSignal()).toBeNull()
  })

  test("reset clears signal", async () => {
    const tool = createOutputTool()
    await (tool as any).execute({ summary: "did a thing" }, {})
    resetOutputSignal()
    expect(getOutputSignal()).toBeNull()
  })
})

describe("createOutputTool execute", () => {
  beforeEach(() => resetOutputSignal())

  test("sets summary", async () => {
    const tool = createOutputTool()
    await (tool as any).execute({ summary: "added health endpoint" }, {})
    expect(getOutputSignal()?.summary).toBe("added health endpoint")
  })

  test("sets result when provided", async () => {
    const tool = createOutputTool()
    await (tool as any).execute({ summary: "done", result: { files_changed: ["a.ts"] } }, {})
    expect(getOutputSignal()?.result).toEqual({ files_changed: ["a.ts"] })
  })

  test("result is undefined when not provided", async () => {
    const tool = createOutputTool()
    await (tool as any).execute({ summary: "done" }, {})
    expect(getOutputSignal()?.result).toBeUndefined()
  })

  test("first call wins — subsequent calls ignored", async () => {
    const tool = createOutputTool()
    await (tool as any).execute({ summary: "first" }, {})
    await (tool as any).execute({ summary: "second" }, {})
    expect(getOutputSignal()?.summary).toBe("first")
  })

  test("returns 'recorded'", async () => {
    const tool = createOutputTool()
    const result = await (tool as any).execute({ summary: "done" }, {})
    expect(result).toBe("recorded")
  })
})

import { describe, it, expect, beforeEach } from "bun:test"
import {
  createInformTool, getInformSignal, resetInformSignal,
  createInformConvergenceTool, getInformConvergenceSignal, resetInformConvergenceSignal,
} from "./inform"

beforeEach(() => {
  resetInformSignal()
  resetInformConvergenceSignal()
})

describe("inform signal", () => {
  it("starts null after reset", () => {
    expect(getInformSignal()).toBeNull()
  })

  it("execute sets signal with content", async () => {
    const tool = createInformTool()
    await tool.execute!({
      items: [{ id: "i1", content: "How HNSW indexing works" }],
    }, {} as any)
    const signal = getInformSignal()
    expect(signal).not.toBeNull()
    expect(signal!.items[0]!.id).toBe("i1")
    expect(signal!.items[0]!.content).toBe("How HNSW indexing works")
  })

  it("execute sets signal with optional context", async () => {
    const tool = createInformTool()
    await tool.execute!({
      items: [{ id: "i1", content: "What is BM25", context: "you asked about search ranking" }],
    }, {} as any)
    expect(getInformSignal()!.items[0]!.context).toBe("you asked about search ranking")
  })

  it("execute sets signal with links", async () => {
    const tool = createInformTool()
    await tool.execute!({
      items: [{
        id: "i2",
        content: "Hybrid search combines BM25 and HNSW",
        links: [{ target: "i1", relation: "depends_on" }],
      }],
    }, {} as any)
    expect(getInformSignal()!.items[0]!.links![0]!.target).toBe("i1")
  })

  it("execute sets signal with multiple items", async () => {
    const tool = createInformTool()
    await tool.execute!({
      items: [
        { id: "i1", content: "What is BM25" },
        { id: "i2", content: "What is HNSW" },
      ],
    }, {} as any)
    expect(getInformSignal()!.items).toHaveLength(2)
  })

  it("resetInformSignal clears after set", async () => {
    const tool = createInformTool()
    await tool.execute!({ items: [{ id: "i1", content: "x" }] }, {} as any)
    expect(getInformSignal()).not.toBeNull()
    resetInformSignal()
    expect(getInformSignal()).toBeNull()
  })

  it("execute returns a string indicating turn will end", async () => {
    const tool = createInformTool()
    const result = await tool.execute!({ items: [{ id: "i1", content: "x" }] }, {} as any)
    expect(typeof result).toBe("string")
    expect(result).toBeTruthy()
  })
})

describe("inform convergence signal", () => {
  it("starts null after reset", () => {
    expect(getInformConvergenceSignal()).toBeNull()
  })

  it("execute sets signal when requesting paraphrase from human", async () => {
    const tool = createInformConvergenceTool()
    await tool.execute!({
      items: [{ id: "i1", prompt: "Please paraphrase what you understood about HNSW" }],
    }, {} as any)
    const signal = getInformConvergenceSignal()
    expect(signal).not.toBeNull()
    expect(signal!.items[0]!.id).toBe("i1")
    expect(signal!.items[0]!.prompt).toBe("Please paraphrase what you understood about HNSW")
  })

  it("execute sets signal when confirming/correcting human paraphrase", async () => {
    const tool = createInformConvergenceTool()
    await tool.execute!({
      items: [{ id: "i1", response: "Almost right — HNSW uses layers not just edges", consistent: false }],
    }, {} as any)
    const signal = getInformConvergenceSignal()
    expect(signal!.items[0]!.response).toBe("Almost right — HNSW uses layers not just edges")
    expect(signal!.items[0]!.consistent).toBe(false)
  })

  it("execute sets signal with consistent true to close", async () => {
    const tool = createInformConvergenceTool()
    await tool.execute!({
      items: [{ id: "i1", response: "Exactly right", consistent: true }],
    }, {} as any)
    expect(getInformConvergenceSignal()!.items[0]!.consistent).toBe(true)
  })

  it("resetInformConvergenceSignal clears after set", async () => {
    const tool = createInformConvergenceTool()
    await tool.execute!({ items: [{ id: "i1", prompt: "paraphrase?" }] }, {} as any)
    expect(getInformConvergenceSignal()).not.toBeNull()
    resetInformConvergenceSignal()
    expect(getInformConvergenceSignal()).toBeNull()
  })

  it("execute returns a string indicating turn will end", async () => {
    const tool = createInformConvergenceTool()
    const result = await tool.execute!({ items: [{ id: "i1", prompt: "paraphrase?" }] }, {} as any)
    expect(typeof result).toBe("string")
    expect(result).toBeTruthy()
  })
})

import { describe, it, expect, beforeEach } from "bun:test"
import {
  createClarifyTool, getClarifySignal, resetClarifySignal,
  createClarifyConvergenceTool, getClarifyConvergenceSignal, resetClarifyConvergenceSignal,
} from "./clarify"

beforeEach(() => {
  resetClarifySignal()
  resetClarifyConvergenceSignal()
})

describe("clarify signal", () => {
  it("starts null after reset", () => {
    expect(getClarifySignal()).toBeNull()
  })

  it("execute sets signal with is-kind item", async () => {
    const tool = createClarifyTool()
    await tool.execute!({
      items: [{ id: "c1", content: "What is the db engine?", why: "need to know before schema design", kind: "is" }],
    }, {} as any)
    const signal = getClarifySignal()
    expect(signal).not.toBeNull()
    expect(signal!.items[0]!.id).toBe("c1")
    expect(signal!.items[0]!.kind).toBe("is")
    expect(signal!.items[0]!.why).toBe("need to know before schema design")
  })

  it("execute sets signal with ought-kind item", async () => {
    const tool = createClarifyTool()
    await tool.execute!({
      items: [{ id: "c1", content: "postgres or sqlite?", why: "determines schema strategy", kind: "ought" }],
    }, {} as any)
    expect(getClarifySignal()!.items[0]!.kind).toBe("ought")
  })

  it("execute sets signal with links", async () => {
    const tool = createClarifyTool()
    await tool.execute!({
      items: [{
        id: "c2",
        content: "which db should we use?",
        why: "schema depends on this",
        kind: "ought",
        links: [{ target: "c1", relation: "depends_on" }],
      }],
    }, {} as any)
    const signal = getClarifySignal()
    expect(signal!.items[0]!.links).toHaveLength(1)
    expect(signal!.items[0]!.links![0]!.target).toBe("c1")
    expect(signal!.items[0]!.links![0]!.relation).toBe("depends_on")
  })

  it("execute sets signal with multiple items", async () => {
    const tool = createClarifyTool()
    await tool.execute!({
      items: [
        { id: "c1", content: "What env?", why: "affects config", kind: "is" },
        { id: "c2", content: "Which db?", why: "affects schema", kind: "ought" },
      ],
    }, {} as any)
    expect(getClarifySignal()!.items).toHaveLength(2)
  })

  it("resetClarifySignal clears after set", async () => {
    const tool = createClarifyTool()
    await tool.execute!({ items: [{ id: "c1", content: "?", why: "x", kind: "is" }] }, {} as any)
    expect(getClarifySignal()).not.toBeNull()
    resetClarifySignal()
    expect(getClarifySignal()).toBeNull()
  })

  it("execute returns a string indicating turn will end", async () => {
    const tool = createClarifyTool()
    const result = await tool.execute!({ items: [{ id: "c1", content: "?", why: "x", kind: "is" }] }, {} as any)
    expect(typeof result).toBe("string")
    expect(result).toBeTruthy()
  })
})

describe("clarify convergence signal", () => {
  it("starts null after reset", () => {
    expect(getClarifyConvergenceSignal()).toBeNull()
  })

  it("execute sets signal with proposal", async () => {
    const tool = createClarifyConvergenceTool()
    await tool.execute!({
      items: [{ id: "c1", proposal: "I'll use postgres with optimistic locking" }],
    }, {} as any)
    const signal = getClarifyConvergenceSignal()
    expect(signal).not.toBeNull()
    expect(signal!.items[0]!.id).toBe("c1")
    expect(signal!.items[0]!.proposal).toBe("I'll use postgres with optimistic locking")
  })

  it("execute sets signal with multiple items", async () => {
    const tool = createClarifyConvergenceTool()
    await tool.execute!({
      items: [
        { id: "c1", proposal: "use postgres" },
        { id: "c2", proposal: "deploy to AWS" },
      ],
    }, {} as any)
    expect(getClarifyConvergenceSignal()!.items).toHaveLength(2)
  })

  it("resetClarifyConvergenceSignal clears after set", async () => {
    const tool = createClarifyConvergenceTool()
    await tool.execute!({ items: [{ id: "c1", proposal: "x" }] }, {} as any)
    expect(getClarifyConvergenceSignal()).not.toBeNull()
    resetClarifyConvergenceSignal()
    expect(getClarifyConvergenceSignal()).toBeNull()
  })

  it("execute returns a string indicating turn will end", async () => {
    const tool = createClarifyConvergenceTool()
    const result = await tool.execute!({ items: [{ id: "c1", proposal: "x" }] }, {} as any)
    expect(typeof result).toBe("string")
    expect(result).toBeTruthy()
  })
})

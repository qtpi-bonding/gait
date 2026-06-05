import { describe, it, expect, beforeEach } from "bun:test"
import { createQuestionsTool, getQuestionsSignal, resetQuestionsSignal } from "./questions"

beforeEach(() => {
  resetQuestionsSignal()
})

describe("questions signal", () => {
  it("starts null after reset", () => {
    expect(getQuestionsSignal()).toBeNull()
  })

  it("execute sets the signal with provided items", async () => {
    const tool = createQuestionsTool()
    await tool.execute!({ items: [{ id: "q1", content: "Which branch?" }] }, {} as any)
    const signal = getQuestionsSignal()
    expect(signal).not.toBeNull()
    expect(signal!.items).toHaveLength(1)
    expect(signal!.items[0]!.id).toBe("q1")
    expect(signal!.items[0]!.content).toBe("Which branch?")
  })

  it("execute sets signal with multiple questions", async () => {
    const tool = createQuestionsTool()
    await tool.execute!({
      items: [
        { id: "q1", content: "What env?" },
        { id: "q2", content: "Run tests first?" },
      ],
    }, {} as any)
    expect(getQuestionsSignal()!.items).toHaveLength(2)
  })

  it("resetQuestionsSignal clears signal after it was set", async () => {
    const tool = createQuestionsTool()
    await tool.execute!({ items: [{ id: "q1", content: "Hello?" }] }, {} as any)
    expect(getQuestionsSignal()).not.toBeNull()
    resetQuestionsSignal()
    expect(getQuestionsSignal()).toBeNull()
  })

  it("execute returns a string indicating turn will end", async () => {
    const tool = createQuestionsTool()
    const result = await tool.execute!({ items: [{ id: "q1", content: "?" }] }, {} as any)
    expect(typeof result).toBe("string")
    expect(result).toBeTruthy()
  })
})

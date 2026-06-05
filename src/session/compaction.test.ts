import { describe, it, expect } from "bun:test"
import { estimateTokens, needsCompaction, chunkEvents } from "./compaction"
import type { AnyEvent } from "./transcript"

const makeEvent = (content: string): AnyEvent => ({
  type: "user_turn",
  content,
  ts: Date.now(),
})

describe("estimateTokens", () => {
  it("estimates 1 token per ~3.5 chars", () => {
    const tokens = estimateTokens("hello world") // 11 chars
    expect(tokens).toBeGreaterThan(2)
    expect(tokens).toBeLessThan(5)
  })

  it("handles empty string", () => {
    expect(estimateTokens("")).toBe(0)
  })
})

describe("needsCompaction", () => {
  it("returns false when under threshold", () => {
    const events: AnyEvent[] = [makeEvent("short")]
    expect(needsCompaction(events, 100_000, 0.8)).toBe(false)
  })

  it("returns true when over threshold", () => {
    const longContent = "x".repeat(100_000)
    const events: AnyEvent[] = [makeEvent(longContent)]
    expect(needsCompaction(events, 10_000, 0.8)).toBe(true)
  })
})

describe("chunkEvents", () => {
  it("splits events into chunks by token limit", () => {
    const events = Array.from({ length: 10 }, (_, i) => makeEvent(`message ${i}`))
    const chunks = chunkEvents(events, 20) // tiny limit forces splits
    expect(chunks.length).toBeGreaterThan(1)
    expect(chunks.flat()).toHaveLength(events.length)
  })

  it("returns single chunk when events fit", () => {
    const events = [makeEvent("short")]
    const chunks = chunkEvents(events, 10_000)
    expect(chunks).toHaveLength(1)
  })
})

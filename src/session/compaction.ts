import type { LanguageModel } from "ai"
import { generateText } from "ai"
import type { AnyEvent } from "./transcript"

// Rough token estimation: ~3.5 chars per token (average across code + prose)
export function estimateTokens(text: string): number {
  return Math.ceil(text.length / 3.5)
}

export function estimateEventsTokens(events: AnyEvent[]): number {
  return estimateTokens(events.map((e) => JSON.stringify(e)).join("\n"))
}

export function needsCompaction(
  events: AnyEvent[],
  contextWindow: number,
  threshold: number,
): boolean {
  const tokens = estimateEventsTokens(events)
  return tokens > contextWindow * threshold
}

// Split events into chunks that each fit within maxTokens
export function chunkEvents(events: AnyEvent[], maxTokens: number): AnyEvent[][] {
  const chunks: AnyEvent[][] = []
  let current: AnyEvent[] = []
  let currentTokens = 0

  for (const event of events) {
    const t = estimateTokens(JSON.stringify(event))
    if (currentTokens + t > maxTokens && current.length > 0) {
      chunks.push(current)
      current = []
      currentTokens = 0
    }
    current.push(event)
    currentTokens += t
  }
  if (current.length > 0) chunks.push(current)
  return chunks
}

async function summarizeChunk(model: LanguageModel, events: AnyEvent[]): Promise<string> {
  const transcript = events.map((e) => JSON.stringify(e)).join("\n")
  const { text } = await generateText({
    model,
    messages: [
      {
        role: "user",
        content: `Summarize the following AI agent session transcript into a dense, factual paragraph. Focus on: what was being worked on, what code changes were made, what decisions were reached, and any open issues. Do not include meta-commentary. Transcript:\n\n${transcript}`,
      },
    ],
  })
  return text
}

// Returns the compaction summary and token usage
export async function compact(opts: {
  model: LanguageModel
  events: AnyEvent[]         // events to summarize (since last compaction)
  contextWindow: number      // model's declared context window in tokens
}): Promise<{ summary: string; usage: { input: number; output: number } }> {
  const { model, events, contextWindow } = opts

  // If events fit in one call (leave 25% headroom for the summarize prompt)
  const maxTokensPerCall = Math.floor(contextWindow * 0.75)
  const totalTokens = estimateEventsTokens(events)

  if (totalTokens <= maxTokensPerCall) {
    const summary = await summarizeChunk(model, events)
    return { summary, usage: { input: totalTokens, output: estimateTokens(summary) } }
  }

  // Overflow fallback: chunked summarization
  const chunks = chunkEvents(events, maxTokensPerCall)
  const chunkSummaries = await Promise.all(chunks.map((chunk) => summarizeChunk(model, chunk)))

  // Summarize the summaries
  const combinedSummaries = chunkSummaries.join("\n\n---\n\n")
  const finalSummary = await summarizeChunk(model, [
    { type: "user_turn", content: `Combine these partial summaries into one:\n\n${combinedSummaries}`, ts: Date.now() },
  ])

  const inputTokens = estimateTokens(combinedSummaries)
  const outputTokens = estimateTokens(finalSummary)
  return { summary: finalSummary, usage: { input: inputTokens, output: outputTokens } }
}

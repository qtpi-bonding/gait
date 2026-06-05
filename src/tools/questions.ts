import { tool } from "ai"
import { z } from "zod"

export interface QuestionsSignal {
  items: Array<{ id: string; content: string }>
}

// Shared mutable state per turn — reset at turn start
let _signal: QuestionsSignal | null = null

export function resetQuestionsSignal(): void {
  _signal = null
}

export function getQuestionsSignal(): QuestionsSignal | null {
  return _signal
}

export function createQuestionsTool() {
  return tool({
    description: "Ask the user one or more questions to clarify the task. Use this when you need information before proceeding. This ends the current turn — the user will reply in the next invocation.",
    parameters: z.object({
      items: z.array(z.object({
        id: z.string().describe("Short identifier for this question, e.g. 'q1'"),
        content: z.string().describe("The question text"),
      })).describe("Questions to ask the user"),
    }),
    execute: async ({ items }) => {
      _signal = { items }
      return `Questions queued. Turn will end after this step.`
    },
  })
}

import { tool } from "ai"
import { z } from "zod"

export interface ClarifySignal {
  items: Array<{
    id: string
    content: string
    why: string
    kind: "is" | "ought"
    links?: Array<{ target: string; relation: "depends_on" | "related_to" | "part_of" }>
  }>
}

// Shared mutable state per turn — reset at turn start
let _signal: ClarifySignal | null = null

export function resetClarifySignal(): void {
  _signal = null
}

export function getClarifySignal(): ClarifySignal | null {
  return _signal
}

export interface ClarifyConvergenceSignal {
  items: Array<{ id: string; proposal: string }>
}

let _convergenceSignal: ClarifyConvergenceSignal | null = null

export function resetClarifyConvergenceSignal(): void {
  _convergenceSignal = null
}

export function getClarifyConvergenceSignal(): ClarifyConvergenceSignal | null {
  return _convergenceSignal
}

export function createClarifyConvergenceTool() {
  return tool({
    description: "Propose your understanding of a clarify item. The human will confirm or correct. Use after receiving a clarify_reply. This ends the turn.",
    parameters: z.object({
      items: z.array(z.object({
        id: z.string(),
        proposal: z.string().describe("Your current understanding of what was answered"),
      })),
    }),
    execute: async ({ items }) => {
      _convergenceSignal = { items }
      return `Convergence proposal queued. Turn will end.`
    },
  })
}

export function createClarifyTool() {
  return tool({
    description: "Ask the user one or more clarifying questions to resolve unknowns before proceeding. Use `kind: \"is\"` for factual gaps (e.g. \"which database engine?\") and `kind: \"ought\"` for value judgments only the user can make (e.g. \"postgres or sqlite?\"). Provide `why` for each question so the user understands the cost of guessing. This ends the current turn — the user will reply in the next invocation.",
    parameters: z.object({
      items: z.array(z.object({
        id: z.string().describe("Short identifier for this question, e.g. 'c1'"),
        content: z.string().describe("The question text"),
        why: z.string().describe("Why this question matters — helps the user understand the cost of skipping"),
        kind: z.enum(["is", "ought"]).describe("'is' for facts, 'ought' for judgment calls"),
        links: z.array(z.object({
          target: z.string().describe("id of the related question/item"),
          relation: z.enum(["depends_on", "related_to", "part_of"]),
        })).optional().describe("Optional relationships to other questions or items"),
      })).describe("Clarifying questions to ask the user"),
    }),
    execute: async ({ items }) => {
      _signal = { items }
      return `Clarifying questions queued. Turn will end after this step.`
    },
  })
}

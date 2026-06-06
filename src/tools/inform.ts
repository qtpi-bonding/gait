import { tool } from "ai"
import { z } from "zod"

export interface InformSignal {
  items: Array<{
    id: string
    content: string
    context?: string
    links?: Array<{ target: string; relation: "depends_on" | "related_to" | "part_of" }>
  }>
}

// Shared mutable state per turn — reset at turn start
let _signal: InformSignal | null = null

export function resetInformSignal(): void {
  _signal = null
}

export function getInformSignal(): InformSignal | null {
  return _signal
}

export interface InformConvergenceSignal {
  items: Array<{
    id: string
    prompt?: string
    response?: string
    consistent?: boolean
  }>
}

let _convergenceSignal: InformConvergenceSignal | null = null

export function resetInformConvergenceSignal(): void {
  _convergenceSignal = null
}

export function getInformConvergenceSignal(): InformConvergenceSignal | null {
  return _convergenceSignal
}

export function createInformConvergenceTool() {
  return tool({
    description: "Either (1) request the human to paraphrase their understanding using 'prompt', or (2) confirm/correct their paraphrase using 'response' + 'consistent'. This ends the turn.",
    parameters: z.object({
      items: z.array(z.object({
        id: z.string(),
        prompt: z.string().optional().describe("Ask human to paraphrase (first call)"),
        response: z.string().optional().describe("Your correction or confirmation of human's paraphrase"),
        consistent: z.boolean().optional().describe("true = human understood correctly"),
      })),
    }),
    execute: async ({ items }) => {
      _convergenceSignal = { items }
      return `Inform convergence queued. Turn will end.`
    },
  })
}

export function createInformTool() {
  return tool({
    description: "Convey knowledge to the user about the codebase, a topic, or a decision rationale. Use this when the user needs context you have but they don't — explanations, summaries, or \"here's why I chose X\". The user will paraphrase back to confirm understanding (inform convergence). This ends the current turn.",
    parameters: z.object({
      items: z.array(z.object({
        id: z.string().describe("Short identifier for this item, e.g. 'i1'"),
        content: z.string().describe("The information to convey"),
        context: z.string().optional().describe("Why this is being shared / what prompted it"),
        links: z.array(z.object({
          target: z.string().describe("id of the related inform item"),
          relation: z.enum(["depends_on", "related_to", "part_of"]),
        })).optional().describe("Optional relationships to other inform items"),
      })).describe("Pieces of information to convey to the user"),
    }),
    execute: async ({ items }) => {
      _signal = { items }
      return `Information queued. Turn will end after this step.`
    },
  })
}

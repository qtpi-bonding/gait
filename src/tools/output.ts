import { tool } from "ai"
import { z } from "zod"

export interface OutputSignal {
  summary: string
  result?: Record<string, unknown>
}

let _signal: OutputSignal | null = null

export function resetOutputSignal(): void {
  _signal = null
}

export function getOutputSignal(): OutputSignal | null {
  return _signal
}

export function createOutputTool() {
  return tool({
    description: "Signal that your turn is complete. Always call this as your final action with a one-sentence summary of what you accomplished.",
    parameters: z.object({
      summary: z.string().describe("One sentence summary of what was accomplished this turn"),
      result: z.record(z.unknown()).optional().describe("Structured result for workflow nodes"),
    }),
    execute: async ({ summary, result }) => {
      if (!_signal) _signal = { summary, result }
      return "recorded"
    },
  })
}

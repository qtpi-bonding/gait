import { tool } from "ai"
import { z } from "zod"
import crypto from "crypto"

// Injected at turn-loop time so spawn_subagent can call runTurn recursively
let _runTurn: ((opts: SubagentRunOpts) => Promise<SubagentResult>) | null = null

export interface SubagentRunOpts {
  sessionId: string
  task: string
  workspacePath: string
  contextFiles: string[]
}

export interface SubagentResult {
  status: "done" | "error"
  result: string
}

export function registerSubagentRunner(
  fn: (opts: SubagentRunOpts) => Promise<SubagentResult>
): void {
  _runTurn = fn
}

export function createSpawnSubagentTool(parentSessionId: string, workspacePath: string) {
  return tool({
    description:
      "Spawn a subagent to complete a specific subtask. The subagent runs in its own session and cannot ask questions (v1). Returns the subagent's final output.",
    parameters: z.object({
      task: z.string().describe("The complete task description for the subagent"),
      context_files: z
        .array(z.string())
        .default([])
        .describe("Files to pass as context to the subagent (relative to codebase)"),
    }),
    execute: async ({ task, context_files }) => {
      if (!_runTurn) throw new Error("subagent runner not registered")

      const subId = `${parentSessionId}-sub-${crypto.randomUUID().slice(0, 8)}`
      const result = await _runTurn({
        sessionId: subId,
        task,
        workspacePath,
        contextFiles: context_files,
      })

      return JSON.stringify({ session_id: subId, status: result.status, result: result.result })
    },
  })
}

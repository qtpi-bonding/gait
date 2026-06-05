import { tool, type CoreTool } from "ai"
import crypto from "crypto"

export interface PendingPermission {
  id: string
  name: string
  input: Record<string, unknown>
  reason: string
}

export function isAllowed(toolName: string, allowList: string[]): boolean {
  return allowList.includes(toolName)
}

// Wraps all tools so that non-allowed ones are batched into `pending` and
// return a sentinel value, while allowed ones execute normally.
// The turn loop aborts streamText when pending becomes non-empty.
export function wrapTools(
  tools: Record<string, CoreTool>,
  allowList: string[],
  pending: PendingPermission[],
  codebasePath: string,
): Record<string, CoreTool> {
  return Object.fromEntries(
    Object.entries(tools).map(([name, t]) => [
      name,
      tool({
        description: t.description ?? "",
        parameters: t.parameters,
        execute: async (args, opts) => {
          if (isAllowed(name, allowList)) {
            // Allowed: delegate to original execute
            return await (t as any).execute(args, opts)
          } else {
            // Not allowed: add to pending batch, return sentinel
            const id = crypto.randomUUID()
            pending.push({
              id,
              name,
              input: args as Record<string, unknown>,
              reason: `LLM requested ${name}`,
            })
            return `[permission required for ${name} — awaiting approval]`
          }
        },
      }),
    ])
  )
}

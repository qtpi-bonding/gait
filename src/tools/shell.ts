// ainix-specific: permission-gated shell command runner
import { tool } from "ai"
import { z } from "zod"
import { execSync } from "child_process"

export function createShellTool(codebasePath: string) {
  return tool({
    description: "Run a shell command in the codebase directory. All shell commands require permission approval unless pre-allowed in ainix.jsonc.",
    parameters: z.object({
      cmd: z.string().describe("Shell command to execute"),
      reason: z.string().describe("Why this command is needed"),
      timeout_ms: z.number().optional().default(30_000).describe("Timeout in milliseconds (default 30s)"),
    }),
    execute: async ({ cmd, timeout_ms = 30_000 }) => {
      const start = Date.now()
      try {
        const output = execSync(cmd, {
          cwd: codebasePath,
          timeout: timeout_ms,
          encoding: "utf-8",
          stdio: ["ignore", "pipe", "pipe"],
        })
        const duration = Date.now() - start
        return `exit 0 (${duration}ms)\n${output}`
      } catch (err: any) {
        const duration = Date.now() - start
        const output = (err.stdout ?? "") + (err.stderr ?? "")
        const timed_out = err.code === "ETIMEDOUT"
        const exit_code = err.status ?? 1
        if (timed_out) return `TIMED OUT after ${timeout_ms}ms\n${output}`
        return `exit ${exit_code} (${duration}ms)\n${output}`
      }
    },
  })
}

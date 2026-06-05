import { createReadTool } from "./read"
import { createWriteTool } from "./write"
import { createEditTool } from "./edit"
import { createMultieditTool } from "./multiedit"
import { createApplyPatchTool } from "./apply_patch"
import { createGlobTool } from "./glob"
import { createGrepTool } from "./grep"
import { createLsTool } from "./ls"
import { createCodesearchTool } from "./codesearch"
import { createWebfetchTool } from "./webfetch"
import { createQuestionsTool } from "./questions"
import { createShellTool } from "./shell"
import type { CoreTool } from "ai"
import type { ConfigDefinedTool } from "../config"
import { execSync } from "child_process"
import { tool } from "ai"
import { z } from "zod"

export function createBuiltinTools(codebasePath: string): Record<string, CoreTool> {
  return {
    read: createReadTool(codebasePath),
    write: createWriteTool(codebasePath),
    edit: createEditTool(codebasePath),
    multiedit: createMultieditTool(codebasePath),
    apply_patch: createApplyPatchTool(codebasePath),
    glob: createGlobTool(codebasePath),
    grep: createGrepTool(codebasePath),
    ls: createLsTool(codebasePath),
    codesearch: createCodesearchTool(codebasePath),
    webfetch: createWebfetchTool(),
    shell: createShellTool(codebasePath),
    questions: createQuestionsTool(),
  }
}

export function createConfigTools(
  configTools: ConfigDefinedTool[],
  workspacePath: string,
  codebasePath: string,
): Record<string, CoreTool> {
  const result: Record<string, CoreTool> = {}
  for (const def of configTools) {
    const cwd = def.cwd === "workspace" ? workspacePath
              : def.cwd === "codebase" ? codebasePath
              : codebasePath  // default
    result[def.name] = tool({
      description: def.description,
      parameters: z.object({}),
      execute: async () => {
        const start = Date.now()
        try {
          const output = execSync(def.cmd, { cwd, encoding: "utf-8", stdio: ["ignore", "pipe", "pipe"] })
          return `exit 0 (${Date.now() - start}ms)\n${output}`
        } catch (err: any) {
          const output = (err.stdout ?? "") + (err.stderr ?? "")
          return `exit ${err.status ?? 1} (${Date.now() - start}ms)\n${output}`
        }
      },
    })
  }
  return result
}

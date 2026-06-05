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

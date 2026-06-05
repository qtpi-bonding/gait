// Ported from OpenCode (MIT) — https://github.com/sst/opencode
import { tool } from "ai"
import { z } from "zod"
import fs from "fs"
import path from "path"

export function createWriteTool(codebasePath: string) {
  return tool({
    description:
      "Write content to a file in the codebase. Creates the file (and parent directories) if they don't exist. Paths are relative to the codebase root.",
    parameters: z.object({
      filePath: z.string().describe("Path to the file to write (relative to codebase root, or absolute)"),
      content: z.string().describe("The content to write to the file"),
    }),
    execute: async ({ filePath, content }) => {
      const resolved = path.isAbsolute(filePath) ? filePath : path.join(codebasePath, filePath)

      fs.mkdirSync(path.dirname(resolved), { recursive: true })
      fs.writeFileSync(resolved, content, "utf-8")

      const bytes = Buffer.byteLength(content, "utf-8")
      return `Wrote ${bytes} bytes to ${resolved}`
    },
  })
}

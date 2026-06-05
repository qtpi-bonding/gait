// Ported from OpenCode (MIT) — https://github.com/sst/opencode
import { tool } from "ai"
import { z } from "zod"
import fs from "fs"
import path from "path"
import { replace } from "./edit"

export function createMultieditTool(codebasePath: string) {
  return tool({
    description:
      "Apply multiple sequential edits to a single file. Each edit replaces oldString with newString. Edits are applied in order.",
    parameters: z.object({
      filePath: z.string().describe("Path to the file to modify (relative to codebase root, or absolute)"),
      edits: z
        .array(
          z.object({
            oldString: z.string().describe("The text to replace"),
            newString: z.string().describe("The replacement text"),
            replaceAll: z.boolean().optional().describe("Replace all occurrences (default false)"),
          }),
        )
        .describe("Array of edit operations to perform sequentially"),
    }),
    execute: async ({ filePath, edits }) => {
      if (!filePath) throw new Error("filePath is required")
      if (!edits || edits.length === 0) throw new Error("edits array must not be empty")

      const resolved = path.isAbsolute(filePath) ? filePath : path.join(codebasePath, filePath)

      if (!fs.existsSync(resolved)) throw new Error(`File not found: ${resolved}`)

      const stat = fs.statSync(resolved)
      if (stat.isDirectory()) throw new Error(`Path is a directory, not a file: ${resolved}`)

      let content = fs.readFileSync(resolved, "utf-8")

      for (let i = 0; i < edits.length; i++) {
        const edit = edits[i]!
        if (edit.oldString === edit.newString) {
          throw new Error(`Edit ${i + 1}: oldString and newString must be different`)
        }
        try {
          content = replace(content, edit.oldString, edit.newString, edit.replaceAll ?? false)
        } catch (err) {
          throw new Error(`Edit ${i + 1} failed: ${err instanceof Error ? err.message : String(err)}`)
        }
      }

      fs.writeFileSync(resolved, content, "utf-8")

      return `Applied ${edits.length} edits to ${resolved}`
    },
  })
}

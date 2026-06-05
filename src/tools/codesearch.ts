// Ported from OpenCode (MIT) — https://github.com/sst/opencode
import { tool } from "ai"
import { z } from "zod"
import { execSync } from "child_process"
import fs from "fs"
import path from "path"

function walkAndSearch(dir: string, pattern: RegExp, results: string[], cap: number): void {
  if (results.length >= cap) return
  let entries: fs.Dirent[]
  try {
    entries = fs.readdirSync(dir, { withFileTypes: true })
  } catch {
    return
  }
  for (const entry of entries) {
    if (results.length >= cap) break
    if (entry.name.startsWith(".") || entry.name === "node_modules") continue
    const full = path.join(dir, entry.name)
    if (entry.isDirectory()) {
      walkAndSearch(full, pattern, results, cap)
    } else if (entry.isFile()) {
      let content: string
      try {
        content = fs.readFileSync(full, "utf-8")
      } catch {
        continue
      }
      const lines = content.split("\n")
      for (let i = 0; i < lines.length && results.length < cap; i++) {
        if (pattern.test(lines[i]!)) {
          results.push(`${full}:${i + 1}: ${lines[i]}`)
        }
      }
    }
  }
}

export function createCodesearchTool(codebasePath: string) {
  return tool({
    description:
      "Search for symbols, function names, class definitions, or patterns in the codebase. Use for finding where something is defined or used. Returns file:line matches.",
    parameters: z.object({
      query: z.string().describe("Symbol name, function name, or regex pattern to search for"),
      path: z.string().optional().describe("Directory to search within (relative to codebase root). Defaults to entire codebase."),
      case_sensitive: z.boolean().optional().default(false).describe("Case-sensitive search (default false)"),
    }),
    execute: async ({ query, path: searchPath, case_sensitive = false }) => {
      const searchDir = searchPath ? path.join(codebasePath, searchPath) : codebasePath
      const MAX_RESULTS = 50

      // Try ripgrep first
      try {
        const flags = case_sensitive ? "" : "-i"
        const output = execSync(
          `rg ${flags} --line-number --no-heading --max-count=1 "${query.replace(/"/g, '\\"')}" "${searchDir}"`,
          { encoding: "utf-8", timeout: 15_000 },
        )
        const lines = output.trim().split("\n").filter(Boolean).slice(0, MAX_RESULTS)
        if (lines.length === 0) return "No matches found."
        const header = lines.length === MAX_RESULTS ? `(first ${MAX_RESULTS} results)\n` : ""
        return header + lines.join("\n")
      } catch (err: any) {
        // exit code 1 means no matches (not an error)
        if (err?.status === 1) return "No matches found."
        // rg not installed — fall back to JS walk
      }

      try {
        const flags = case_sensitive ? "" : "i"
        const pattern = new RegExp(query, flags)
        const results: string[] = []
        walkAndSearch(searchDir, pattern, results, MAX_RESULTS)
        if (results.length === 0) return "No matches found."
        return results.join("\n")
      } catch {
        return `Invalid search pattern: ${query}`
      }
    },
  })
}

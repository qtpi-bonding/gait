// Ported from OpenCode (MIT) — https://github.com/sst/opencode
import { tool } from "ai"
import { z } from "zod"
import fs from "fs"
import path from "path"
import { execSync } from "child_process"

const MAX_LINE_LENGTH = 2000
const LIMIT = 100

interface MatchEntry {
  path: string
  lineNum: number
  lineText: string
  modTime: number
}

function rgSearch(pattern: string, searchPath: string, include?: string): MatchEntry[] {
  const args = [
    "rg",
    "-nH",
    "--hidden",
    "--no-messages",
    "--field-match-separator=|",
    "--regexp",
    pattern,
  ]
  if (include) args.push("--glob", include)
  args.push(searchPath)

  const output = execSync(args.map((a) => JSON.stringify(a)).join(" "), {
    encoding: "utf-8",
    stdio: ["pipe", "pipe", "pipe"],
  })

  const lines = output.trim().split(/\r?\n/)
  const matches: MatchEntry[] = []

  for (const line of lines) {
    if (!line) continue
    const [filePath, lineNumStr, ...rest] = line.split("|")
    if (!filePath || !lineNumStr || rest.length === 0) continue
    const lineNum = parseInt(lineNumStr, 10)
    const lineText = rest.join("|")
    let modTime = 0
    try { modTime = fs.statSync(filePath).mtimeMs } catch {}
    matches.push({ path: filePath, lineNum, lineText, modTime })
  }

  return matches
}

function* walkFiles(dir: string): Generator<string> {
  let entries: fs.Dirent[]
  try { entries = fs.readdirSync(dir, { withFileTypes: true }) } catch { return }
  for (const entry of entries) {
    const full = path.join(dir, entry.name)
    if (entry.isDirectory()) { yield* walkFiles(full) }
    else if (entry.isFile()) { yield full }
  }
}

function regexSearch(pattern: string, searchPath: string): MatchEntry[] {
  let regex: RegExp
  try { regex = new RegExp(pattern) } catch (e) { throw new Error(`Invalid regex pattern: ${pattern}`) }

  const matches: MatchEntry[] = []
  const isFile = fs.existsSync(searchPath) && fs.statSync(searchPath).isFile()
  const files = isFile ? [searchPath] : [...walkFiles(searchPath)]

  for (const file of files) {
    let content: string
    try { content = fs.readFileSync(file, "utf-8") } catch { continue }
    let modTime = 0
    try { modTime = fs.statSync(file).mtimeMs } catch {}
    const lines = content.split("\n")
    for (let i = 0; i < lines.length; i++) {
      const lineText = lines[i]!
      if (regex.test(lineText)) {
        matches.push({ path: file, lineNum: i + 1, lineText, modTime })
      }
    }
  }

  return matches
}

export function createGrepTool(codebasePath: string) {
  return tool({
    description:
      "Search for a regex pattern across files in the codebase. Returns matching lines with file and line number.",
    parameters: z.object({
      pattern: z.string().describe("The regex pattern to search for in file contents"),
      path: z
        .string()
        .optional()
        .describe("The directory or file to search in (relative to codebase root). Defaults to entire codebase."),
      include: z
        .string()
        .optional()
        .describe('File pattern to include in the search (e.g. "*.js", "*.{ts,tsx}")'),
    }),
    execute: async ({ pattern, path: searchPath, include }) => {
      if (!pattern) throw new Error("pattern is required")

      const resolved = searchPath
        ? path.isAbsolute(searchPath) ? searchPath : path.join(codebasePath, searchPath)
        : codebasePath

      let matches: MatchEntry[]

      try {
        matches = rgSearch(pattern, resolved, include)
      } catch {
        // rg not available or failed — fall back to JS regex walk
        try {
          matches = regexSearch(pattern, resolved)
        } catch (err) {
          throw new Error(`grep failed: ${err instanceof Error ? err.message : String(err)}`)
        }
      }

      if (matches.length === 0) return "No files found"

      matches.sort((a, b) => b.modTime - a.modTime)

      const truncated = matches.length > LIMIT
      const finalMatches = truncated ? matches.slice(0, LIMIT) : matches

      const outputLines = [`Found ${finalMatches.length} matches`]
      let currentFile = ""
      for (const match of finalMatches) {
        if (currentFile !== match.path) {
          if (currentFile !== "") outputLines.push("")
          currentFile = match.path
          outputLines.push(`${match.path}:`)
        }
        const truncatedLine =
          match.lineText.length > MAX_LINE_LENGTH
            ? match.lineText.substring(0, MAX_LINE_LENGTH) + "..."
            : match.lineText
        outputLines.push(`  Line ${match.lineNum}: ${truncatedLine}`)
      }

      if (truncated) {
        outputLines.push("")
        outputLines.push("(Results are truncated. Consider using a more specific path or pattern.)")
      }

      return outputLines.join("\n")
    },
  })
}

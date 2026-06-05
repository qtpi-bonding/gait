// Ported from OpenCode (MIT) — https://github.com/sst/opencode
import { tool } from "ai"
import { z } from "zod"
import fs from "fs"
import path from "path"
import { execSync } from "child_process"

const IGNORE_PATTERNS = new Set([
  "node_modules",
  "__pycache__",
  ".git",
  "dist",
  "build",
  "target",
  "vendor",
  "bin",
  "obj",
  ".idea",
  ".vscode",
  ".zig-cache",
  "zig-out",
  ".coverage",
  "coverage",
  "tmp",
  "temp",
  ".cache",
  "cache",
  "logs",
  ".venv",
  "venv",
  "env",
])

const LIMIT = 100

function shouldIgnore(name: string): boolean {
  return IGNORE_PATTERNS.has(name)
}

function collectFiles(dir: string, relativeBase: string, files: string[], limit: number): boolean {
  let entries: fs.Dirent[]
  try { entries = fs.readdirSync(dir, { withFileTypes: true }) } catch { return false }

  for (const entry of entries) {
    if (files.length >= limit) return true
    if (shouldIgnore(entry.name)) continue
    const relativePath = relativeBase ? `${relativeBase}/${entry.name}` : entry.name
    if (entry.isDirectory()) {
      if (collectFiles(path.join(dir, entry.name), relativePath, files, limit)) return true
    } else if (entry.isFile()) {
      files.push(relativePath)
    }
  }
  return false
}

function collectFilesWithRipgrep(searchDir: string): { files: string[]; truncated: boolean } {
  const ignoreGlobs = [...IGNORE_PATTERNS].map((p) => `--glob=!${p}/**`).join(" ")
  const output = execSync(`rg --files --hidden --no-messages ${ignoreGlobs} "${searchDir}"`, {
    encoding: "utf-8",
    stdio: ["pipe", "pipe", "pipe"],
  })
  const all = output
    .trim()
    .split("\n")
    .filter(Boolean)
    .map((f) => path.relative(searchDir, f))
  const truncated = all.length >= LIMIT
  return { files: truncated ? all.slice(0, LIMIT) : all, truncated }
}

function renderDirTree(searchDir: string, files: string[], truncated: boolean): string {
  const dirs = new Set<string>()
  const filesByDir = new Map<string, string[]>()

  for (const file of files) {
    const dir = path.dirname(file)
    const parts = dir === "." ? [] : dir.split(path.sep)
    for (let i = 0; i <= parts.length; i++) {
      const dirPath = i === 0 ? "." : parts.slice(0, i).join("/")
      dirs.add(dirPath)
    }
    if (!filesByDir.has(dir)) filesByDir.set(dir, [])
    filesByDir.get(dir)!.push(path.basename(file))
  }

  function renderDir(dirPath: string, depth: number): string {
    const indent = "  ".repeat(depth)
    let out = ""
    if (depth > 0) out += `${indent}${path.basename(dirPath)}/\n`
    const childIndent = "  ".repeat(depth + 1)
    const children = Array.from(dirs)
      .filter((d) => {
        const parent = d.includes("/") ? d.substring(0, d.lastIndexOf("/")) : "."
        return parent === dirPath && d !== dirPath
      })
      .sort()
    for (const child of children) out += renderDir(child, depth + 1)
    const dirFiles = filesByDir.get(dirPath) || []
    for (const f of dirFiles.sort()) out += `${childIndent}${f}\n`
    return out
  }

  let output = `${searchDir}/\n` + renderDir(".", 0)
  if (truncated) {
    output += "\n(Results are truncated. Only first 100 files shown.)"
  }
  return output
}

export function createLsTool(codebasePath: string) {
  return tool({
    description:
      "List files in a directory of the codebase as a tree. Ignores common build/dependency directories.",
    parameters: z.object({
      path: z
        .string()
        .optional()
        .describe(
          "The directory to list (relative to codebase root). Omit to list the codebase root.",
        ),
      ignore: z.array(z.string()).optional().describe("Additional directory/file names to ignore"),
    }),
    execute: async ({ path: dirPath, ignore }) => {
      const searchDir = dirPath
        ? path.isAbsolute(dirPath) ? dirPath : path.join(codebasePath, dirPath)
        : codebasePath

      if (!fs.existsSync(searchDir)) throw new Error(`Directory not found: ${searchDir}`)

      const stat = fs.statSync(searchDir)
      if (!stat.isDirectory()) throw new Error(`Not a directory: ${searchDir}`)

      // Add extra ignore patterns from params
      if (ignore) for (const p of ignore) IGNORE_PATTERNS.add(p)

      let files: string[]
      let truncated = false

      try {
        const result = collectFilesWithRipgrep(searchDir)
        files = result.files
        truncated = result.truncated
      } catch {
        // rg not available — walk manually
        files = []
        truncated = collectFiles(searchDir, "", files, LIMIT)
      }

      // Remove extra patterns added from params to avoid mutation side effects
      if (ignore) for (const p of ignore) IGNORE_PATTERNS.delete(p)

      return renderDirTree(searchDir, files, truncated)
    },
  })
}

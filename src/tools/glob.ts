// Ported from OpenCode (MIT) — https://github.com/sst/opencode
import { tool } from "ai"
import { z } from "zod"
import fs from "fs"
import path from "path"

const LIMIT = 100

// Recursive walk fallback (used when Bun.Glob is unavailable or rg not found)
function* walkFiles(dir: string, pattern: RegExp): Generator<string> {
  let entries: fs.Dirent[]
  try {
    entries = fs.readdirSync(dir, { withFileTypes: true })
  } catch {
    return
  }
  for (const entry of entries) {
    const full = path.join(dir, entry.name)
    if (entry.isDirectory()) {
      yield* walkFiles(full, pattern)
    } else if (entry.isFile()) {
      yield full
    }
  }
}

// Convert glob pattern to a rough regex for fallback
function globToRegex(pattern: string): RegExp {
  const escaped = pattern
    .replace(/[.+^${}()|[\]\\]/g, "\\$&")
    .replace(/\*\*/g, "GLOBSTAR")
    .replace(/\*/g, "[^/]*")
    .replace(/\?/g, "[^/]")
    .replace(/GLOBSTAR/g, ".*")
  return new RegExp(escaped + "$")
}

export function createGlobTool(codebasePath: string) {
  return tool({
    description:
      "Find files in the codebase matching a glob pattern. Results sorted by modification time (newest first).",
    parameters: z.object({
      pattern: z.string().describe("The glob pattern to match files against (e.g. '**/*.ts', 'src/**/*.json')"),
      path: z
        .string()
        .optional()
        .describe(
          "The directory to search in (relative to codebase root). Omit to search the entire codebase.",
        ),
    }),
    execute: async ({ pattern, path: pathPrefix }) => {
      const searchDir = pathPrefix
        ? path.isAbsolute(pathPrefix) ? pathPrefix : path.join(codebasePath, pathPrefix)
        : codebasePath

      if (!fs.existsSync(searchDir)) {
        throw new Error(`Directory not found: ${searchDir}`)
      }

      const files: Array<{ path: string; mtime: number }> = []
      let truncated = false

      try {
        // Use Bun.Glob if available
        const glob = new Bun.Glob(pattern)
        for (const file of glob.scanSync({ cwd: searchDir, onlyFiles: true })) {
          if (files.length >= LIMIT) { truncated = true; break }
          const full = path.join(searchDir, file)
          let mtime = 0
          try { mtime = fs.statSync(full).mtimeMs } catch {}
          files.push({ path: full, mtime })
        }
      } catch {
        // Fallback: walk + regex
        const regex = globToRegex(pattern)
        for (const full of walkFiles(searchDir, regex)) {
          if (!regex.test(full.slice(searchDir.length + 1))) continue
          if (files.length >= LIMIT) { truncated = true; break }
          let mtime = 0
          try { mtime = fs.statSync(full).mtimeMs } catch {}
          files.push({ path: full, mtime })
        }
      }

      files.sort((a, b) => b.mtime - a.mtime)

      if (files.length === 0) return "No files found"

      const lines = files.map((f) => f.path)
      if (truncated) {
        lines.push("")
        lines.push("(Results are truncated. Consider using a more specific path or pattern.)")
      }

      return lines.join("\n")
    },
  })
}

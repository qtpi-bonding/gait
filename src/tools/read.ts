// Ported from OpenCode (MIT) — https://github.com/sst/opencode
import { tool } from "ai"
import { z } from "zod"
import fs from "fs"
import path from "path"

const DEFAULT_READ_LIMIT = 2000
const MAX_LINE_LENGTH = 2000
const MAX_BYTES = 50 * 1024

const BINARY_EXTENSIONS = new Set([
  ".zip", ".tar", ".gz", ".exe", ".dll", ".so", ".class", ".jar", ".war",
  ".7z", ".doc", ".docx", ".xls", ".xlsx", ".ppt", ".pptx", ".odt", ".ods",
  ".odp", ".bin", ".dat", ".obj", ".o", ".a", ".lib", ".wasm", ".pyc", ".pyo",
])

function isBinaryByExtension(filepath: string): boolean {
  return BINARY_EXTENSIONS.has(path.extname(filepath).toLowerCase())
}

function isBinaryByContent(filepath: string): boolean {
  try {
    const bufferSize = Math.min(4096, fs.statSync(filepath).size)
    if (bufferSize === 0) return false
    const buf = Buffer.alloc(bufferSize)
    const fd = fs.openSync(filepath, "r")
    fs.readSync(fd, buf, 0, bufferSize, 0)
    fs.closeSync(fd)
    let nonPrintable = 0
    for (let i = 0; i < buf.length; i++) {
      const byte = buf[i]!
      if (byte === 0) return true
      if (byte < 9 || (byte > 13 && byte < 32)) nonPrintable++
    }
    return nonPrintable / buf.length > 0.3
  } catch {
    return false
  }
}

export function createReadTool(codebasePath: string) {
  return tool({
    description:
      "Read a file from the codebase, optionally with line range. Paths are relative to the codebase root.",
    parameters: z.object({
      filePath: z.string().describe("Path to the file (relative to codebase root, or absolute)"),
      offset: z.coerce.number().optional().describe("Line number to start reading from (0-based)"),
      limit: z.coerce.number().optional().describe("Number of lines to read (default 2000)"),
    }),
    execute: async ({ filePath, offset = 0, limit = DEFAULT_READ_LIMIT }) => {
      const resolved = path.isAbsolute(filePath) ? filePath : path.join(codebasePath, filePath)

      if (!fs.existsSync(resolved)) {
        const dir = path.dirname(resolved)
        const base = path.basename(resolved)
        const nearby = fs.existsSync(dir)
          ? fs
              .readdirSync(dir)
              .filter((e) => e.toLowerCase().includes(base.toLowerCase()) || base.toLowerCase().includes(e.toLowerCase()))
              .slice(0, 3)
          : []
        const suggestions =
          nearby.length > 0 ? `\nDid you mean:\n${nearby.map((e) => path.join(dir, e)).join("\n")}` : ""
        throw new Error(`File not found: ${resolved}${suggestions}`)
      }

      if (isBinaryByExtension(resolved) || isBinaryByContent(resolved)) {
        throw new Error(`Cannot read binary file: ${resolved}`)
      }

      const text = fs.readFileSync(resolved, "utf-8")
      const lines = text.split("\n")

      const raw: string[] = []
      let bytes = 0
      let truncatedByBytes = false
      const end = Math.min(lines.length, offset + limit)
      for (let i = offset; i < end; i++) {
        const rawLine = lines[i]!
        const line =
          rawLine.length > MAX_LINE_LENGTH ? rawLine.substring(0, MAX_LINE_LENGTH) + "..." : rawLine
        const size = Buffer.byteLength(line, "utf-8") + (raw.length > 0 ? 1 : 0)
        if (bytes + size > MAX_BYTES) {
          truncatedByBytes = true
          break
        }
        raw.push(line)
        bytes += size
      }

      const content = raw
        .map((line, index) => `${(index + offset + 1).toString().padStart(5, "0")}| ${line}`)
        .join("\n")

      const totalLines = lines.length
      const lastReadLine = offset + raw.length
      const hasMoreLines = totalLines > lastReadLine

      let footer: string
      if (truncatedByBytes) {
        footer = `\n\n(Output truncated at ${MAX_BYTES} bytes. Use 'offset' parameter to read beyond line ${lastReadLine})`
      } else if (hasMoreLines) {
        footer = `\n\n(File has more lines. Use 'offset' parameter to read beyond line ${lastReadLine})`
      } else {
        footer = `\n\n(End of file - total ${totalLines} lines)`
      }

      return `<file>\n${content}${footer}\n</file>`
    },
  })
}

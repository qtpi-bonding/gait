// Ported from OpenCode (MIT) — https://github.com/sst/opencode
import { tool } from "ai"
import { z } from "zod"
import fs from "fs"
import fsPromises from "fs/promises"
import path from "path"

// ─── Patch parser (ported from opencode src/patch/index.ts) ──────────────────

interface UpdateFileChunk {
  old_lines: string[]
  new_lines: string[]
  change_context?: string
  is_end_of_file?: boolean
}

type Hunk =
  | { type: "add"; path: string; contents: string }
  | { type: "delete"; path: string }
  | { type: "update"; path: string; move_path?: string; chunks: UpdateFileChunk[] }

function stripHeredoc(input: string): string {
  const heredocMatch = input.match(/^(?:cat\s+)?<<['"]?(\w+)['"]?\s*\n([\s\S]*?)\n\1\s*$/)
  return heredocMatch ? (heredocMatch[2] ?? input) : input
}

function parsePatchHeader(
  lines: string[],
  startIdx: number,
): { filePath: string; movePath?: string; nextIdx: number } | null {
  const line = lines[startIdx]
  if (!line) return null
  if (line.startsWith("*** Add File:")) {
    const filePath = line.split(":", 2)[1]?.trim()
    return filePath ? { filePath, nextIdx: startIdx + 1 } : null
  }
  if (line.startsWith("*** Delete File:")) {
    const filePath = line.split(":", 2)[1]?.trim()
    return filePath ? { filePath, nextIdx: startIdx + 1 } : null
  }
  if (line.startsWith("*** Update File:")) {
    const filePath = line.split(":", 2)[1]?.trim()
    if (!filePath) return null
    let movePath: string | undefined
    let nextIdx = startIdx + 1
    const nextLine = lines[nextIdx]
    if (nextLine && nextLine.startsWith("*** Move to:")) {
      movePath = nextLine.split(":", 2)[1]?.trim()
      nextIdx++
    }
    return { filePath, movePath, nextIdx }
  }
  return null
}

function parseAddFileContent(lines: string[], startIdx: number): { content: string; nextIdx: number } {
  let content = ""
  let i = startIdx
  while (i < lines.length) {
    const line = lines[i]!
    if (line.startsWith("***")) break
    if (line.startsWith("+")) content += line.substring(1) + "\n"
    i++
  }
  if (content.endsWith("\n")) content = content.slice(0, -1)
  return { content, nextIdx: i }
}

function parseUpdateFileChunks(
  lines: string[],
  startIdx: number,
): { chunks: UpdateFileChunk[]; nextIdx: number } {
  const chunks: UpdateFileChunk[] = []
  let i = startIdx
  while (i < lines.length) {
    const line = lines[i]!
    if (line.startsWith("***")) break
    if (line.startsWith("@@")) {
      const contextLine = line.substring(2).trim()
      i++
      const oldLines: string[] = []
      const newLines: string[] = []
      let isEndOfFile = false
      while (i < lines.length) {
        const changeLine = lines[i]!
        if (changeLine.startsWith("@@") || changeLine.startsWith("***")) break
        if (changeLine === "*** End of File") { isEndOfFile = true; i++; break }
        if (changeLine.startsWith(" ")) { oldLines.push(changeLine.substring(1)); newLines.push(changeLine.substring(1)) }
        else if (changeLine.startsWith("-")) { oldLines.push(changeLine.substring(1)) }
        else if (changeLine.startsWith("+")) { newLines.push(changeLine.substring(1)) }
        i++
      }
      chunks.push({ old_lines: oldLines, new_lines: newLines, change_context: contextLine || undefined, is_end_of_file: isEndOfFile || undefined })
    } else {
      i++
    }
  }
  return { chunks, nextIdx: i }
}

function parsePatch(patchText: string): { hunks: Hunk[] } {
  const cleaned = stripHeredoc(patchText.trim())
  const lines = cleaned.split("\n")
  const hunks: Hunk[] = []
  const beginIdx = lines.findIndex((l) => l.trim() === "*** Begin Patch")
  const endIdx = lines.findIndex((l) => l.trim() === "*** End Patch")
  if (beginIdx === -1 || endIdx === -1 || beginIdx >= endIdx) {
    throw new Error("Invalid patch format: missing *** Begin Patch / *** End Patch markers")
  }
  let i = beginIdx + 1
  while (i < endIdx) {
    const header = parsePatchHeader(lines, i)
    if (!header) { i++; continue }
    const currentLine = lines[i]!
    if (currentLine.startsWith("*** Add File:")) {
      const { content, nextIdx } = parseAddFileContent(lines, header.nextIdx)
      hunks.push({ type: "add", path: header.filePath, contents: content })
      i = nextIdx
    } else if (currentLine.startsWith("*** Delete File:")) {
      hunks.push({ type: "delete", path: header.filePath })
      i = header.nextIdx
    } else if (currentLine.startsWith("*** Update File:")) {
      const { chunks, nextIdx } = parseUpdateFileChunks(lines, header.nextIdx)
      hunks.push({ type: "update", path: header.filePath, move_path: header.movePath, chunks })
      i = nextIdx
    } else {
      i++
    }
  }
  return { hunks }
}

function normalizeUnicode(str: string): string {
  return str
    .replace(/[‘’‚‛]/g, "'")
    .replace(/[“”„‟]/g, '"')
    .replace(/[‐‑‒–—―]/g, "-")
    .replace(/…/g, "...")
    .replace(/ /g, " ")
}

type Comparator = (a: string, b: string) => boolean

function tryMatch(lines: string[], pattern: string[], startIndex: number, compare: Comparator, eof: boolean): number {
  if (eof) {
    const fromEnd = lines.length - pattern.length
    if (fromEnd >= startIndex) {
      let matches = true
      for (let j = 0; j < pattern.length; j++) {
        if (!compare(lines[fromEnd + j]!, pattern[j]!)) { matches = false; break }
      }
      if (matches) return fromEnd
    }
  }
  for (let i = startIndex; i <= lines.length - pattern.length; i++) {
    let matches = true
    for (let j = 0; j < pattern.length; j++) {
      if (!compare(lines[i + j]!, pattern[j]!)) { matches = false; break }
    }
    if (matches) return i
  }
  return -1
}

function seekSequence(lines: string[], pattern: string[], startIndex: number, eof = false): number {
  if (pattern.length === 0) return -1
  let r = tryMatch(lines, pattern, startIndex, (a, b) => a === b, eof)
  if (r !== -1) return r
  r = tryMatch(lines, pattern, startIndex, (a, b) => a.trimEnd() === b.trimEnd(), eof)
  if (r !== -1) return r
  r = tryMatch(lines, pattern, startIndex, (a, b) => a.trim() === b.trim(), eof)
  if (r !== -1) return r
  return tryMatch(lines, pattern, startIndex, (a, b) => normalizeUnicode(a.trim()) === normalizeUnicode(b.trim()), eof)
}

function deriveNewContents(filePath: string, chunks: UpdateFileChunk[]): string {
  const originalContent = fs.readFileSync(filePath, "utf-8")
  let originalLines = originalContent.split("\n")
  if (originalLines.length > 0 && originalLines[originalLines.length - 1] === "") originalLines.pop()

  const replacements: Array<[number, number, string[]]> = []
  let lineIndex = 0

  for (const chunk of chunks) {
    if (chunk.change_context) {
      const contextIdx = seekSequence(originalLines, [chunk.change_context], lineIndex)
      if (contextIdx === -1) throw new Error(`Failed to find context '${chunk.change_context}' in ${filePath}`)
      lineIndex = contextIdx + 1
    }
    if (chunk.old_lines.length === 0) {
      const insertionIdx = originalLines.length > 0 && originalLines[originalLines.length - 1] === ""
        ? originalLines.length - 1 : originalLines.length
      replacements.push([insertionIdx, 0, chunk.new_lines])
      continue
    }
    let pattern = chunk.old_lines
    let newSlice = chunk.new_lines
    let found = seekSequence(originalLines, pattern, lineIndex, chunk.is_end_of_file)
    if (found === -1 && pattern.length > 0 && pattern[pattern.length - 1] === "") {
      pattern = pattern.slice(0, -1)
      if (newSlice.length > 0 && newSlice[newSlice.length - 1] === "") newSlice = newSlice.slice(0, -1)
      found = seekSequence(originalLines, pattern, lineIndex, chunk.is_end_of_file)
    }
    if (found !== -1) {
      replacements.push([found, pattern.length, newSlice])
      lineIndex = found + pattern.length
    } else {
      throw new Error(`Failed to find expected lines in ${filePath}:\n${chunk.old_lines.join("\n")}`)
    }
  }

  replacements.sort((a, b) => a[0]! - b[0]!)
  const result = [...originalLines]
  for (let i = replacements.length - 1; i >= 0; i--) {
    const entry = replacements[i]!
    const [startIdx, oldLen, newSegment] = entry
    result.splice(startIdx, oldLen, ...newSegment)
  }
  if (result.length === 0 || result[result.length - 1] !== "") result.push("")
  return result.join("\n")
}

// ─── Tool factory ─────────────────────────────────────────────────────────────

export function createApplyPatchTool(codebasePath: string) {
  return tool({
    description:
      "Apply a structured patch to files in the codebase. Uses OpenCode-style patch format with *** Begin Patch / *** End Patch markers.",
    parameters: z.object({
      patchText: z.string().describe("The full patch text that describes all changes to be made"),
    }),
    execute: async ({ patchText }) => {
      if (!patchText) throw new Error("patchText is required")

      let hunks: Hunk[]
      try {
        const result = parsePatch(patchText)
        hunks = result.hunks
      } catch (error) {
        throw new Error(`apply_patch parse failed: ${error instanceof Error ? error.message : String(error)}`)
      }

      if (hunks.length === 0) {
        const normalized = patchText.replace(/\r\n/g, "\n").replace(/\r/g, "\n").trim()
        if (normalized === "*** Begin Patch\n*** End Patch") throw new Error("patch rejected: empty patch")
        throw new Error("apply_patch: no hunks found in patch")
      }

      const summaryLines: string[] = []

      for (const hunk of hunks) {
        const filePath = path.isAbsolute(hunk.path) ? hunk.path : path.resolve(codebasePath, hunk.path)

        switch (hunk.type) {
          case "add": {
            const newContent = hunk.contents.length === 0 || hunk.contents.endsWith("\n")
              ? hunk.contents : `${hunk.contents}\n`
            await fsPromises.mkdir(path.dirname(filePath), { recursive: true })
            await fsPromises.writeFile(filePath, newContent, "utf-8")
            summaryLines.push(`A ${path.relative(codebasePath, filePath)}`)
            break
          }

          case "update": {
            if (!fs.existsSync(filePath)) {
              throw new Error(`File not found for update: ${filePath}`)
            }
            const newContent = deriveNewContents(filePath, hunk.chunks)
            if (hunk.move_path) {
              const movePath = path.isAbsolute(hunk.move_path)
                ? hunk.move_path : path.resolve(codebasePath, hunk.move_path)
              await fsPromises.mkdir(path.dirname(movePath), { recursive: true })
              await fsPromises.writeFile(movePath, newContent, "utf-8")
              await fsPromises.unlink(filePath)
              summaryLines.push(`M ${path.relative(codebasePath, filePath)} -> ${path.relative(codebasePath, movePath)}`)
            } else {
              await fsPromises.writeFile(filePath, newContent, "utf-8")
              summaryLines.push(`M ${path.relative(codebasePath, filePath)}`)
            }
            break
          }

          case "delete": {
            await fsPromises.unlink(filePath)
            summaryLines.push(`D ${path.relative(codebasePath, filePath)}`)
            break
          }
        }
      }

      return `Patch applied successfully. Updated files:\n${summaryLines.join("\n")}`
    },
  })
}

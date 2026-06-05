// Ported from OpenCode (MIT) — https://github.com/sst/opencode
// Edit logic based on approaches from cline and gemini-cli (see opencode edit.ts for credits)
import { tool } from "ai"
import { z } from "zod"
import fs from "fs"
import path from "path"

// ─── Replacer infrastructure (ported from opencode) ──────────────────────────

type Replacer = (content: string, find: string) => Generator<string, void, unknown>

function levenshtein(a: string, b: string): number {
  if (a === "" || b === "") return Math.max(a.length, b.length)
  const matrix: number[][] = Array.from({ length: a.length + 1 }, (_, i) =>
    Array.from({ length: b.length + 1 }, (_, j) => (i === 0 ? j : j === 0 ? i : 0)),
  )
  for (let i = 1; i <= a.length; i++) {
    for (let j = 1; j <= b.length; j++) {
      const cost = a[i - 1] === b[j - 1] ? 0 : 1
      matrix[i]![j] = Math.min(matrix[i - 1]![j]! + 1, matrix[i]![j - 1]! + 1, matrix[i - 1]![j - 1]! + cost)
    }
  }
  return matrix[a.length]![b.length]!
}

const SimpleReplacer: Replacer = function* (_content, find) {
  yield find
}

const LineTrimmedReplacer: Replacer = function* (content, find) {
  const originalLines = content.split("\n")
  const searchLines = find.split("\n")
  if (searchLines[searchLines.length - 1] === "") searchLines.pop()

  for (let i = 0; i <= originalLines.length - searchLines.length; i++) {
    let matches = true
    for (let j = 0; j < searchLines.length; j++) {
      if (originalLines[i + j]!.trim() !== searchLines[j]!.trim()) {
        matches = false
        break
      }
    }
    if (matches) {
      let start = 0
      for (let k = 0; k < i; k++) start += originalLines[k]!.length + 1
      let end = start
      for (let k = 0; k < searchLines.length; k++) {
        end += originalLines[i + k]!.length
        if (k < searchLines.length - 1) end += 1
      }
      yield content.substring(start, end)
    }
  }
}

const BlockAnchorReplacer: Replacer = function* (content, find) {
  const SINGLE_THRESHOLD = 0.0
  const MULTI_THRESHOLD = 0.3
  const originalLines = content.split("\n")
  const searchLines = find.split("\n")
  if (searchLines.length < 3) return
  if (searchLines[searchLines.length - 1] === "") searchLines.pop()

  const firstLine = searchLines[0]!.trim()
  const lastLine = searchLines[searchLines.length - 1]!.trim()
  const searchBlockSize = searchLines.length

  const candidates: Array<{ startLine: number; endLine: number }> = []
  for (let i = 0; i < originalLines.length; i++) {
    if (originalLines[i]!.trim() !== firstLine) continue
    for (let j = i + 2; j < originalLines.length; j++) {
      if (originalLines[j]!.trim() === lastLine) {
        candidates.push({ startLine: i, endLine: j })
        break
      }
    }
  }
  if (candidates.length === 0) return

  function computeMatch(startLine: number, endLine: number): number {
    const actualBlockSize = endLine - startLine + 1
    const linesToCheck = Math.min(searchBlockSize - 2, actualBlockSize - 2)
    if (linesToCheck <= 0) return 1.0
    let similarity = 0
    for (let j = 1; j < searchBlockSize - 1 && j < actualBlockSize - 1; j++) {
      const orig = originalLines[startLine + j]!.trim()
      const srch = searchLines[j]!.trim()
      const maxLen = Math.max(orig.length, srch.length)
      if (maxLen === 0) continue
      similarity += (1 - levenshtein(orig, srch) / maxLen) / linesToCheck
    }
    return similarity
  }

  function yieldMatch(startLine: number, endLine: number) {
    let start = 0
    for (let k = 0; k < startLine; k++) start += originalLines[k]!.length + 1
    let end = start
    for (let k = startLine; k <= endLine; k++) {
      end += originalLines[k]!.length
      if (k < endLine) end += 1
    }
    return content.substring(start, end)
  }

  if (candidates.length === 1) {
    const { startLine, endLine } = candidates[0]!
    if (computeMatch(startLine, endLine) >= SINGLE_THRESHOLD) {
      yield yieldMatch(startLine, endLine)
    }
    return
  }

  let bestMatch: { startLine: number; endLine: number } | null = null
  let maxSimilarity = -1
  for (const candidate of candidates) {
    const sim = computeMatch(candidate.startLine, candidate.endLine)
    if (sim > maxSimilarity) {
      maxSimilarity = sim
      bestMatch = candidate
    }
  }
  if (maxSimilarity >= MULTI_THRESHOLD && bestMatch) {
    yield yieldMatch(bestMatch.startLine, bestMatch.endLine)
  }
}

const WhitespaceNormalizedReplacer: Replacer = function* (content, find) {
  const normalize = (t: string) => t.replace(/\s+/g, " ").trim()
  const normalizedFind = normalize(find)
  const lines = content.split("\n")
  for (const line of lines) {
    if (normalize(line) === normalizedFind) {
      yield line
    } else {
      const normalizedLine = normalize(line)
      if (normalizedLine.includes(normalizedFind)) {
        const words = find.trim().split(/\s+/)
        if (words.length > 0) {
          const pattern = words.map((w) => w.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")).join("\\s+")
          try {
            const match = line.match(new RegExp(pattern))
            if (match?.[0]) yield match[0]
          } catch {}
        }
      }
    }
  }
  const findLines = find.split("\n")
  if (findLines.length > 1) {
    for (let i = 0; i <= lines.length - findLines.length; i++) {
      const block = lines.slice(i, i + findLines.length)
      if (normalize(block.join("\n")) === normalizedFind) yield block.join("\n")
    }
  }
}

const IndentationFlexibleReplacer: Replacer = function* (content, find) {
  const removeIndent = (text: string) => {
    const ls = text.split("\n")
    const nonEmpty = ls.filter((l) => l.trim().length > 0)
    if (nonEmpty.length === 0) return text
    const min = Math.min(...nonEmpty.map((l) => (l.match(/^(\s*)/) || ["", ""])[1]!.length))
    return ls.map((l) => (l.trim().length === 0 ? l : l.slice(min))).join("\n")
  }
  const normalizedFind = removeIndent(find)
  const contentLines = content.split("\n")
  const findLines = find.split("\n")
  for (let i = 0; i <= contentLines.length - findLines.length; i++) {
    const block = contentLines.slice(i, i + findLines.length).join("\n")
    if (removeIndent(block) === normalizedFind) yield block
  }
}

const EscapeNormalizedReplacer: Replacer = function* (content, find) {
  const unescape = (str: string) =>
    str.replace(/\\(n|t|r|'|"|`|\\|\n|\$)/g, (_, c) => {
      switch (c) {
        case "n": return "\n"
        case "t": return "\t"
        case "r": return "\r"
        case "\n": return "\n"
        default: return c
      }
    })
  const unescapedFind = unescape(find)
  if (content.includes(unescapedFind)) yield unescapedFind
  const lines = content.split("\n")
  const findLines = unescapedFind.split("\n")
  for (let i = 0; i <= lines.length - findLines.length; i++) {
    const block = lines.slice(i, i + findLines.length).join("\n")
    if (unescape(block) === unescapedFind) yield block
  }
}

const TrimmedBoundaryReplacer: Replacer = function* (content, find) {
  const trimmedFind = find.trim()
  if (trimmedFind === find) return
  if (content.includes(trimmedFind)) yield trimmedFind
  const lines = content.split("\n")
  const findLines = find.split("\n")
  for (let i = 0; i <= lines.length - findLines.length; i++) {
    const block = lines.slice(i, i + findLines.length).join("\n")
    if (block.trim() === trimmedFind) yield block
  }
}

const MultiOccurrenceReplacer: Replacer = function* (content, find) {
  let start = 0
  while (true) {
    const index = content.indexOf(find, start)
    if (index === -1) break
    yield find
    start = index + find.length
  }
}

const ContextAwareReplacer: Replacer = function* (content, find) {
  const findLines = find.split("\n")
  if (findLines.length < 3) return
  if (findLines[findLines.length - 1] === "") findLines.pop()
  const contentLines = content.split("\n")
  const firstLine = findLines[0]!.trim()
  const lastLine = findLines[findLines.length - 1]!.trim()

  for (let i = 0; i < contentLines.length; i++) {
    if (contentLines[i]!.trim() !== firstLine) continue
    for (let j = i + 2; j < contentLines.length; j++) {
      if (contentLines[j]!.trim() === lastLine) {
        const blockLines = contentLines.slice(i, j + 1)
        if (blockLines.length === findLines.length) {
          let matching = 0
          let total = 0
          for (let k = 1; k < blockLines.length - 1; k++) {
            const bl = blockLines[k]!.trim()
            const fl = findLines[k]!.trim()
            if (bl.length > 0 || fl.length > 0) {
              total++
              if (bl === fl) matching++
            }
          }
          if (total === 0 || matching / total >= 0.5) {
            yield blockLines.join("\n")
          }
        }
        break
      }
    }
  }
}

export function replace(content: string, oldString: string, newString: string, replaceAll = false): string {
  if (oldString === newString) throw new Error("oldString and newString must be different")

  let notFound = true

  for (const replacer of [
    SimpleReplacer,
    LineTrimmedReplacer,
    BlockAnchorReplacer,
    WhitespaceNormalizedReplacer,
    IndentationFlexibleReplacer,
    EscapeNormalizedReplacer,
    TrimmedBoundaryReplacer,
    ContextAwareReplacer,
    MultiOccurrenceReplacer,
  ]) {
    for (const search of replacer(content, oldString)) {
      const index = content.indexOf(search)
      if (index === -1) continue
      notFound = false
      if (replaceAll) return content.replaceAll(search, newString)
      const lastIndex = content.lastIndexOf(search)
      if (index !== lastIndex) continue
      return content.substring(0, index) + newString + content.substring(index + search.length)
    }
  }

  if (notFound) throw new Error("oldString not found in content")
  throw new Error(
    "Found multiple matches for oldString. Provide more surrounding lines in oldString to identify the correct match.",
  )
}

// ─── Tool factory ─────────────────────────────────────────────────────────────

export function createEditTool(codebasePath: string) {
  return tool({
    description:
      "Edit a file by replacing one exact string with another. The oldString must uniquely identify the location to change.",
    parameters: z.object({
      filePath: z.string().describe("Path to the file to modify (relative to codebase root, or absolute)"),
      oldString: z.string().describe("The text to replace (must exist exactly once in the file)"),
      newString: z.string().describe("The replacement text"),
      replaceAll: z.boolean().optional().describe("Replace all occurrences of oldString (default false)"),
    }),
    execute: async ({ filePath, oldString, newString, replaceAll = false }) => {
      if (!filePath) throw new Error("filePath is required")
      if (oldString === newString) throw new Error("oldString and newString must be different")

      const resolved = path.isAbsolute(filePath) ? filePath : path.join(codebasePath, filePath)

      // Handle create-if-empty case
      if (oldString === "") {
        fs.mkdirSync(path.dirname(resolved), { recursive: true })
        fs.writeFileSync(resolved, newString, "utf-8")
        return `Created ${resolved} with ${Buffer.byteLength(newString, "utf-8")} bytes`
      }

      if (!fs.existsSync(resolved)) throw new Error(`File not found: ${resolved}`)

      const stat = fs.statSync(resolved)
      if (stat.isDirectory()) throw new Error(`Path is a directory, not a file: ${resolved}`)

      const contentOld = fs.readFileSync(resolved, "utf-8")
      const contentNew = replace(contentOld, oldString, newString, replaceAll)

      fs.writeFileSync(resolved, contentNew, "utf-8")

      const charsChanged = Math.abs(contentNew.length - contentOld.length)
      return `Edited ${resolved}: replaced ${charsChanged} chars`
    },
  })
}

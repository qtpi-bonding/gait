import { tool } from "ai"
import { z } from "zod"
import { execSync } from "child_process"
import fs from "fs"
import path from "path"
import { readEvents } from "../session/transcript"

export function createListAgentsTool(workspacePath: string) {
  return tool({
    description: "List active agent sessions not yet done, sorted by last activity descending.",
    parameters: z.object({}),
    execute: async () => {
      let branchOut: string
      try {
        branchOut = execSync("git branch --list 'session/*'", { cwd: workspacePath, encoding: "utf-8" }).trim()
      } catch {
        return []
      }
      if (!branchOut) return []

      const ids = branchOut.split("\n")
        .map(l => l.replace(/^[*+ ]+/, "").trim())
        .filter(l => l.startsWith("session/"))
        .map(l => l.slice("session/".length))
        .filter(Boolean)

      const results = []
      for (const sessionId of ids) {
        const txPath = path.join(workspacePath, "sessions", sessionId, "transcript.jsonl")
        if (!fs.existsSync(txPath)) continue
        const events = readEvents(txPath)
        if (!events.length) continue
        const lastEvt = events[events.length - 1]!
        const ts = lastEvt.ts
        let status: string
        switch (lastEvt.type) {
          case "turn_complete": status = "done"; break
          case "error":         status = "error"; break
          case "clarify":       status = "clarify"; break
          case "inform":        status = "inform"; break
          case "permissions":   status = "permissions"; break
          default:              status = "running"; break
        }
        let summary = ""
        for (let i = events.length - 1; i >= 0; i--) {
          const e = events[i]!
          if (e.type === "assistant_text") { summary = e.content.slice(0, 100); break }
        }
        results.push({ session_id: sessionId, last_active_ts: ts, status, summary })
      }
      return results.filter(r => r.status !== "done").sort((a, b) => b.last_active_ts - a.last_active_ts)
    },
  })
}

import { tool } from "ai"
import { z } from "zod"
import crypto from "crypto"
import fs from "fs"
import path from "path"
import { createSessionWorktree, commitSession, getCodebaseHash } from "../git"
import { initSession, appendEvent } from "../session/transcript"
import { loadConfig } from "../config"

export function createSpawnAgentTool(currentSessionId: string, workspacePath: string) {
  return tool({
    description: "Spawn a new agent session for a subtask. Returns the session ID immediately.",
    parameters: z.object({
      task: z.string(),
      context_files: z.array(z.string()).optional(),
      agent: z.string().optional(),
      system_prompt: z.string().optional(),
    }),
    execute: async ({ task, context_files: _context_files, agent, system_prompt }) => {
      const newId = "s" + crypto.randomUUID().slice(0, 8)
      createSessionWorktree(workspacePath, newId)
      const sessionDir = path.join(workspacePath, "sessions", newId)
      const txPath = path.join(sessionDir, "transcript.jsonl")
      const sjPath = path.join(sessionDir, "session.json")
      const sj: any = {
        id: newId,
        assigned_by: currentSessionId,
        created_at: Date.now(),
        token_totals: { input: 0, output: 0, cache_read: 0, cache_write: 0 },
      }
      if (agent) sj.agent_name = agent
      if (system_prompt) sj.system_prompt = system_prompt
      fs.writeFileSync(sjPath, JSON.stringify(sj, null, 2), "utf-8")
      const config = loadConfig(workspacePath)
      const codebaseHash = getCodebaseHash(path.join(workspacePath, "codebase"))
      initSession({ transcriptPath: txPath, id: newId, model: config.model, codebaseHash, codebaseDirty: false })
      appendEvent(txPath, { type: "user_turn", content: task, ts: Date.now() })
      commitSession(workspacePath, newId, "session(" + newId + "): spawn from " + currentSessionId)
      return { session_id: newId }
    },
  })
}

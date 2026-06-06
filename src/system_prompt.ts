import fs from "fs"
import path from "path"

interface SystemPromptOpts {
  sessionId: string
  workspacePath: string
  codebasePath: string
  agentPrompt?: string
}

interface TurnContextOpts {
  codebaseHash: string
  dirtyFiles: string[]
  timestamp: number
}

export function buildSystemPrompt(opts: SystemPromptOpts): string {
  const { sessionId, workspacePath, codebasePath, agentPrompt } = opts

  const gaitBlock = `<gait>
session: ${sessionId}
workspace: ${workspacePath}
codebase: ${codebasePath}
</gait>`

  const agentsPath = path.join(workspacePath, "AGENTS.md")
  const agentsSection = fs.existsSync(agentsPath)
    ? `\n\n<agents>\n${fs.readFileSync(agentsPath, "utf-8").trim()}\n</agents>`
    : ""

  const agentSection = agentPrompt ? `\n\n<agent-instructions>\n${agentPrompt}\n</agent-instructions>` : ""

  return `${gaitBlock}${agentsSection}${agentSection}`
}

export function buildTurnContext(opts: TurnContextOpts): string {
  const { codebaseHash, dirtyFiles, timestamp } = opts

  const gitLine = dirtyFiles.length === 0
    ? `git: ${codebaseHash} | clean`
    : `git: ${codebaseHash} | dirty\n${dirtyFiles.map(f => `  ${f.trim()}`).join("\n")}`

  return `<gait-state>\ntimestamp: ${timestamp}\n${gitLine}\n</gait-state>`
}

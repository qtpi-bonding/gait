import fs from "fs"
import path from "path"

export function loadAgentSystemPrompt(workspacePath: string, agentName: string): string {
  const agentPath = path.join(workspacePath, "agents", `${agentName}.md`)
  if (!fs.existsSync(agentPath)) {
    throw new Error(`Agent "${agentName}" not found. Expected: ${agentPath}`)
  }
  return fs.readFileSync(agentPath, "utf-8").trim()
}

export function resolveAgentPrompt(
  workspacePath: string,
  opts: {
    systemPrompt?: string
    agentName?: string
    defaultAgent?: string
  }
): string | undefined {
  if (opts.systemPrompt) return opts.systemPrompt
  const name = opts.agentName ?? opts.defaultAgent
  if (!name) return undefined
  return loadAgentSystemPrompt(workspacePath, name)
}

import fs from "fs"
import path from "path"
import { checkGitVersion, initWorkspace, addSubmodule, initialCommit } from "../../git"

const DEFAULT_CONFIG = `{
  // gait configuration
  "model": "anthropic/claude-sonnet-4-6",
  // Allow list: tools that run without approval. Default empty = everything gated.
  "allow": [],
  // Tier 1 config-defined tools: shell commands the LLM can call by name
  "tools": []
}
`

export async function initCommand(argv: string[]): Promise<void> {
  const [repoUrl] = argv
  const workspacePath = process.cwd()

  checkGitVersion()
  initWorkspace(workspacePath)

  fs.mkdirSync(path.join(workspacePath, "sessions"), { recursive: true })

  const gitignorePath = path.join(workspacePath, ".gitignore")
  const gitignoreContent = "sessions/\n.gait-ws.lock\n"
  if (!fs.existsSync(gitignorePath)) {
    fs.writeFileSync(gitignorePath, gitignoreContent, "utf-8")
  } else {
    const existing = fs.readFileSync(gitignorePath, "utf-8")
    if (!existing.includes("sessions/")) {
      fs.appendFileSync(gitignorePath, "\n" + gitignoreContent)
    }
  }

  const configPath = path.join(workspacePath, "gait.jsonc")
  if (!fs.existsSync(configPath)) {
    fs.writeFileSync(configPath, DEFAULT_CONFIG, "utf-8")
  }

  const agentsPath = path.join(workspacePath, "AGENTS.md")
  if (!fs.existsSync(agentsPath)) {
    const agentsContent = [
      "# gait agent",
      "",
      "You are a coding agent running inside gait, a stateless turn-based agent framework.",
      "",
      "**How it works:**",
      "- Each `gait run` invocation is one turn. Your full conversation history is replayed from a transcript each turn.",
      "- The `<gait>` block in your context shows your session ID, workspace, and codebase path. A `<gait-state>` block shows the current timestamp and codebase git status.",
      "- The codebase is the external repo you work on. The workspace holds gait config and session history — keep the codebase clean.",
      "",
      "**Tools:**",
      "- `read`, `write`, `edit`, `multiedit`, `apply_patch`, `glob`, `grep`, `ls`, `codesearch`, `shell`, `webfetch` — work on the codebase",
      "- Tools not in the allow list require user approval before running",
      "- `questions` — ask the user clarifying questions; pauses the turn until they answer on the next invocation",
      "- `spawn_subagent` — delegate a sub-task to a child session",
      "",
      "**Turn model:**",
      "- Complete your task in as few turns as possible",
      "- Read files rather than asking when you need information",
      "- Use `questions` only when blocked on a decision only the user can make",
    ].join("\n") + "\n"
    fs.writeFileSync(agentsPath, agentsContent, "utf-8")
  }

  if (repoUrl) {
    addSubmodule(workspacePath, repoUrl)
    initialCommit(workspacePath)
    console.log(`\ngait workspace initialized with codebase submodule at ${repoUrl}`)
    console.log(`\nStart an agent turn:`)
    console.log(`  gait run --task "..."`)
  } else {
    initialCommit(workspacePath)
    console.log(`\ngait workspace initialized`)
    console.log(`\nStart an agent turn:`)
    console.log(`  gait run --task "..."`)
    console.log(`\nTo point at a codebase:`)
    console.log(`  gait run --codebase /path/to/repo --task "..."`)
    console.log(`\nEdit AGENTS.md to add project-specific instructions for the agent.`)
  }
}
